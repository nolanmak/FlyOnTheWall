//! `fotw` — the CLI. Recovery tool, and the primary test surface.
//!
//! `doctor` matters more than it looks. There is **no public API** to query
//! the macOS system-audio permission, and a denial delivers silence that is
//! indistinguishable from a quiet room. A round-trip capture is the only
//! truthful answer available, so that is what this does.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fotw_audio::{
    AudioPlatform, CaptureTimestamp, FormatRequest, FrameFlags, FrameSink, SystemScope, TapError,
    platform,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("doctor") => doctor(),
        Some(other) => {
            eprintln!("fotw: unknown command `{other}`");
            eprintln!("usage: fotw doctor");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("usage: fotw doctor");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Default)]
struct Stats {
    callbacks: AtomicUsize,
    samples: AtomicU64,
    nonzero: AtomicU64,
    silent_buffers: AtomicUsize,
    errors: AtomicUsize,
}

struct ProbeSink(Arc<Stats>);

impl FrameSink for ProbeSink {
    fn on_frames(&mut self, pcm: &[f32], _ts: CaptureTimestamp, flags: FrameFlags) {
        self.0.callbacks.fetch_add(1, Ordering::Relaxed);
        self.0
            .samples
            .fetch_add(pcm.len() as u64, Ordering::Relaxed);
        let nz = pcm.iter().filter(|s| **s != 0.0).count() as u64;
        self.0.nonzero.fetch_add(nz, Ordering::Relaxed);
        if flags.contains(FrameFlags::SILENT) {
            self.0.silent_buffers.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn on_error(&mut self, _e: TapError) {
        self.0.errors.fetch_add(1, Ordering::Relaxed);
    }
}

fn doctor() -> ExitCode {
    println!("FlyOnTheWall doctor");
    println!("  os            : {}", os_version());
    println!(
        "  bundled       : {}",
        if in_bundle() {
            "yes"
        } else {
            "NO — see below"
        }
    );

    let plat = platform::host();
    let caps = plat.caps();
    println!("  system mix    : {}", caps.system_mix);
    println!("  needs consent : {}", caps.needs_consent_for_system);
    println!(
        "  system audio  : {:?} (no API exists to query this — probing instead)",
        plat.permission(fotw_audio::Permission::SystemAudio)
    );

    let mut tap = match plat.open_system(SystemScope::DefaultOutputMix, FormatRequest::any()) {
        Ok(t) => t,
        Err(e) => {
            println!("\n  ✗ could not create the tap: {e}");
            return ExitCode::FAILURE;
        }
    };

    let stats = Arc::new(Stats::default());
    let format = match tap.start(Box::new(ProbeSink(Arc::clone(&stats)))) {
        Ok(f) => f,
        Err(e) => {
            println!("\n  ✗ could not start the tap: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("  tap format    : {format}");

    let probe = Duration::from_secs(3);
    println!(
        "\n  capturing for {}s — play some audio now…",
        probe.as_secs()
    );
    let began = Instant::now();
    while began.elapsed() < probe {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = tap.stop();

    let callbacks = stats.callbacks.load(Ordering::Relaxed);
    let samples = stats.samples.load(Ordering::Relaxed);
    let nonzero = stats.nonzero.load(Ordering::Relaxed);
    let silent = stats.silent_buffers.load(Ordering::Relaxed);
    let bytes_per_sec = (samples as f64 * 4.0) / probe.as_secs_f64();
    let theoretical = f64::from(format.sample_rate_hz) * f64::from(format.channels) * 4.0;

    println!("\n  callbacks     : {callbacks}");
    println!("  samples       : {samples}");
    println!(
        "  throughput    : {bytes_per_sec:.0} B/s vs {theoretical:.0} B/s theoretical ({:.1}%)",
        bytes_per_sec / theoretical * 100.0
    );
    println!(
        "  non-zero      : {nonzero} ({:.1}%)",
        nonzero as f64 / samples.max(1) as f64 * 100.0
    );
    println!("  silent buffers: {silent}/{callbacks}");

    println!();
    if callbacks == 0 {
        println!("  ✗ NO CALLBACKS. The IOProc never fired. Either the tap was not");
        println!("    registered, or permission was denied — the two are");
        println!("    indistinguishable from here.");
        println!("    Try: tccutil reset AudioCapture com.flyonthewall.fotw");
        return ExitCode::FAILURE;
    }
    if nonzero == 0 {
        println!("  ✗ CALLBACKS BUT ONLY SILENCE.");
        println!("    Three causes, in order of likelihood:");
        println!("      1. nothing was playing — re-run with audio");
        println!("      2. permission denied (a denial delivers silence, not an error)");
        println!("      3. the macOS 26 all-zero-buffer defect (CAP-05)");
        println!("    Grant: System Settings → Privacy & Security →");
        println!("           Screen & System Audio Recording");
        return ExitCode::FAILURE;
    }
    println!(
        "  ✓ CAPTURING REAL AUDIO — {nonzero} non-zero samples in {}s",
        probe.as_secs()
    );
    ExitCode::SUCCESS
}

fn in_bundle() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.ends_with("Contents/MacOS")))
        .unwrap_or(false)
}

fn os_version() -> String {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}
