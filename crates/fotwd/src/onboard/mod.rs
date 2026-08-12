//! Onboarding: prove the grants by using them, and distrust the proof when
//! the environment invalidates it (issue #31, docs/REQUIREMENTS.md 6.3).
//!
//! # Why this is not a permission check
//!
//! **There is no public API to query or request the macOS system-audio
//! grant.** The prompt fires only on the first `AudioDeviceStart` of an
//! aggregate device containing a tap, and a denial delivers *silence
//! indistinguishable from a quiet room*. AudioCap's private TCC.framework
//! probe is explicitly not shipped: it is undocumented and its own users
//! report it unreliable.
//!
//! So every step here is a **round trip**. Start the thing, play a tone
//! through the default output, count what came back. The only honest sentences
//! onboarding can say are about samples, never about permissions — see
//! `the_report_never_claims_a_permission_it_only_inferred` in
//! `tests/onboard.rs`.
//!
//! # And why a pass is sometimes not a pass
//!
//! *Verified in testing:* an unsigned or ad-hoc-signed binary run from a
//! terminal captures real system audio **with no prompt at all**, because it
//! inherits the grant from the responsible terminal process. Your development
//! machine will lie to you: you conclude capture works, you ship, and every
//! user gets silence.
//!
//! [`Environment::warnings`] is the countermeasure. A successful capture in an
//! untrustworthy environment is reported as
//! [`Outcome::VerifiedButUnsound`] — never as a pass — and the report is not
//! `ready()`. The two conditions that matter:
//!
//! - **Not in a `.app` bundle.** No `Info.plist`, therefore no
//!   `NSAudioCaptureUsageDescription`, and a missing usage description
//!   suppresses the TCC prompt rather than failing.
//! - **An ad-hoc or absent signature.** TCC keys its record off the
//!   Designated Requirement; an ad-hoc signature mints a new cdhash-based one
//!   on every rebuild, so the grant is dropped every time someone types
//!   `cargo build`. `just dev-sign` exists to solve exactly this, which is why
//!   a stable self-signed identity is treated as sound.

mod probe;

use std::time::Duration;

pub use probe::{HostProbe, ProbeReading, ToneSource};

/// How long each round trip runs.
///
/// Long enough that a scheduling hiccup cannot masquerade as "no callbacks
/// arrived" — which would report a denial to a user whose permissions are
/// fine. `fotw doctor` uses three seconds interactively; onboarding runs
/// several probes in a row, so it is shorter and still comfortably above the
/// noise floor.
pub const PROBE_WINDOW: Duration = Duration::from_millis(1_200);

/// The deep link to the pane the system-audio grant actually lives in.
///
/// Since macOS 15 it is surfaced as **"System Audio Recording Only"** inside
/// Privacy & Security → **Screen & System Audio Recording** — literally the
/// screen-recording pane, even though we never get screen access.
pub const SYSTEM_AUDIO_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

/// The microphone pane, which is a different pane and a different grant.
pub const MICROPHONE_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";

/// Our bundle identifier, used in the recovery commands.
pub const BUNDLE_ID: &str = "com.flyonthewall.fotw";

/// What this binary's code signature looks like to the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeSignature {
    /// No signature at all.
    Unsigned,
    /// `codesign -s -`: a new code identity on every build.
    AdHoc,
    /// A stable self-signed identity, as minted by `just dev-sign`.
    SelfSigned {
        /// The certificate's common name.
        common_name: String,
    },
    /// Signed by a real certificate authority chain.
    Authority {
        /// The leaf authority, e.g. `Developer ID Application: …`.
        name: String,
    },
}

impl CodeSignature {
    /// Read `codesign -dvv` output.
    ///
    /// Parsed rather than assumed: the difference between an ad-hoc signature
    /// and a stable one decides whether a passing probe means anything, and
    /// guessing from "is there a certificate file lying around" is how that
    /// gets silently wrong.
    #[must_use]
    pub fn parse(codesign_output: &str) -> Self {
        if codesign_output.contains("not signed at all") {
            return Self::Unsigned;
        }
        if codesign_output
            .lines()
            .any(|l| l.trim() == "Signature=adhoc")
        {
            return Self::AdHoc;
        }
        let Some(authority) = codesign_output
            .lines()
            .find_map(|l| l.trim().strip_prefix("Authority="))
        else {
            // Signed by something we could not read. Treated as ad-hoc: the
            // conservative reading, because the consequence of being wrong the
            // other way is a developer trusting a result they should not.
            return Self::AdHoc;
        };
        // A self-signed identity has exactly one authority line and no Apple
        // chain above it. `just dev-sign` produces "FlyOnTheWall Dev".
        let chained = codesign_output
            .lines()
            .filter(|l| l.trim_start().starts_with("Authority="))
            .count()
            > 1;
        if chained {
            Self::Authority {
                name: authority.to_owned(),
            }
        } else {
            Self::SelfSigned {
                common_name: authority.to_owned(),
            }
        }
    }

    /// Whether TCC's record of this code survives a rebuild.
    #[must_use]
    pub const fn identity_is_stable(&self) -> bool {
        matches!(self, Self::SelfSigned { .. } | Self::Authority { .. })
    }
}

/// How this binary is running.
#[derive(Debug, Clone)]
pub struct Environment {
    /// Whether the executable sits inside `…/Contents/MacOS`.
    pub in_app_bundle: bool,
    /// What its signature looks like.
    pub signature: CodeSignature,
    /// The terminal it was launched from, if it was.
    pub terminal: Option<String>,
}

/// A reason a successful capture proves less than it appears to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustWarning {
    /// The binary is unbundled **and** was launched from a terminal, so any
    /// grant it appears to have may be the terminal's.
    InheritedTerminalGrant {
        /// Which terminal, as far as the environment says.
        terminal: String,
    },
    /// No `Info.plist`, so no usage description, so no prompt.
    NotInBundle,
    /// Ad-hoc or absent signature: the grant is dropped on the next build.
    IdentityChurnsEveryBuild,
}

impl TrustWarning {
    /// One line, shouted.
    #[must_use]
    pub fn headline(&self) -> String {
        match self {
            Self::InheritedTerminalGrant { terminal } => format!(
                "THIS RESULT IS NOT EVIDENCE — the grant may belong to your terminal ({terminal})"
            ),
            Self::NotInBundle => {
                "Not running from FlyOnTheWall.app — macOS never sees our usage descriptions"
                    .to_owned()
            }
            Self::IdentityChurnsEveryBuild => {
                "Ad-hoc signature — this grant will be dropped by the next `cargo build`".to_owned()
            }
        }
    }

    /// The paragraph under the headline.
    #[must_use]
    pub fn explain(&self) -> String {
        match self {
            Self::InheritedTerminalGrant { terminal } => format!(
                "An unbundled binary launched from a terminal inherits that terminal's \
                 TCC grant: macOS holds {terminal} responsible, not us. Audio really was \
                 captured, and it proves nothing about what a user will get — they will \
                 get silence. Build and run the bundle instead:\n\
                 \x20   just run"
            ),
            Self::NotInBundle => "TCC reads NSAudioCaptureUsageDescription and \
                 NSMicrophoneUsageDescription from Contents/Info.plist. Without a bundle there \
                 is no plist, and a missing usage description suppresses the prompt entirely \
                 rather than failing. Use `just run`."
                .to_owned(),
            Self::IdentityChurnsEveryBuild => "TCC keys its record off the code's Designated \
                 Requirement. An ad-hoc signature mints a new cdhash-based requirement on \
                 every build, so macOS treats each build as a brand-new app and drops the \
                 grant. `just dev-sign` creates a persisted self-signed identity that does \
                 not move."
                .to_owned(),
        }
    }
}

impl Environment {
    /// Everything that makes this run's evidence untrustworthy, worst first.
    ///
    /// Note that the terminal case does **not** depend on being unbundled.
    /// Launching `FlyOnTheWall.app/Contents/MacOS/fotwd` from a shell makes the
    /// *terminal* the responsible process just as surely as running a bare
    /// `target/debug/fotwd` does — the bundle only helps when the bundle is
    /// what gets launched (`open -a`, Finder, launchd). Getting this wrong
    /// would hand a clean bill of health to the one habit the justfile
    /// specifically warns against.
    #[must_use]
    pub fn warnings(&self) -> Vec<TrustWarning> {
        let mut out = Vec::new();
        if let Some(terminal) = &self.terminal {
            out.push(TrustWarning::InheritedTerminalGrant {
                terminal: terminal.clone(),
            });
        }
        if !self.in_app_bundle {
            out.push(TrustWarning::NotInBundle);
        }
        if !self.signature.identity_is_stable() {
            out.push(TrustWarning::IdentityChurnsEveryBuild);
        }
        out
    }

    /// Whether a passing probe in this environment means anything for users.
    #[must_use]
    pub fn evidence_is_trustworthy(&self) -> bool {
        self.warnings().is_empty()
    }

    /// Read the environment this process is actually running in.
    ///
    /// Kept out of the pure logic above so every branch of it is testable
    /// without a bundle, a terminal or a signing identity.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            in_app_bundle: probe::in_app_bundle(),
            signature: probe::signature(),
            terminal: probe::responsible_terminal(),
        }
    }
}

/// One thing onboarding verifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The system-audio tap (the grant that cannot be queried).
    SystemAudio,
    /// The microphone leg.
    Microphone,
}

impl Step {
    /// What the step is called on screen.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SystemAudio => "System audio",
            Self::Microphone => "Microphone",
        }
    }
}

/// What to do about a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remedy {
    /// Where to go, in words.
    pub headline: String,
    /// A `x-apple.systempreferences:` deep link, when one exists.
    pub settings_url: Option<&'static str>,
    /// Shell commands worth running, in order.
    pub commands: Vec<String>,
}

/// The verdict on one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The round trip worked, in an environment where that means something.
    Verified {
        /// What arrived.
        detail: String,
    },
    /// The round trip worked, but the environment invalidates the result.
    VerifiedButUnsound {
        /// What arrived.
        detail: String,
        /// Why it does not count.
        warnings: Vec<TrustWarning>,
    },
    /// The round trip did not work, and we know what to do about it.
    Failed {
        /// What happened.
        detail: String,
        /// What to do.
        remedy: Remedy,
    },
    /// The measurement cannot distinguish success from failure.
    Inconclusive {
        /// What happened, and what to change before trying again.
        detail: String,
        /// What to do if it turns out to be a denial.
        remedy: Remedy,
    },
}

impl Outcome {
    /// Whether this step can be relied on.
    #[must_use]
    pub const fn is_sound(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }

    /// A one-character status for the report.
    #[must_use]
    pub const fn mark(&self) -> &'static str {
        match self {
            Self::Verified { .. } => "✓",
            Self::VerifiedButUnsound { .. } => "!",
            Self::Failed { .. } => "✗",
            Self::Inconclusive { .. } => "?",
        }
    }
}

/// The remedy for a system-audio failure.
fn system_audio_remedy() -> Remedy {
    Remedy {
        headline: "System Settings → Privacy & Security → Screen & System Audio Recording, \
                   then enable FlyOnTheWall under \"System Audio Recording Only\". \
                   We never get screen access; that is just where Apple put it."
            .to_owned(),
        settings_url: Some(SYSTEM_AUDIO_SETTINGS_URL),
        // The service is `AudioCapture`. Several 2026 write-ups (and issue #31)
        // cite `SystemAudioCaptureRequests`, which does not exist anywhere:
        // /usr/bin/tccutil builds `kTCCService%s`, and `kTCCServiceAudioCapture`
        // is the symbol present in tccd on macOS 26.3.
        commands: vec![format!("tccutil reset AudioCapture {BUNDLE_ID}")],
    }
}

fn microphone_remedy() -> Remedy {
    Remedy {
        headline: "System Settings → Privacy & Security → Microphone, then enable FlyOnTheWall."
            .to_owned(),
        settings_url: Some(MICROPHONE_SETTINGS_URL),
        commands: vec![format!("tccutil reset Microphone {BUNDLE_ID}")],
    }
}

/// Turn one round-trip measurement into a verdict.
///
/// Pure, so every branch is reachable from a test with no audio device: the
/// reading and the environment are both data.
#[must_use]
pub fn interpret(step: Step, reading: &ProbeReading, env: &Environment) -> Outcome {
    let remedy = match step {
        Step::SystemAudio => system_audio_remedy(),
        Step::Microphone => microphone_remedy(),
    };

    if let Some(error) = &reading.error {
        return Outcome::Failed {
            detail: format!("could not start: {error}"),
            remedy,
        };
    }
    if !reading.started {
        return Outcome::Failed {
            detail: "the capture never started".to_owned(),
            remedy,
        };
    }
    if reading.callbacks == 0 {
        return Outcome::Failed {
            detail: "no callbacks at all — the IO proc never fired".to_owned(),
            remedy,
        };
    }

    // The asymmetry between the two legs is deliberate. We can play a tone
    // into the system mix, so silence there is meaningful. We cannot make the
    // user speak, so silence on the mic leg is just a quiet room.
    if reading.nonzero == 0 {
        match step {
            Step::SystemAudio if reading.tone_played => {
                return Outcome::Failed {
                    detail: format!(
                        "{} callbacks, every sample digitally silent, while a test tone was \
                         playing — this is what a denied system-audio grant looks like",
                        reading.callbacks
                    ),
                    remedy,
                };
            }
            Step::SystemAudio => {
                return Outcome::Inconclusive {
                    detail: format!(
                        "{} callbacks, all silent, and no tone was playing. Play some audio \
                         and run this again — silence and a denial are indistinguishable \
                         from here",
                        reading.callbacks
                    ),
                    remedy,
                };
            }
            Step::Microphone => {}
        }
    }

    let detail = match step {
        Step::Microphone if reading.nonzero == 0 => format!(
            "{} callbacks, {} samples, all silent (a quiet room reads exactly like this)",
            reading.callbacks, reading.samples
        ),
        _ => format!(
            "{} callbacks, {} samples, {} non-zero",
            reading.callbacks, reading.samples, reading.nonzero
        ),
    };

    let warnings = env.warnings();
    if warnings.is_empty() {
        Outcome::Verified { detail }
    } else {
        Outcome::VerifiedButUnsound { detail, warnings }
    }
}

/// Greedy wrap, so a paragraph of recovery copy is readable in a terminal.
///
/// Hand-rolled for the same reason `fotwd`'s transcript wrap is: one function,
/// no dependency, and the failure mode is cosmetic.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

/// Everything onboarding measured.
#[derive(Debug, Default)]
pub struct Report {
    steps: Vec<(Step, Outcome)>,
}

impl Report {
    /// Record a step's verdict.
    pub fn push(&mut self, step: Step, outcome: Outcome) {
        self.steps.push((step, outcome));
    }

    /// Every verdict, in order.
    pub fn steps(&self) -> impl Iterator<Item = &(Step, Outcome)> {
        self.steps.iter()
    }

    /// Whether this machine will record for a *user*.
    ///
    /// A [`Outcome::VerifiedButUnsound`] is deliberately not ready: reporting
    /// an inherited-grant pass as a working install is the exact mistake this
    /// module exists to prevent.
    #[must_use]
    pub fn ready(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|(_, o)| o.is_sound())
    }

    /// The report as text.
    ///
    /// Never says "granted", "authorized" or "has permission": we do not know
    /// any of those things and cannot find out. It says what arrived.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (step, outcome) in &self.steps {
            out.push_str(&format!(
                "  {} {:<13} {}\n",
                outcome.mark(),
                step.label(),
                match outcome {
                    Outcome::Verified { detail }
                    | Outcome::VerifiedButUnsound { detail, .. }
                    | Outcome::Failed { detail, .. }
                    | Outcome::Inconclusive { detail, .. } => detail,
                }
            ));
            match outcome {
                Outcome::VerifiedButUnsound { warnings, .. } => {
                    for w in warnings {
                        out.push_str(&format!("\n    !! {}\n", w.headline()));
                        for line in wrap(&w.explain(), 68) {
                            out.push_str(&format!("       {line}\n"));
                        }
                    }
                }
                Outcome::Failed { remedy, .. } | Outcome::Inconclusive { remedy, .. } => {
                    out.push('\n');
                    for line in wrap(&remedy.headline, 68) {
                        out.push_str(&format!("    → {line}\n"));
                    }
                    for cmd in &remedy.commands {
                        out.push_str(&format!("        {cmd}\n"));
                    }
                }
                Outcome::Verified { .. } => {}
            }
        }
        out.push('\n');
        if self.ready() {
            out.push_str(
                "  ready — audio arrived on every leg, from a bundle macOS can hold \
                          responsible.\n",
            );
        } else {
            out.push_str(
                "  NOT ready — see above. Nothing here can tell you the permission is \
                 granted; only that audio did or did not arrive.\n",
            );
        }
        out
    }
}
