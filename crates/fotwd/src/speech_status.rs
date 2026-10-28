//! Session-scoped transcription telemetry without stream, model or file ownership.
use crate::failover::stream::{Phase, StatusReader};
use fotw_stt::{Source, StreamEvent, StreamState};
use fotw_web::recorder::{CaptionStatus, CaptureSource, SpeechSourceStatus, SpeechState};
use std::sync::{Arc, Mutex};

enum Reader {
    Legacy(SpeechSourceStatus),
    Durable(StatusReader),
}
#[derive(Default)]
struct Inner {
    /// Live-caption state per leg (#245), when captions run.
    captions: Vec<(CaptureSource, crate::captions::CaptionState)>,
    readers: Vec<(CaptureSource, Reader)>,
    processing: Option<fotw_pipeline::aec_worker::Status>,
}
/// One capture's status. Durable readers retain only metadata and atomics.
#[derive(Clone, Default)]
pub struct SpeechMonitor(Arc<Mutex<Inner>>);
impl std::fmt::Debug for SpeechMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpeechMonitor(<redacted>)")
    }
}
fn capture_source(source: Source) -> CaptureSource {
    match source {
        Source::System => CaptureSource::System,
        Source::Mic => CaptureSource::Mic,
    }
}
fn durable(source: CaptureSource, reader: &StatusReader) -> SpeechSourceStatus {
    let status = reader.read();
    let words = status
        .epoch
        .engine
        .capabilities
        .word_timestamps
        .in_streaming();
    let speakers = status.epoch.engine.capabilities.diarization.in_streaming();
    SpeechSourceStatus {
        source,
        provider: status.epoch.engine.provider,
        model: status.epoch.engine.model,
        revision: status.epoch.engine.revision,
        state: match status.phase {
            Phase::Cloud => SpeechState::Cloud,
            Phase::Local => SpeechState::Local,
            Phase::Complete => SpeechState::Complete,
            Phase::Incomplete | Phase::PrefixRecovered => SpeechState::Incomplete,
        },
        epoch: Some(status.epoch.number),
        from_ms: Some(status.epoch.from_sample / 16),
        switch_reason: status
            .epoch
            .reason
            .and_then(|reason| serde_json::to_value(reason).ok())
            .and_then(|v| v.as_str().map(str::to_owned)),
        pending_audio_ms: Some(status.lag_samples / 16),
        word_timestamps_supported: Some(words),
        diarization_supported: Some(speakers),
        worker_running: Some(status.worker_running),
        error: status.error,
        captions: None,
    }
}
impl SpeechMonitor {
    /// Bounded metadata reads, with no IO, credential lookup or native work.
    pub fn snapshot(&self) -> Vec<SpeechSourceStatus> {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .readers
            .iter()
            .map(|(source, read)| {
                let mut status = match read {
                    Reader::Legacy(status) => status.clone(),
                    Reader::Durable(reader) => durable(*source, reader),
                };
                status.captions = inner.captions.iter().find(|(s, _)| s == source).map(
                    |(_, state)| match state {
                        crate::captions::CaptionState::Live => CaptionStatus::Live,
                        crate::captions::CaptionState::Paused => CaptionStatus::Paused,
                    },
                );
                status
            })
            .collect()
    }

    /// Record a leg's live-caption state (#245).
    pub(crate) fn set_captions(&self, source: Source, state: crate::captions::CaptionState) {
        let source = capture_source(source);
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.captions.retain(|(s, _)| *s != source);
        inner.captions.push((source, state));
    }
    /// Known native work still outstanding after capture's drain ended.
    pub fn has_retained_native_worker(&self) -> bool {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .processing
            .as_ref()
            .is_some_and(|status| status.is_running())
            || inner.readers.iter().any(|(_, reader)| match reader {
                Reader::Durable(reader) => reader.read().worker_running,
                Reader::Legacy(_) => false,
            })
    }
    #[cfg(feature = "experimental-aec")]
    pub(crate) fn bind_processing(&self, status: fotw_pipeline::aec_worker::Status) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).processing = Some(status);
    }
    fn register(&self, source: Source, reader: Reader) {
        let source = capture_source(source);
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.readers.retain(|(s, _)| *s != source);
        inner.readers.push((source, reader));
    }
    pub(crate) fn bind_durable(&self, source: Source, reader: StatusReader) {
        self.register(source, Reader::Durable(reader));
    }
    pub(crate) fn bind_legacy(
        &self,
        source: Source,
        provider: &str,
        model: &str,
        words: bool,
        speakers: bool,
    ) {
        self.register(
            source,
            Reader::Legacy(SpeechSourceStatus {
                source: capture_source(source),
                provider: provider.into(),
                model: model.into(),
                revision: None,
                state: SpeechState::Connecting,
                epoch: None,
                from_ms: None,
                switch_reason: None,
                pending_audio_ms: None,
                word_timestamps_supported: Some(words),
                diarization_supported: Some(speakers),
                worker_running: None,
                error: None,
                captions: None,
            }),
        );
    }
    pub(crate) fn observe_legacy(&self, source: Source, event: &StreamEvent) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some((_, Reader::Legacy(status))) = inner
            .readers
            .iter_mut()
            .find(|(s, _)| *s == capture_source(source))
        else {
            return;
        };
        match event {
            StreamEvent::State(state) => {
                status.state = match state {
                    StreamState::Connecting => SpeechState::Connecting,
                    StreamState::Open => SpeechState::Open,
                    StreamState::Reconnecting => SpeechState::Reconnecting,
                    StreamState::Closed => SpeechState::Closed,
                }
            }
            StreamEvent::Error(error) => status.error = Some(error.message.clone()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_lifecycle_keeps_unknown_coverage_and_actual_model() {
        let monitor = SpeechMonitor::default();
        monitor.bind_legacy(Source::System, "deepgram", "configured-model", true, true);
        monitor.observe_legacy(
            Source::System,
            &StreamEvent::State(StreamState::Reconnecting),
        );
        let status = monitor.snapshot().pop().unwrap();
        assert_eq!(status.state, SpeechState::Reconnecting);
        assert_eq!(status.model, "configured-model");
        assert_eq!(status.pending_audio_ms, None);
        assert_eq!(status.worker_running, None);
        monitor.observe_legacy(Source::System, &StreamEvent::State(StreamState::Closed));
        assert_eq!(monitor.snapshot()[0].state, SpeechState::Closed);
        assert!(!monitor.has_retained_native_worker());
    }
    #[test]
    fn source_updates_and_old_monitors_cannot_change_another_capture() {
        let old = SpeechMonitor::default();
        let fresh = SpeechMonitor::default();
        old.bind_legacy(Source::System, "deepgram", "nova-3", true, true);
        old.bind_legacy(
            Source::Mic,
            fotw_stt::local::PROVIDER,
            "small.en",
            false,
            false,
        );
        old.observe_legacy(
            Source::System,
            &StreamEvent::State(StreamState::Reconnecting),
        );
        let status = old.snapshot();
        assert_eq!(status.len(), 2);
        assert_eq!(status[1].source, CaptureSource::Mic);
        assert_eq!(status[1].state, SpeechState::Connecting);
        assert!(fresh.snapshot().is_empty());
    }
}
