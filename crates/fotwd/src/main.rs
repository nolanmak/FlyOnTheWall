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
use fotw_shell::StartOrigin;
use fotw_store::{ArchiveOptions, Db};
use fotw_stt::{DeepgramStreamConfig, Source, deepgram::DeepgramConfig};
use fotw_summarize::template::{TemplateSet, default_templates_dir};
use fotwd::audit::{AuditKind, AuditLog};
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
        Some("onboard") => onboard(),
        Some("detect") => {
            let secs = args
                .get(1)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(120);
            detect(secs)
        }
        Some("key") => key_command(&args[1..]),
        Some("recover") => {
            let pos = positionals(&args[1..], &[]);
            let root = pos.first().map_or_else(default_root, PathBuf::from);
            recover_command(data_root_of(&root), args.iter().any(|a| a == "--check"))
        }
        Some("summarize") => {
            let positional: Vec<&String> =
                args[1..].iter().filter(|a| !a.starts_with("--")).collect();
            let Some(id) = positional.first().map(|s| (*s).clone()) else {
                eprintln!("usage: fotwd summarize <meeting-id> [dir] [--template <slug>]");
                return ExitCode::FAILURE;
            };
            let root = positional
                .get(1)
                .map_or_else(default_root, |s| PathBuf::from(s.as_str()));
            let slug = flag_value(&args, "--template");
            summarize_command(root, id, slug).await
        }
        Some("list") => {
            let root = args.get(1).map_or_else(default_root, PathBuf::from);
            list(root)
        }
        Some("templates") => templates_command(&args[1..]),
        Some("export") => export_command(&args[1..]),
        Some("export-all") => export_all_command(&args[1..]),
        Some("import") => import_command(&args[1..]),
        _ => {
            eprintln!(
                "usage: fotwd <serve [dir] | record [seconds] [dir] [--i-have-consent] | \
                 list [dir] | summarize <id> [dir] [--template <slug>] | disclose | \
                 onboard | detect [seconds] | \
                 key <set <provider>|list> | recover [dir] [--check] | \
                 templates <list|install|show <slug>> | \
                 export <id> [dir] [--format md|txt|json] [--out <file>] | \
                 export-all <dest> [dir] [--audio] [--resume] [--yes-plaintext] | \
                 import <dest> [dir]>"
            );
            eprintln!();
            eprintln!("  Set DEEPGRAM_API_KEY to transcribe as well as record.");
            eprintln!("  Without it the meeting is still recorded and can be");
            eprintln!("  transcribed later from the audio on disk.");
            ExitCode::FAILURE
        }
    }
}

/// The value after a `--flag`, if it is present and has one.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let at = args.iter().position(|a| a == flag)?;
    args.get(at + 1)
        .filter(|v| !v.starts_with("--"))
        .map(ToOwned::to_owned)
}

/// Positional arguments, `--flags` and their values removed.
fn positionals<'a>(args: &'a [String], value_flags: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a.starts_with("--") {
            skip = value_flags.contains(&a.as_str());
            continue;
        }
        out.push(a.as_str());
    }
    out
}

/// The app data root: `<sessions>/..`, matching [`fotwd::open_library`].
fn data_root_of(sessions: &std::path::Path) -> PathBuf {
    sessions.parent().unwrap_or(sessions).to_path_buf()
}

/// `fotwd templates <list|install|show <slug>>`.
///
/// Templates are files (SUM-08), so this command exists to point at the
/// directory rather than to manage anything: everything it does can also be
/// done with `ls`, `$EDITOR` and `git`, which is the entire argument for files
/// over an in-app editor.
fn templates_command(args: &[String]) -> ExitCode {
    let dir = default_templates_dir();
    match args.first().map(String::as_str) {
        Some("install") => match TemplateSet::install_builtins(&dir) {
            Ok(written) => {
                println!("  templates  : {}", dir.display());
                if written.is_empty() {
                    println!("  (all six were already there — nothing was overwritten)");
                } else {
                    for p in &written {
                        println!(
                            "  + {}",
                            p.file_name().unwrap_or_default().to_string_lossy()
                        );
                    }
                }
                println!();
                println!("  These are yours now. Edit them, keep them in git, share them.");
                println!("  `install` never overwrites a file you have edited; delete one to");
                println!("  get ours back.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("fotwd: cannot write {}: {e}", dir.display());
                ExitCode::FAILURE
            }
        },
        Some("show") => {
            let Some(slug) = args.get(1) else {
                eprintln!("usage: fotwd templates show <slug>");
                return ExitCode::FAILURE;
            };
            match TemplateSet::load_or_builtin(&dir) {
                Ok(set) => match set.get(slug) {
                    Some(t) => {
                        println!("{}", t.prompt_body());
                        ExitCode::SUCCESS
                    }
                    None => {
                        eprintln!("fotwd: no template `{slug}`");
                        ExitCode::FAILURE
                    }
                },
                Err(e) => {
                    eprintln!("fotwd: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        None | Some("list") => match TemplateSet::load_or_builtin(&dir) {
            Ok(set) => {
                println!("  templates  : {}", dir.display());
                if !dir.exists() {
                    println!("  (not installed yet — showing the built-in defaults)");
                }
                println!();
                for t in set.iter() {
                    println!("  {:<16} {}", t.slug, t.description);
                    if !t.default_for.is_empty() {
                        println!("  {:<16} default for: {}", "", t.default_for.join(", "));
                    }
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                // Located, and never a silent fallback: the whole point.
                eprintln!("fotwd: {e}");
                ExitCode::FAILURE
            }
        },
        Some(other) => {
            eprintln!("fotwd: unknown templates command `{other}`");
            eprintln!("usage: fotwd templates <list|install|show <slug>>");
            ExitCode::FAILURE
        }
    }
}

/// `fotwd export <meeting-id> [dir] [--format md|txt|json] [--out <file>]`.
fn export_command(args: &[String]) -> ExitCode {
    let pos = positionals(args, &["--format", "--out"]);
    let Some(id) = pos.first() else {
        eprintln!("usage: fotwd export <meeting-id> [dir] [--format md|txt|json] [--out <file>]");
        return ExitCode::FAILURE;
    };
    let root = pos.get(1).map_or_else(default_root, PathBuf::from);
    let format = flag_value(args, "--format").unwrap_or_else(|| "md".to_owned());

    let db = match open_db(&root) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fotwd: {e}");
            return ExitCode::FAILURE;
        }
    };
    let doc = match db.export_meeting(id) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fotwd: {e}");
            return ExitCode::FAILURE;
        }
    };

    let rendered = match format.as_str() {
        "md" | "markdown" => doc.to_markdown(),
        "txt" | "text" => doc.to_plain_text(),
        "json" => doc.to_json_pretty(),
        other => {
            eprintln!("fotwd: unknown format `{other}` (md, txt or json)");
            return ExitCode::FAILURE;
        }
    };

    match flag_value(args, "--out") {
        Some(path) => {
            if let Err(e) = std::fs::write(&path, &rendered) {
                eprintln!("fotwd: cannot write {path}: {e}");
                return ExitCode::FAILURE;
            }
            // Said every time a file is written, not only for bulk archives:
            // one meeting in the clear is still a meeting in the clear.
            println!(
                "  wrote {} ({} bytes, PLAIN TEXT — not encrypted)",
                path,
                rendered.len()
            );
        }
        None => print!("{rendered}"),
    }
    ExitCode::SUCCESS
}

/// `fotwd export-all <dest> [dir] [--audio] [--resume] [--yes-plaintext]`.
///
/// The acknowledgement is not a formality. The library is SQLCipher-encrypted
/// and this writes every meeting the user has ever recorded into an ordinary
/// directory in plain text; somebody who has not registered that has just
/// undone the encryption without meaning to.
fn export_all_command(args: &[String]) -> ExitCode {
    let pos = positionals(args, &[]);
    let Some(dest) = pos.first().map(PathBuf::from) else {
        eprintln!("usage: fotwd export-all <dest> [dir] [--audio] [--resume] [--yes-plaintext]");
        return ExitCode::FAILURE;
    };
    let root = pos.get(1).map_or_else(default_root, PathBuf::from);
    let data_root = data_root_of(&root);
    let include_audio = args.iter().any(|a| a == "--audio");

    let db = match open_db(&root) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fotwd: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Projected size first, before anything is written (issue #37).
    let projection = match db.projected_archive_size(&data_root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fotwd: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("  meetings   : {}", projection.meetings);
    if include_audio {
        println!(
            "  audio      : {} files, about {:.1} GB",
            projection.audio_files,
            projection.audio_bytes as f64 / 1_073_741_824.0
        );
    } else {
        println!(
            "  audio      : not included ({} files, about {:.1} GB — pass --audio to add it)",
            projection.audio_files,
            projection.audio_bytes as f64 / 1_073_741_824.0
        );
    }
    println!("  destination: {}", dest.display());
    println!();
    println!("  ⚠  THIS ARCHIVE WILL NOT BE ENCRYPTED.");
    println!("     Your library is stored encrypted. This is not: every transcript,");
    println!("     note and summary will be readable by anyone who can read these");
    println!("     files. Deleting a meeting in FlyOnTheWall will not reach into it.");
    if include_audio {
        println!("     The audio will be written decrypted too.");
    }
    println!();

    if !args.iter().any(|a| a == "--yes-plaintext") {
        println!("  Re-run with --yes-plaintext to confirm you want this.");
        return ExitCode::FAILURE;
    }

    let opts = ArchiveOptions {
        include_audio,
        data_root: Some(data_root),
        templates_dir: Some(default_templates_dir()),
        resume: args.iter().any(|a| a == "--resume"),
    };

    let mut last = 0usize;
    match db.export_library(&dest, &opts, &mut |p| {
        if p.done != last {
            last = p.done;
            println!("  [{}/{}] {}", p.done, p.total, p.meeting_id);
        }
    }) {
        Ok(report) => {
            println!();
            println!(
                "  written    : {} ({} skipped)",
                report.written, report.skipped
            );
            if report.audio_files > 0 {
                println!(
                    "  audio      : {} files, {} bytes",
                    report.audio_files, report.audio_bytes
                );
            }
            for missing in &report.missing_media {
                println!("  ! recorded but not on disk: {missing}");
            }
            println!("  ✓ {}", dest.display());
            println!("    Read README.txt in there before you copy it anywhere.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("fotwd: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `fotwd import <archive> [dir]`.
fn import_command(args: &[String]) -> ExitCode {
    let pos = positionals(args, &[]);
    let Some(src) = pos.first().map(PathBuf::from) else {
        eprintln!("usage: fotwd import <archive> [dir]");
        return ExitCode::FAILURE;
    };
    let root = pos.get(1).map_or_else(default_root, PathBuf::from);
    let data_root = data_root_of(&root);

    let mut db = match open_db(&root) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("fotwd: {e}");
            return ExitCode::FAILURE;
        }
    };

    match db.import_library_with(&src, Some(&default_templates_dir()), Some(&data_root)) {
        Ok(report) => {
            println!("  meetings   : {}", report.meetings);
            println!("  inserted   : {} rows", report.rows_inserted);
            println!("  already in : {} rows", report.rows_already_present);
            if report.templates_restored > 0 {
                println!("  templates  : {} restored", report.templates_restored);
            }
            if report.media_restored > 0 {
                println!("  media      : {} files restored", report.media_restored);
            }
            // Never silent: a conflict is the one outcome where the archive
            // did not fully win, and the user is the only one who can decide.
            for c in &report.conflicts {
                println!("  ! {}/{}: {}", c.table, c.id, c.detail);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("fotwd: {e}");
            eprintln!("  Nothing was imported — the library is unchanged.");
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

    // CON-01's acceptance criterion is about the audit log, not the UI: no
    // audio buffer reaches disk without a user-initiated Start recorded here
    // first. Written *before* the tap opens, so a crash during capture leaves
    // the record of who asked and what they were warned about.
    let audit = AuditLog::at(&root);
    if let Err(e) = audit.record(AuditKind::SessionStart {
        origin: StartOrigin::Cli.label().to_owned(),
        detected_app: None,
        jurisdiction_warning: escalation.user_text(),
        acknowledged_all_party: escalation.blocks() && acknowledged,
    }) {
        // A recording we cannot account for is not one we should make.
        eprintln!(
            "fotwd: could not write the audit log at {}: {e}",
            audit.path().display()
        );
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
            if let Err(e) = audit.record(AuditKind::SessionEnd {
                session: outcome.dir.display().to_string(),
                duration_ms: seconds * 1_000,
            }) {
                eprintln!("  ! could not write the audit log: {e}");
            }
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

/// `fotwd onboard` — walk the grants, verifying each by using it (issue #31).
///
/// Nothing here asks the system for a permission state it cannot honestly
/// report. The system-audio grant has no query API at all, and a denial
/// arrives as silence, so the only truthful test is a round trip: start a tap,
/// play a tone through the default output from another process, count what
/// came back.
///
/// The step that surprises people is the last one. On a developer's machine
/// this usually *passes* — and it passes for reasons that will not hold for a
/// user, because an unbundled binary launched from a terminal inherits the
/// terminal's grant. That case is reported as loudly as a failure.
fn onboard() -> ExitCode {
    use fotwd::onboard::{Environment, HostProbe, PROBE_WINDOW, Report, Step, interpret};

    println!("FlyOnTheWall onboarding");
    println!();
    println!("  What is about to happen:");
    println!("    1. a one-second recording of your microphone");
    println!("    2. a test tone, and a one-second recording of your system audio");
    println!();
    println!("  Neither is written to disk. macOS may show a permission prompt;");
    println!("  the system-audio one appears only when the tap starts, and there");
    println!("  is no API to ask for it in advance.");
    println!();

    let env = Environment::detect();
    println!(
        "  bundle     : {}",
        if env.in_app_bundle { "yes" } else { "no" }
    );
    println!("  signature  : {:?}", env.signature);
    println!(
        "  launched by: {}",
        env.terminal.as_deref().unwrap_or("not a terminal")
    );
    println!();

    // The mic leg is different, and issue #31 says to handle it up front: its
    // authorization status is a real, queryable API. We still round-trip it
    // afterwards, because a status of "authorized" and a device that delivers
    // nothing are different failures.
    let plat = platform::host();
    let mic_state = plat.permission(fotw_audio::Permission::Microphone);
    println!("  microphone authorization: {mic_state:?}");
    if mic_state.is_blocking() {
        println!("    → System Settings → Privacy & Security → Microphone");
    }
    println!(
        "  system audio authorization: {:?} (no API exists; probing instead)",
        plat.permission(fotw_audio::Permission::SystemAudio)
    );
    println!();

    let probe = HostProbe;
    let mut report = Report::default();

    println!("  recording the microphone for {PROBE_WINDOW:?}…");
    let mic = probe.microphone(PROBE_WINDOW);
    report.push(Step::Microphone, interpret(Step::Microphone, &mic, &env));

    println!("  playing a tone and recording system audio for {PROBE_WINDOW:?}…");
    let system = probe.system_audio(PROBE_WINDOW);
    report.push(
        Step::SystemAudio,
        interpret(Step::SystemAudio, &system, &env),
    );

    println!();
    print!("{}", report.render());

    if report.ready() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// `fotwd detect` — watch meeting detection decide, and record nothing.
///
/// This command **cannot** start a recording, by construction: it holds a
/// [`Detector`](fotwd::detect::Detector) and prints its output. Arming is all
/// detection is allowed to do (CON-01), and this is the surface that makes
/// that visible on a real machine — join a call and watch it arm; leave Zoom
/// idling in the background and watch it not.
fn detect(seconds: u64) -> ExitCode {
    use fotw_audio::activity::ActivityProbe;
    use fotw_shell::{Monotonic, ShellCore, ShellEffect, ShellInput};
    use fotwd::detect::{Detection, Detector, DetectorConfig, NoCalendar};

    let plat = platform::host();
    let mut detector = Detector::new(DetectorConfig::default());
    // The real shell state machine, so what is printed is what the menu bar
    // would show -- including the assertion that nothing here ever produces
    // a StartCapture.
    let mut shell = ShellCore::new();

    println!("FlyOnTheWall detect — {seconds}s, recording nothing");
    println!("  conjunction: a known conferencing app AND that app holding the mic");
    println!("  (or a calendar match, once EventKit lands — MTG-01)");
    println!();

    let started = std::time::Instant::now();
    let mut last = String::new();
    while started.elapsed() < Duration::from_secs(seconds) {
        let now = Monotonic::from_duration(started.elapsed());
        let snapshot = plat.snapshot();
        let reason;
        let arg = match &snapshot {
            Ok(s) => Ok(s),
            Err(e) => {
                reason = e.to_string();
                Err(reason.as_str())
            }
        };

        let effects = match detector.poll(now, arg, &NoCalendar) {
            Detection::Arm(meeting) => {
                println!(
                    "  [{:>4}s] ARMED — {}",
                    now.as_duration().as_secs(),
                    meeting.headline()
                );
                println!("          evidence: {}", meeting.evidence);
                for line in meeting.consent_notice.lines() {
                    println!("          {line}");
                }
                println!("          → the user must now press Start. Nothing is recording.");
                shell.handle(ShellInput::MeetingDetected { at: now, meeting })
            }
            Detection::Clear => {
                println!("  [{:>4}s] cleared", now.as_duration().as_secs());
                shell.handle(ShellInput::DetectionCleared)
            }
            Detection::Idle | Detection::Hold => Vec::new(),
        };

        // The invariant, asserted at runtime on a real machine and not only in
        // the test suite.
        assert!(
            !effects.contains(&ShellEffect::StartCapture),
            "CON-01 violated: detection commanded capture"
        );

        if let Ok(s) = &snapshot {
            let line = summarise(s);
            if line != last {
                println!("  [{:>4}s] {line}", now.as_duration().as_secs());
                last = line;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    let stats = detector.stats();
    println!();
    println!(
        "  armed {} time(s), started {} — conversion {:.0}%",
        stats.armed,
        stats.started,
        stats.conversion() * 100.0
    );
    println!("  (local only: these counters are never sent anywhere)");
    ExitCode::SUCCESS
}

/// One line describing what the machine is doing, for `fotwd detect`.
fn summarise(snapshot: &fotw_audio::ActivitySnapshot) -> String {
    let holders: Vec<String> = snapshot
        .input_holders()
        .map(|c| {
            c.bundle_id
                .clone()
                .unwrap_or_else(|| format!("pid {}", c.pid))
        })
        .collect();
    let input = snapshot.default_input.as_ref().map_or_else(
        || "no input device".to_owned(),
        |d| {
            format!(
                "{} [{:?}] running={}",
                d.name, d.transport, d.running_somewhere
            )
        },
    );
    format!(
        "{} clients · input {input} · mic held by [{}]",
        snapshot.clients.len(),
        holders.join(", ")
    )
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

/// `fotwd recover [dir] [--check]` — open a library with the Recovery Key.
///
/// The command for the day the keychain is gone. It reads the key from
/// **stdin**, never from `argv`: an argument is readable by any same-user
/// process through `ps`, and a Recovery Key typed onto a command line also
/// lands in the shell history file — where it would outlive the emergency it
/// was typed for. Same reasoning as `fotwd key set`.
///
/// `--check` verifies a written-down key and changes nothing: no keychain
/// write, no rewrite of the sealed file. It is what a careful user runs a month
/// later to find out whether the card in the drawer is still good, and it has
/// to be safe to run at any time or nobody will run it.
fn recover_command(data_root: PathBuf, check_only: bool) -> ExitCode {
    use fotwd::recovery;

    let store = if check_only {
        None
    } else {
        match secrets::keystore() {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("fotwd: no OS keychain available: {e}");
                eprintln!("  The library can still be checked with --check.");
                return ExitCode::FAILURE;
            }
        }
    };

    println!("  library    : {}", data_root.join("db.sqlite3").display());
    println!();
    eprint!("  Recovery Key (fotw1-…): ");
    use std::io::Write;
    let _ = std::io::stderr().flush();
    let mut typed = String::new();
    if std::io::stdin().read_line(&mut typed).is_err() || typed.trim().is_empty() {
        eprintln!("fotwd: no Recovery Key was entered");
        return ExitCode::FAILURE;
    }

    if check_only {
        return match recovery::check(&data_root, &typed) {
            Ok(()) => {
                println!();
                println!("  ✓ that Recovery Key opens this library.");
                println!("    Nothing was changed: the keychain and the recovery file are");
                println!("    exactly as they were.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!();
                eprintln!("fotwd: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let store = store.expect("checked above");
    match recovery::recover(&data_root, &store, &typed) {
        Ok(mut outcome) => {
            let count = outcome
                .db
                .meetings()
                .list(1, 0)
                .map(|m| m.len())
                .unwrap_or(0);
            println!();
            println!("  ✓ the library is open.");
            if count == 0 {
                println!("    (it contains no meetings yet)");
            }
            if outcome.key_restored_to_keychain {
                println!("    The master key has been put back in your keychain, so the next");
                println!("    run is an ordinary one and will not ask for anything.");
            }
            println!();
            println!("  Keep the Recovery Key. It is still the only backup, and this");
            println!("  keychain can be lost again.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!();
            eprintln!("fotwd: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `fotwd summarize <meeting-id>` — turn a stored transcript into a document.
///
/// Runs entirely off the library. The transcript is immutable and the summary
/// is a derived, versioned artifact, so this costs zero STT spend and can be
/// re-run with a different model or template without touching the audio.
async fn summarize_command(root: PathBuf, meeting_id: String, slug: Option<String>) -> ExitCode {
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

    // A malformed template file is fatal here, never a silent fallback to the
    // default (issue #36). A user whose template did not apply and was not
    // told will believe it did.
    let set = match TemplateSet::load_or_builtin(default_templates_dir()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fotwd: {e}");
            eprintln!("  The summary was NOT generated. Fix the template and re-run.");
            return ExitCode::FAILURE;
        }
    };
    let title = db
        .meetings()
        .get(&meeting_id)
        .map(|m| m.title)
        .unwrap_or_default();
    let template = match &slug {
        Some(s) => match set.get(s) {
            Some(t) => t.clone(),
            None => {
                eprintln!(
                    "fotwd: no template `{s}` in {}",
                    default_templates_dir().display()
                );
                eprintln!(
                    "  available: {}",
                    set.iter()
                        .map(|t| t.slug.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return ExitCode::FAILURE;
            }
        },
        // The divergence from Granola worth shipping: a template can claim a
        // calendar-event-title pattern and become the default for it.
        None => match set.for_event_title(&title) {
            Some(t) => t.clone(),
            None => {
                eprintln!("fotwd: no templates are installed — run `fotwd templates install`");
                return ExitCode::FAILURE;
            }
        },
    };
    println!("  template   : {} ({})", template.name, template.slug);

    match fotwd::summarize::summarize_meeting(&mut db, &store, &meeting_id, &template).await {
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
