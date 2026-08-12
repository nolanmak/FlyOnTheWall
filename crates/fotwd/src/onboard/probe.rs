//! The round trip itself, and the environment it ran in.
//!
//! Everything in this file talks to the actual machine, which is why the
//! interpretation of what it returns lives in the parent module instead: the
//! verdicts are pure functions of a [`ProbeReading`] and an
//! [`Environment`](super::Environment), so they are all reachable from a test
//! with no audio device and no TCC database.

use std::io::IsTerminal;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fotw_audio::{
    AudioPlatform, CaptureTimestamp, DeviceId, FormatRequest, FrameFlags, FrameSink, SystemScope,
    TapError, platform,
};

use super::CodeSignature;

/// What one round trip measured.
///
/// Deliberately raw. It carries no opinion about what the numbers mean —
/// [`super::interpret`] holds all of that, so the opinion is testable and this
/// is not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeReading {
    /// Whether the tap started at all.
    pub started: bool,
    /// How many times the IO proc fired.
    pub callbacks: u64,
    /// How many samples arrived.
    pub samples: u64,
    /// How many of them were not exactly zero.
    pub nonzero: u64,
    /// Whether a test tone was playing during the window. Decides whether
    /// silence is a denial or just a quiet machine.
    pub tone_played: bool,
    /// Why it failed, if it did.
    pub error: Option<String>,
}

#[derive(Debug, Default)]
struct Counters {
    callbacks: AtomicU64,
    samples: AtomicU64,
    nonzero: AtomicU64,
}

struct CountingSink(Arc<Counters>);

impl FrameSink for CountingSink {
    fn on_frames(&mut self, pcm: &[f32], _ts: CaptureTimestamp, _flags: FrameFlags) {
        self.0.callbacks.fetch_add(1, Ordering::Relaxed);
        self.0
            .samples
            .fetch_add(pcm.len() as u64, Ordering::Relaxed);
        let nonzero = pcm.iter().filter(|s| **s != 0.0).count() as u64;
        self.0.nonzero.fetch_add(nonzero, Ordering::Relaxed);
    }

    fn on_error(&mut self, _e: TapError) {}
}

/// A test tone playing through the default output.
///
/// Shelling out to `afplay` rather than opening an output stream: this needs
/// no audio-output code anywhere in the project, no new dependency, and — the
/// point — it plays from **another process**, so it exercises the same path a
/// meeting does. A tone we rendered ourselves would be captured through our
/// own process's audio client, which is not the thing being tested.
#[derive(Debug)]
pub struct ToneSource(Option<Child>);

impl ToneSource {
    /// Start the tone. Returns a handle that stops it when dropped.
    #[must_use]
    pub fn start() -> Self {
        // Submarine is the longest of the stock sounds, and repeating it keeps
        // the window covered. `-v 2` because the tap is attenuated in
        // proportion to the output device's stereo-pair count (6.4), and a
        // quiet tone on an 8-output interface can land near the noise floor.
        let child = Command::new("/usr/bin/afplay")
            .args(["-v", "2", "/System/Library/Sounds/Submarine.aiff"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        Self(child)
    }

    /// Whether the tone is actually playing.
    #[must_use]
    pub const fn playing(&self) -> bool {
        self.0.is_some()
    }
}

impl Drop for ToneSource {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Runs the real round trips against this machine.
#[derive(Debug, Default)]
pub struct HostProbe;

impl HostProbe {
    /// Capture the system mix for `window`, with a tone playing.
    #[must_use]
    pub fn system_audio(&self, window: Duration) -> ProbeReading {
        let plat = platform::host();
        let tap = plat.open_system(SystemScope::DefaultOutputMix, FormatRequest::any());
        let tone = ToneSource::start();
        let mut reading = Self::run(tap, window);
        reading.tone_played = tone.playing();
        reading
    }

    /// Capture the default microphone for `window`.
    ///
    /// No tone: playing one into the microphone leg would measure the room,
    /// not the grant, and on speakers it would be captured by the echo path
    /// we are otherwise trying to keep out of the recording.
    #[must_use]
    pub fn microphone(&self, window: Duration) -> ProbeReading {
        let plat = platform::host();
        let tap = plat.open_mic(&DeviceId::new("default"), FormatRequest::any());
        Self::run(tap, window)
    }

    fn run(tap: Result<Box<dyn fotw_audio::AudioTap>, TapError>, window: Duration) -> ProbeReading {
        let mut tap = match tap {
            Ok(t) => t,
            Err(e) => {
                return ProbeReading {
                    error: Some(e.to_string()),
                    ..ProbeReading::default()
                };
            }
        };
        let counters = Arc::new(Counters::default());
        // This is the call that raises the TCC prompt on macOS -- not the tap
        // creation above. A denial does not fail it; it delivers silence.
        if let Err(e) = tap.start(Box::new(CountingSink(Arc::clone(&counters)))) {
            return ProbeReading {
                error: Some(e.to_string()),
                ..ProbeReading::default()
            };
        }

        let began = Instant::now();
        while began.elapsed() < window {
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = tap.stop();

        ProbeReading {
            started: true,
            callbacks: counters.callbacks.load(Ordering::Relaxed),
            samples: counters.samples.load(Ordering::Relaxed),
            nonzero: counters.nonzero.load(Ordering::Relaxed),
            tone_played: false,
            error: None,
        }
    }
}

/// Whether this executable sits inside a `.app`.
pub(super) fn in_app_bundle() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.ends_with("Contents/MacOS")))
        .unwrap_or(false)
}

/// Read this executable's signature with `codesign -dvv`.
///
/// `codesign` writes its description to **stderr**, which is the detail that
/// makes a naive implementation report every binary as unsigned.
pub(super) fn signature() -> CodeSignature {
    let Ok(exe) = std::env::current_exe() else {
        return CodeSignature::Unsigned;
    };
    let Ok(out) = Command::new("/usr/bin/codesign")
        .arg("-dvv")
        .arg(&exe)
        .output()
    else {
        return CodeSignature::Unsigned;
    };
    let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    CodeSignature::parse(&text)
}

/// The terminal this process was launched from, if it was.
///
/// Three signals, cheapest first, because any one of them can be absent:
/// `TERM_PROGRAM` is unset over SSH and under `launchd`; stdin is not a tty
/// when output is piped (which is exactly how a developer runs this while
/// grepping the output); and the parent process is the ground truth but costs
/// two `ps` calls.
///
/// Being *wrong* here is asymmetric, so it leans toward reporting a terminal:
/// a spurious warning costs a paragraph of output, while a missed one is how
/// a developer ships a build that records silence for everyone.
pub(super) fn responsible_terminal() -> Option<String> {
    if let Ok(program) = std::env::var("TERM_PROGRAM")
        && !program.is_empty()
    {
        return Some(program);
    }
    if let Some(parent) = parent_command()
        && is_shell_like(&parent)
    {
        return Some(parent);
    }
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        return Some("a terminal".to_owned());
    }
    None
}

fn parent_command() -> Option<String> {
    let ppid = ps_field("ppid=", &std::process::id().to_string())?;
    let comm = ps_field("comm=", ppid.trim())?;
    let comm = comm.trim();
    Some(
        comm.rsplit('/')
            .next()
            .unwrap_or(comm)
            .trim_start_matches('-')
            .to_owned(),
    )
}

fn ps_field(field: &str, pid: &str) -> Option<String> {
    let out = Command::new("/bin/ps")
        .args(["-o", field, "-p", pid])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Whether a parent process name means "a person is at a shell".
fn is_shell_like(name: &str) -> bool {
    const SHELLS: [&str; 8] = ["zsh", "bash", "sh", "fish", "dash", "ksh", "tcsh", "csh"];
    const TERMINALS: [&str; 6] = [
        "Terminal",
        "iTerm2",
        "ghostty",
        "alacritty",
        "kitty",
        "wezterm",
    ];
    SHELLS.contains(&name) || TERMINALS.iter().any(|t| name.eq_ignore_ascii_case(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_and_terminals_are_recognised_and_other_parents_are_not() {
        for name in ["zsh", "bash", "fish", "ghostty", "Terminal", "iTerm2"] {
            assert!(is_shell_like(name), "{name}");
        }
        // The cases that must NOT be read as a terminal: these are how the
        // shipped app is actually started, and a false positive here would
        // print the loud warning to every user.
        for name in ["launchd", "Finder", "fotwd", "open", "systemd"] {
            assert!(!is_shell_like(name), "{name}");
        }
    }
}
