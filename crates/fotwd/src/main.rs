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
use fotw_secrets::{KeyStore, Provider};
use fotw_store::Db;
use fotw_stt::{DeepgramStreamConfig, Source, deepgram::DeepgramConfig};
use fotwd::consent::{DisclosureKit, JurisdictionSignals, Rules};
use fotwd::persist;
use fotwd::secrets;
use fotwd::session::{self, Transcription};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("record") => {
            let acknowledged = args.iter().any(|a| a == "--i-have-consent");
            let positional: Vec<&String> =
                args[1..].iter().filter(|a| !a.starts_with("--")).collect();
            let secs: u64 = positional
                .first()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10);
            let root = positional
                .get(1)
                .map_or_else(default_root, |s| PathBuf::from(s.as_str()));
            record(root, secs, acknowledged).await
        }
        Some("serve") => {
            let root = args
                .iter()
                .skip(1)
                .find(|a| !a.starts_with("--"))
                .map_or_else(default_root, |s| PathBuf::from(s.as_str()));
            if let Err(e) = std::fs::create_dir_all(&root) {
                eprintln!("fotwd: cannot create {}: {e}", root.display());
                return ExitCode::FAILURE;
            }
            let launch = if args.iter().any(|a| a == "--print-url") {
                fotwd::serve::Launch::PrintUrl
            } else if args.iter().any(|a| a == "--no-open") {
                fotwd::serve::Launch::Nothing
            } else {
                fotwd::serve::Launch::OpenBrowser
            };
            match fotwd::serve::serve(root, launch).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("fotwd: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("disclose") => {
            let kit = DisclosureKit::default();
            println!("── paste into the meeting chat ──");
            println!("{}", kit.chat_notice());
            println!();
            println!("── paste into a calendar invite ──");
            println!("{}", kit.calendar_blurb());
            println!();
            println!("── say out loud ──");
            println!("{}", kit.verbal_script());
            ExitCode::SUCCESS
        }
        Some("key") => key_command(&args[1..]),
        Some("summarize") => {
            let Some(id) = args.get(1).cloned() else {
                eprintln!("usage: fotwd summarize <meeting-id> [dir]");
                return ExitCode::FAILURE;
            };
            let root = args.get(2).map_or_else(default_root, PathBuf::from);
            summarize_command(root, id).await
        }
        Some("list") => {
            let root = args.get(1).map_or_else(default_root, PathBuf::from);
            list(root)
        }
        _ => {
            eprintln!(
                "usage: fotwd <serve [dir] | record [seconds] [dir] [--i-have-consent] | \
                 list [dir] | summarize <id> [dir] | disclose | \
                 key <set <provider>|list>>"
            );
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

async fn record(root: PathBuf, seconds: u64, acknowledged: bool) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!("fotwd: cannot create {}: {e}", root.display());
        return ExitCode::FAILURE;
    }

    // CON-05: the consent gate runs BEFORE anything is captured. Detection
    // arms; the user starts. Recording first and asking afterwards is the
    // conduct pleaded against Granola.
    let home = std::env::var("FOTW_JURISDICTION").unwrap_or_else(|_| "US-CA".to_owned());
    let escalation = Rules::builtin().escalate(&JurisdictionSignals::home(&home));
    println!("{}", escalation.user_text());
    if escalation.blocks() && !acknowledged {
        println!();
        println!("  Recording is blocked until you confirm every participant has");
        println!("  consented. Re-run with --i-have-consent, and use the disclosure");
        println!("  text from `fotwd disclose` to actually tell them.");
        println!();
        println!("  (set FOTW_JURISDICTION to your own jurisdiction; it defaults to");
        println!("   US-CA, the strictest common case, rather than guessing)");
        return ExitCode::FAILURE;
    }
    println!();

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

    let key_store = secrets::keystore().ok();
    let found = key_store
        .as_ref()
        .and_then(|s| secrets::deepgram_key(s as &dyn KeyStore));
    let transcription = match found {
        Some((secret, origin)) => {
            match origin {
                secrets::Origin::Environment => println!(
                    "  transcribe : Deepgram nova-3 (key from DEEPGRAM_API_KEY — \
                     readable by any child process; prefer `fotwd key set deepgram`)"
                ),
                _ => println!("  transcribe : Deepgram nova-3 (key from keychain)"),
            }
            let key = secret.expose().to_owned();
            // Only the system leg is transcribed. The mic leg needs its own
            // connection and doubles the bill; that is the explicit
            // "two cloud streams" decision in spec 7.5, not a default.
            Transcription::Deepgram(Box::new(DeepgramStreamConfig::new(
                key,
                DeepgramConfig::new(session_id(), Source::System),
            )))
        }
        None => {
            println!("  transcribe : off (run `fotwd key set deepgram` to enable)");
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

            // Persist AFTER the WAL is finalized, never instead of it. The
            // database is an index over the session directory: if this fails
            // the meeting is still on disk and can be imported again.
            match open_db(&root).and_then(|mut db| {
                persist::persist_session(&mut db, &outcome, &default_title())
                    .map_err(|e| format!("{e}"))
            }) {
                Ok(id) => println!("  meeting    : {id}"),
                Err(e) => eprintln!("  ! could not add to the library: {e}"),
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

/// Open the library beside the sessions, keyed from the OS keychain.
fn open_db(root: &std::path::Path) -> Result<Db, String> {
    fotwd::open_library(root)
}

fn default_title() -> String {
    format!(
        "Untitled recording — {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    )
}

fn list(root: PathBuf) -> ExitCode {
    let mut db = match open_db(&root) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fotwd: {e}");
            return ExitCode::FAILURE;
        }
    };
    let meetings = match db.meetings().list(50, 0) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("fotwd: {e}");
            return ExitCode::FAILURE;
        }
    };
    if meetings.is_empty() {
        println!("  (no meetings yet)");
        return ExitCode::SUCCESS;
    }
    println!("  {:<38}  {:<9}  {:>7}  title", "id", "state", "secs");
    for m in &meetings {
        let secs = m.duration_ms.unwrap_or(0) / 1000;
        println!("  {:<38}  {:<9}  {secs:>7}  {}", m.id, m.state, m.title);
    }
    ExitCode::SUCCESS
}

/// `fotwd key set <provider>` — read a key from stdin into the keychain.
///
/// Stdin, never an argument: argv is readable by any same-user process, and a
/// key pasted onto a command line also lands in the shell history file.
fn key_command(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("set") => {
            let Some(slug) = args.get(1) else {
                eprintln!("usage: fotwd key set <deepgram|elevenlabs|openai|anthropic>");
                return ExitCode::FAILURE;
            };
            let Some(provider) = Provider::from_slug(slug) else {
                eprintln!("fotwd: unknown provider `{slug}`");
                return ExitCode::FAILURE;
            };
            let store = match secrets::keystore() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("fotwd: no OS keychain available: {e}");
                    return ExitCode::FAILURE;
                }
            };
            eprint!(
                "paste the {} key (input is not echoed to the log): ",
                provider.display_name()
            );
            use std::io::Write;
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                eprintln!("fotwd: could not read the key");
                return ExitCode::FAILURE;
            }
            match secrets::store_key(&store, provider, &line) {
                Ok(()) => {
                    println!("stored {} in the keychain", provider.display_name());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("fotwd: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("list") => {
            let store = match secrets::keystore() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("fotwd: no OS keychain available: {e}");
                    return ExitCode::FAILURE;
                }
            };
            for p in Provider::ALL {
                let present = store
                    .contains(fotw_secrets::SecretKey::ApiKey(p))
                    .unwrap_or(false);
                println!(
                    "  {:<12} {}",
                    p.slug(),
                    if present { "configured" } else { "—" }
                );
            }
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: fotwd key <set <provider> | list>");
            ExitCode::FAILURE
        }
    }
}

/// `fotwd summarize <meeting-id>` — turn a stored transcript into a document.
///
/// Runs entirely off the library. The transcript is immutable and the summary
/// is a derived, versioned artifact, so this costs zero STT spend and can be
/// re-run with a different model or template without touching the audio.
async fn summarize_command(root: PathBuf, meeting_id: String) -> ExitCode {
    let store = match secrets::keystore() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fotwd: no OS keychain available: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut db = match open_db(&root) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fotwd: {e}");
            return ExitCode::FAILURE;
        }
    };

    match fotwd::summarize::summarize_meeting(&mut db, &store, &meeting_id, "").await {
        Ok(out) => {
            println!("  version    : v{}", out.version);
            println!("  grounding  : {:.0}%", out.coverage * 100.0);
            println!("  items kept : {}", out.kept_items);
            if out.dropped_items > 0 {
                // Surfaced, never silent: the user should know the model
                // proposed items whose evidence did not check out.
                println!(
                    "  dropped    : {} (evidence did not check out)",
                    out.dropped_items
                );
            }
            println!();
            for line in out.markdown.lines() {
                println!("  {line}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("fotwd: {e}");
            ExitCode::FAILURE
        }
    }
}
