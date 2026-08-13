//! Put a real meeting-detection prompt on screen and let a person click it.
//!
//! ```sh
//! cargo run -p fotw-shell --example prompt_preview
//! ```
//!
//! Every other route to this panel needs a real meeting: a conferencing app
//! holding the microphone, a TCC grant, a signed bundle and twenty seconds of
//! dwell. So the consent surface — the one thing CON-01 says has to be in
//! front of a human — was reviewable only by reading its renderer. That is how
//! the panel came to not exist at all for four milestones (issue #52) with the
//! whole suite green.
//!
//! This runs the **real** [`ShellRuntime`] against the **real** AppKit shell.
//! The only fake is the detector: the host reports a meeting once, a few
//! seconds after launch, exactly as `fotwd::detect::Detector` would.
//!
//! What to check, which is `crates/fotw-shell/QA.md` §6b:
//!
//! - the prompt appears at the top right, above a full-screen call;
//! - **Start is greyed out** until the all-party box is ticked (CON-05);
//! - clicking anywhere on it does not take focus off the app you were typing
//!   in, and does not put FlyOnTheWall in the ⌘-tab list;
//! - Start prints an audit record naming `detection-prompt` *before* it starts
//!   capture, and the prompt is replaced by the recording pill;
//! - Not now / Never for this app print their suppression and take the prompt
//!   down without recording anything.
//!
//! Nothing here captures audio: `start_capture` prints and returns.

use std::time::Instant;

use fotw_shell::{DetectedMeeting, DetectionUpdate, HotkeyMap, Level, ShellHost, StartOrigin, run};

/// How long after launch the fake detector arms, so there is time to see the
/// prompt appear rather than finding it already there.
const ARM_AFTER_SECS: u64 = 3;

/// The prompt a fresh install actually draws.
///
/// `DetectorConfig::home_jurisdiction` defaults to `US-CA`, California is
/// all-party, so **the blocking checkbox is the default path** and not an
/// edge case worth previewing separately.
fn california_meeting() -> DetectedMeeting {
    DetectedMeeting::new("us.zoom.xos", "Zoom", "Zoom is using the microphone")
        .with_title("Weekly design review")
        .with_consent_notice(
            "These jurisdictions require every participant's consent:\n  \
             • California — Cal. Penal Code § 632 (https://leginfo.legislature.ca.gov/faces/\
             codes_displaySection.xhtml?lawCode=PEN&sectionNum=632)\n\
             This is not legal advice.",
            true,
        )
}

struct PreviewHost {
    started: Instant,
    armed: bool,
}

impl ShellHost for PreviewHost {
    fn poll_detection(&mut self) -> Option<DetectionUpdate> {
        if self.armed || self.started.elapsed().as_secs() < ARM_AFTER_SECS {
            return None;
        }
        self.armed = true;
        println!("detector: ARMED — a person must now press Start. Nothing is recording.");
        Some(DetectionUpdate::Armed(california_meeting()))
    }

    fn audit_start(&mut self, origin: StartOrigin) {
        println!("audit    : session start, origin = {}", origin.label());
    }

    fn start_capture(&mut self) -> Result<(), String> {
        println!("capture  : start (this preview records nothing)");
        Ok(())
    }

    fn stop_capture(&mut self) {
        println!("capture  : stop");
    }

    fn snooze_detection(&mut self) {
        println!("detector : snoozed — 'Not now'");
    }

    fn suppress_app(&mut self, app_key: &str) {
        println!("detector : suppressed {app_key} — 'Never for this app'");
    }

    fn level(&mut self) -> Level {
        // A slow sweep, so the pill's meter is visibly alive after a start.
        #[expect(
            clippy::cast_precision_loss,
            reason = "a preview animation over a value under 1000"
        )]
        Level::new((self.started.elapsed().as_millis() % 1_000) as f32 / 1_000.0)
    }

    fn quit(&mut self) {
        println!("quit");
        std::process::exit(0);
    }
}

fn main() {
    println!("prompt_preview — the detection prompt appears in {ARM_AFTER_SECS}s.");
    println!("  Quit from the menu-bar item when you are done.");
    println!();

    let host = PreviewHost {
        started: Instant::now(),
        armed: false,
    };
    // `run` never returns on success -- its `Ok` type is `Infallible` -- so
    // reaching the next line at all means the shell could not come up: a
    // hotkey another app already owns, or no window server.
    let Err(e) = run(host, HotkeyMap::defaults());
    eprintln!("prompt_preview: {e}");
    std::process::exit(1);
}
