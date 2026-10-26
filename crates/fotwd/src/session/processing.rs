//! Actual recording-pump adapter; originals are synced before every admission.
use super::{LegChannel, PcmFeed};
use crate::live_aec::{ConversionChain, Coordinator, Prepared};
use fotw_audio::{StreamFormat, clock::Clock};
use fotw_pipeline::{
    aec::{BypassReason, Output, OutputKind},
    reference_resample::{AlignmentEpoch, Config, NativeFrame, ProcessingFrame, TapGeneration},
    resample::ConversionOrigin,
    ring::CaptureObservation,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) struct NativeContext<'a> {
    pub(super) generation: u64,
    pub(super) format: StreamFormat,
    pub(super) phase: ConversionOrigin,
    pub(super) accepted_start: u64,
    pub(super) observation: Option<CaptureObservation>,
    pub(super) samples: &'a [f32],
}
#[derive(Clone, Copy)]
struct Shape {
    generation: u64,
    format: StreamFormat,
    phase: ConversionOrigin,
    archive_start: u64,
}
impl Shape {
    fn initial(format: StreamFormat) -> Self {
        Self {
            generation: 1,
            format,
            phase: ConversionOrigin {
                input_frame_numerator: 0,
                denominator: 1,
            },
            archive_start: 0,
        }
    }
}
pub(super) struct PumpAec {
    owner: Coordinator,
    sink: Arc<dyn PcmFeed>,
    clock: Arc<dyn Clock>,
    render: Shape,
    mic: Shape,
    mic_archive: StreamFormat,
    canonical_phase: ConversionOrigin,
    anchor: u64,
    next: u64,
    output: Vec<Output>,
    failed: bool,
    unknown: bool,
    deadline_ms: u64,
    warnings: Vec<String>,
}
impl PumpAec {
    #[allow(clippy::too_many_arguments)] // capture/archive geometry and source admission
    pub(super) fn bind(
        prepared: Prepared,
        sink: Arc<dyn PcmFeed>,
        system: StreamFormat,
        mic: StreamFormat,
        canonical_phase: ConversionOrigin,
        anchor: u64,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, String> {
        if !sink.processing_declared() {
            return Err("microphone feed did not declare its selected processor".into());
        }
        let render = Shape::initial(system);
        let shape = Shape::initial(mic);
        let config = Self::config(render, shape, mic, canonical_phase, anchor, 1)?;
        let deadline_ms = prepared.deadline_ms();
        let owner = prepared.bind(config, (system.channels, mic.channels), anchor)?;
        Ok(Self {
            owner,
            sink,
            clock,
            render,
            mic: shape,
            mic_archive: mic,
            canonical_phase,
            anchor,
            next: anchor,
            output: Vec::new(),
            failed: false,
            unknown: false,
            deadline_ms,
            warnings: Vec::new(),
        })
    }
    fn config(
        render: Shape,
        mic: Shape,
        archive: StreamFormat,
        canonical_phase: ConversionOrigin,
        anchor: u64,
        epoch: u64,
    ) -> Result<Config, String> {
        let domain = ConversionChain {
            native_rate_hz: mic.format.sample_rate_hz,
            archive_rate_hz: archive.sample_rate_hz,
            archive_phase: mic.phase,
            canonical_phase,
            archive_start_frame: mic.archive_start,
            canonical_anchor: ProcessingFrame(anchor),
            native_start_frame: NativeFrame(0),
        }
        .domain()?;
        Ok(Config {
            render_rate_hz: render.format.sample_rate_hz,
            capacity_frames: render.format.sample_rate_hz as usize * 2,
            mic: domain,
            render_generation: TapGeneration(render.generation),
            mic_generation: TapGeneration(mic.generation),
            epoch: AlignmentEpoch(epoch),
        })
    }
    pub(super) fn observe(&mut self, channel: LegChannel, observation: Option<CaptureObservation>) {
        if self.failed {
            return;
        }
        if let Some(observation) = observation {
            let now = self.clock.now_ns();
            let result = match channel {
                LegChannel::System => self
                    .owner
                    .observe_render(observation, now, &mut self.output),
                LegChannel::Mic => self.owner.observe_mic(observation, now, &mut self.output),
            };
            if let Err(error) = result {
                self.fail(error);
            }
            self.publish();
        }
    }
    pub(super) fn capture(
        &mut self,
        channel: LegChannel,
        native: Option<NativeContext<'_>>,
        archive_before: u64,
    ) {
        if self.failed {
            return;
        }
        let Some(native) = native else {
            if !self.unknown {
                if let Err(error) = self.owner.reset_timing(&mut self.output) {
                    self.fail(error);
                }
                self.unknown = true;
                self.publish();
            }
            return;
        };
        self.unknown = false;
        let old = match channel {
            LegChannel::System => self.render,
            LegChannel::Mic => self.mic,
        };
        if old.generation != native.generation
            || old.format != native.format
            || old.phase != native.phase
        {
            let archive_channels = if channel == LegChannel::Mic {
                self.mic_archive.channels
            } else {
                1
            };
            let replacement = Shape {
                generation: native.generation,
                format: native.format,
                phase: native.phase,
                archive_start: archive_before / u64::from(archive_channels),
            };
            let (render, mic) = if channel == LegChannel::System {
                (replacement, self.mic)
            } else {
                (self.render, replacement)
            };
            let result = self
                .owner
                .route_epoch()
                .checked_add(1)
                .ok_or_else(|| "AEC route epoch overflow".to_owned())
                .and_then(|epoch| {
                    Self::config(
                        render,
                        mic,
                        self.mic_archive,
                        self.canonical_phase,
                        self.anchor,
                        epoch,
                    )
                })
                .and_then(|config| {
                    self.owner.reconfigure(
                        config,
                        (render.format.channels, mic.format.channels),
                        &mut self.output,
                    )
                });
            if let Err(error) = result {
                self.fail(error);
                self.publish();
                return;
            }
            self.render = render;
            self.mic = mic;
        }
        self.observe(channel, native.observation);
        if channel == LegChannel::System && !self.failed {
            if let Err(error) = self.owner.append_render(
                native.accepted_start,
                native.samples,
                self.clock.now_ns(),
                &mut self.output,
            ) {
                self.fail(error);
            }
            self.publish();
        }
    }
    pub(super) fn write_mic(&mut self, pcm: &[f32]) {
        let start = self.next;
        let Some(end) = start.checked_add(pcm.len() as u64) else {
            self.fail("microphone processing clock overflow".into());
            self.publish();
            return;
        };
        let accepted = if self.failed {
            start
        } else {
            if let Err(error) =
                self.owner
                    .push_mic(start, pcm, self.clock.now_ns(), &mut self.output)
            {
                self.fail(error);
            }
            self.owner.next_sample()
        };
        if self.failed && (start..=end).contains(&accepted) {
            for (i, chunk) in pcm[(accepted - start) as usize..].chunks(3200).enumerate() {
                self.output.push(Output {
                    epoch: self.owner.route_epoch(),
                    start_sample: accepted + i as u64 * 3200,
                    samples: chunk.to_vec(),
                    kind: OutputKind::Raw(BypassReason::BackendFailed),
                });
            }
        }
        self.next = end;
        self.publish();
    }
    pub(super) fn poll(&mut self) {
        if !self.failed
            && let Err(error) = self.owner.poll(self.clock.now_ns(), &mut self.output)
        {
            self.fail(error);
        }
        self.publish();
    }
    pub(super) fn finish(&mut self) {
        if !self.failed {
            if let Err(error) = self.owner.finish(self.clock.now_ns(), &mut self.output) {
                self.fail(error);
            }
            let until = Instant::now() + Duration::from_millis(self.deadline_ms + 20);
            while !self.failed && !self.owner.is_drained() && Instant::now() < until {
                self.poll();
                std::thread::sleep(Duration::from_millis(1));
            }
            if !self.owner.is_drained() {
                self.owner.cancel(&mut self.output);
            }
        }
        self.publish();
    }
    fn fail(&mut self, error: String) {
        if !self.failed {
            self.warnings.push(format!(
                "audio processing unavailable; original microphone input retained: {error}"
            ));
            self.owner.cancel(&mut self.output);
            self.failed = true;
        }
    }
    fn publish(&mut self) {
        for output in self.output.drain(..) {
            self.sink.write_processed(&output);
        }
    }
    pub(super) fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }
}
