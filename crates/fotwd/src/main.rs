//! `fotwd` — the FlyOnTheWall daemon.
//!
//! Today it records one meeting and exits. The loopback HTTP/WS server and
//! the AppKit shell land on top of this same session machinery.
//!
//! # The key never touches disk or argv
//!
//! It is read from `DEEPGRAM_API_KEY` and used in place. Passing it as a
//! command-line argument would put it in the process argument vector, where
//! any same-user process can read it; writing it to a config file is what
//! `fotw-secrets` and the OS keychain exist to prevent.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use fotw_audio::{AudioPlatform, DeviceId, FormatRequest, SystemScope, platform};
use fotw_stt::{DeepgramStreamConfig, Source, deepgram::DeepgramConfig};
use fotwd::session::{self, Transcription};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("record") => {
            let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            let root = args.get(2).map_or_else(default_root, PathBuf::from);
            record(root, secs).await
        }
        _ => {
            eprintln!("usage: fotwd record [seconds] [dir]");
            eprintln!();
            eprintln!("  Set DEEPGRAM_API_KEY to transcribe as well as record.");
            eprintln!("  Without it the meeting is still recorded and can be");
            eprintln!("  transcribed later from the audio on disk.");
            ExitCode::FAILURE
        }
    }
}

/// A per-run session id, used to stamp every transcript segment.
fn session_id() -> String {
    format!(
        "fotwd-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    )
}

fn default_root() -> PathBuf {
    std::env::var("HOME")
        .map(|h| {
            PathBuf::from(h).join("Library/Application Support/com.flyonthewall.fotw/sessions")
        })
        .unwrap_or_else(|_| std::env::temp_dir().join("fotw-sessions"))
}

async fn record(root: PathBuf, seconds: u64) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!("fotwd: cannot create {}: {e}", root.display());
        return ExitCode::FAILURE;
    }

    let plat = platform::host();
    let system = match plat.open_system(SystemScope::DefaultOutputMix, FormatRequest::any()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fotwd: could not open the system tap: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The mic is optional on purpose: a machine with no input device should
    // still record the far end rather than refuse to start.
    let mic = plat
        .open_mic(&DeviceId::new("default"), FormatRequest::any())
        .ok();

    let transcription = match std::env::var("DEEPGRAM_API_KEY") {
        Ok(key) if !key.trim().is_empty() => {
            println!("  transcribe : Deepgram nova-3 (streaming)");
            // Only the system leg is transcribed. The mic leg needs its own
            // connection and doubles the bill; that is the explicit
            // "two cloud streams" decision in spec 7.5, not a default.
            Transcription::Deepgram(Box::new(DeepgramStreamConfig::new(
                key,
                DeepgramConfig::new(session_id(), Source::System),
            )))
        }
        _ => {
            println!("  transcribe : off (set DEEPGRAM_API_KEY to enable)");
            Transcription::Disabled
        }
    };

    println!("  recording  : {seconds}s");
    println!("  sessions   : {}", root.display());
    println!();

    match session::run(
        &root,
        system,
        mic,
        transcription,
        Duration::from_secs(seconds),
    )
    .await
    {
        Ok(outcome) => {
            println!("  system     : {} samples", outcome.system_samples);
            println!("  mic        : {} samples", outcome.mic_samples);
            println!(
                "  buffers    : {} ({} silent)",
                outcome.total_buffers, outcome.silent_buffers
            );

            if !outcome.segments.is_empty() {
                if let Err(e) = session::append_segments(&outcome.dir, &outcome.segments) {
                    eprintln!("  ! could not append the transcript: {e}");
                }
                println!("  segments   : {}", outcome.segments.len());
                println!();
                println!("  ── transcript ─────────────────────────────");
                for line in textwrap(&outcome.transcript_text(), 68) {
                    println!("  {line}");
                }
                println!("  ───────────────────────────────────────────");
            }

            println!();
            println!("  ✓ {}", outcome.dir.display());

            if !outcome.captured_audio() {
                println!();
                println!("  ! every buffer was digitally silent.");
                println!("    Either nothing was playing, or the system-audio");
                println!("    permission was denied — macOS reports a denial as");
                println!("    silence, not as an error. Grant it under System");
                println!("    Settings > Privacy & Security > Screen & System");
                println!("    Audio Recording, then:");
                println!("      tccutil reset AudioCapture com.flyonthewall.fotw");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("fotwd: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Minimal greedy wrap. Not worth a dependency.
fn textwrap(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push("(no speech detected)".into());
    }
    out
}
