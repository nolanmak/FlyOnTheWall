//! Global hotkeys, described in plain Rust so the rules are testable on Linux.
//!
//! The shell registers hotkeys through `global-hotkey`, which on macOS uses
//! Carbon's `RegisterEventHotKey`. That path needs **no** Accessibility TCC
//! grant, which is the entire reason it was chosen (docs/REQUIREMENTS.md 5.5).
//!
//! # The trap
//!
//! `global-hotkey` silently takes a *different* path for media keys. From its
//! macOS backend (`src/platform_impl/macos/mod.rs`), `register()` is:
//!
//! ```text
//! if let Some(scan_code) = key_to_scancode(hotkey.key) {  // Carbon
//! } else if is_media_key(hotkey.key) {                    // CGEventTapCreate
//! } else {                                                // Err
//! ```
//!
//! and `is_media_key` is exactly `MediaPlayPause | MediaTrackNext |
//! MediaTrackPrevious | MediaFastForward | MediaRewind`. That branch calls
//! `CGEventTapCreate(Session, HeadInsertEventTap, Default, …)` — an *active*
//! session tap, which macOS gates behind the Accessibility grant. The crate
//! never calls `AXIsProcessTrusted`, so without the grant you get a null tap
//! and a `FailedToWatchMediaKeyEvent` error, and with a partially-granted
//! system you get a hotkey that never fires and no diagnostic at all.
//!
//! So this module makes the ban structural: [`Chord::validate`] rejects every
//! media key before a chord can reach the registrar, and the mapping into
//! `global_hotkey::hotkey::Code` has no arm that can produce one.
//!
//! Two corrections to what the platform notes claim, verified against
//! `global-hotkey 0.8.0` source:
//!
//! - `Code::MediaStop` is **not** in `is_media_key` *and* has no scancode, so
//!   it fails with `FailedToRegister("Unknown scancode")` rather than taking
//!   the tap path. Rejected here as [`HotkeyError::UnsupportedKey`].
//! - `AudioVolumeUp` / `AudioVolumeDown` / `AudioVolumeMute` **do** have
//!   Carbon scancodes (`0x48`/`0x49`/`0x4a`) and take the Carbon path, so they
//!   need no Accessibility grant. They are still rejected here, for the
//!   different reason that the system consumes them first
//!   ([`HotkeyError::SystemReserved`]).

use std::fmt;

/// Chord modifiers, as a bit set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Modifiers(u8);

impl Modifiers {
    /// No modifiers.
    pub const NONE: Self = Self(0);
    /// The Control key.
    pub const CONTROL: Self = Self(1 << 0);
    /// The Option (Alt) key.
    pub const OPTION: Self = Self(1 << 1);
    /// The Shift key.
    pub const SHIFT: Self = Self(1 << 2);
    /// The Command key.
    pub const COMMAND: Self = Self(1 << 3);

    /// Whether every bit of `other` is set here.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no modifier is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw bits, for the platform mapping.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl fmt::Display for Modifiers {
    /// Apple's canonical order: Control, Option, Shift, Command.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.contains(Self::CONTROL) {
            f.write_str("⌃")?;
        }
        if self.contains(Self::OPTION) {
            f.write_str("⌥")?;
        }
        if self.contains(Self::SHIFT) {
            f.write_str("⇧")?;
        }
        if self.contains(Self::COMMAND) {
            f.write_str("⌘")?;
        }
        Ok(())
    }
}

/// A media key. Every variant is rejected; the enum exists so the *reason* can
/// be specific and so the rejection is exhaustive over the real key set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MediaKey {
    /// Play/Pause. Takes the CGEventTap path.
    PlayPause,
    /// Next track. Takes the CGEventTap path.
    NextTrack,
    /// Previous track. Takes the CGEventTap path.
    PreviousTrack,
    /// Fast forward. Takes the CGEventTap path.
    FastForward,
    /// Rewind. Takes the CGEventTap path.
    Rewind,
    /// Stop. Has neither a scancode nor a tap arm in `global-hotkey 0.8`.
    Stop,
    /// Volume up. Carbon path, but consumed by the system.
    VolumeUp,
    /// Volume down. Carbon path, but consumed by the system.
    VolumeDown,
    /// Mute. Carbon path, but consumed by the system.
    Mute,
}

impl MediaKey {
    /// Every media key, so the rejection tests are exhaustive.
    pub const ALL: [Self; 9] = [
        Self::PlayPause,
        Self::NextTrack,
        Self::PreviousTrack,
        Self::FastForward,
        Self::Rewind,
        Self::Stop,
        Self::VolumeUp,
        Self::VolumeDown,
        Self::Mute,
    ];

    /// Whether `global-hotkey` routes this key through `CGEventTapCreate`.
    #[must_use]
    pub const fn needs_event_tap(self) -> bool {
        matches!(
            self,
            Self::PlayPause
                | Self::NextTrack
                | Self::PreviousTrack
                | Self::FastForward
                | Self::Rewind
        )
    }
}

/// The key half of a chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    /// An ASCII letter. Normalised to lowercase by [`Key::letter`].
    Letter(char),
    /// A digit row key, `0..=9`.
    Digit(u8),
    /// A function key, `F1..=F20`.
    Function(u8),
    /// Space.
    Space,
    /// Return.
    Return,
    /// Escape.
    Escape,
    /// Comma.
    Comma,
    /// Period.
    Period,
    /// Forward slash.
    Slash,
    /// A media key. Always rejected — see the module docs.
    Media(MediaKey),
}

impl Key {
    /// An ASCII letter key, lowercased. `None` for anything else.
    #[must_use]
    pub fn letter(c: char) -> Option<Self> {
        c.is_ascii_alphabetic()
            .then(|| Self::Letter(c.to_ascii_lowercase()))
    }

    /// Whether this key is usable without a modifier.
    ///
    /// Only `F13..=F20`. Those keys exist on extended keyboards precisely to
    /// be bound bare and are not claimed by the system; every other bare key
    /// would be stolen from every other application on the Mac.
    #[must_use]
    pub const fn is_safe_bare(self) -> bool {
        matches!(self, Self::Function(n) if n >= 13 && n <= 20)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Letter(c) => write!(f, "{}", c.to_ascii_uppercase()),
            Self::Digit(d) => write!(f, "{d}"),
            Self::Function(n) => write!(f, "F{n}"),
            Self::Space => f.write_str("Space"),
            Self::Return => f.write_str("Return"),
            Self::Escape => f.write_str("Esc"),
            Self::Comma => f.write_str(","),
            Self::Period => f.write_str("."),
            Self::Slash => f.write_str("/"),
            Self::Media(m) => write!(f, "{m:?}"),
        }
    }
}

/// A modifier-plus-key combination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chord {
    /// The modifiers held.
    pub mods: Modifiers,
    /// The key pressed.
    pub key: Key,
}

impl Chord {
    /// Build a chord. Does not validate; call [`Chord::validate`] or bind it
    /// into a [`HotkeyMap`], which validates for you.
    #[must_use]
    pub const fn new(mods: Modifiers, key: Key) -> Self {
        Self { mods, key }
    }

    /// Whether this chord can be registered without an Accessibility grant
    /// and without stealing a key from the rest of the system.
    ///
    /// # Errors
    ///
    /// See [`HotkeyError`]. Media keys are rejected first, because that is the
    /// failure with no visible symptom.
    pub fn validate(self) -> Result<(), HotkeyError> {
        if let Key::Media(media) = self.key {
            return Err(match media {
                MediaKey::Stop => HotkeyError::UnsupportedKey { key: media },
                MediaKey::VolumeUp | MediaKey::VolumeDown | MediaKey::Mute => {
                    HotkeyError::SystemReserved { key: media }
                }
                _ => HotkeyError::MediaKeyNeedsAccessibility { key: media },
            });
        }
        match self.key {
            Key::Letter(c) if !c.is_ascii_lowercase() => {
                return Err(HotkeyError::InvalidKey { key: self.key });
            }
            Key::Digit(d) if d > 9 => return Err(HotkeyError::InvalidKey { key: self.key }),
            Key::Function(n) if n == 0 || n > 20 => {
                return Err(HotkeyError::InvalidKey { key: self.key });
            }
            _ => {}
        }
        if self.mods.is_empty() && !self.key.is_safe_bare() {
            return Err(HotkeyError::NoModifier { key: self.key });
        }
        Ok(())
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.mods, self.key)
    }
}

/// What a hotkey does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    /// Start a session, or stop the running one.
    ToggleRecording,
    /// Bring the notes window forward.
    OpenNotes,
}

impl HotkeyAction {
    /// Every action.
    pub const ALL: [Self; 2] = [Self::ToggleRecording, Self::OpenNotes];
}

/// Why a chord cannot be registered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HotkeyError {
    /// A media key that `global-hotkey` routes through `CGEventTapCreate`.
    #[error(
        "{key:?} is a media key: global-hotkey registers it with CGEventTapCreate, \
         which requires the Accessibility grant in System Settings. The user would \
         get an unexplained prompt, or a hotkey that never fires and reports no error"
    )]
    MediaKeyNeedsAccessibility {
        /// The offending key.
        key: MediaKey,
    },
    /// A media key `global-hotkey 0.8` cannot register at all.
    #[error(
        "{key:?} has no Carbon virtual key code in global-hotkey 0.8 and no event-tap arm either"
    )]
    UnsupportedKey {
        /// The offending key.
        key: MediaKey,
    },
    /// A key macOS consumes before any application sees it.
    #[error("{key:?} is consumed by the system before an application hotkey can match it")]
    SystemReserved {
        /// The offending key.
        key: MediaKey,
    },
    /// A bare key that would be taken from every other application.
    #[error(
        "{key} needs at least one modifier: a bare global hotkey steals the key from every app"
    )]
    NoModifier {
        /// The offending key.
        key: Key,
    },
    /// A key outside its valid range.
    #[error("{key:?} is not a valid key")]
    InvalidKey {
        /// The offending key.
        key: Key,
    },
    /// The chord is already bound.
    #[error("{chord} is already bound to {existing:?}")]
    DuplicateChord {
        /// The chord.
        chord: Chord,
        /// What it is already bound to.
        existing: HotkeyAction,
    },
    /// The action already has a chord.
    #[error("{action:?} is already bound to {existing}")]
    DuplicateAction {
        /// The action.
        action: HotkeyAction,
        /// The chord it already has.
        existing: Chord,
    },
}

/// The chord-to-action table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HotkeyMap {
    bindings: Vec<(Chord, HotkeyAction)>,
}

impl HotkeyMap {
    /// A map with nothing bound.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            bindings: Vec::new(),
        }
    }

    /// The shipped defaults.
    ///
    /// # Panics
    ///
    /// If a default fails [`Chord::validate`] — which is the point. A future
    /// edit that binds a media key by default fails loudly at the first call
    /// instead of shipping a hotkey that never fires.
    #[must_use]
    pub fn defaults() -> Self {
        let mut map = Self::empty();
        map.bind(
            Chord::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Letter('r')),
            HotkeyAction::ToggleRecording,
        )
        .expect("default toggle-recording chord must be registrable");
        map.bind(
            Chord::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Letter('n')),
            HotkeyAction::OpenNotes,
        )
        .expect("default open-notes chord must be registrable");
        map
    }

    /// Bind a chord to an action.
    ///
    /// # Errors
    ///
    /// If the chord is not registrable, or if either the chord or the action
    /// is already bound. One chord per action and one action per chord: a
    /// duplicate is always a mistake, and silently letting the last one win
    /// makes it invisible.
    pub fn bind(&mut self, chord: Chord, action: HotkeyAction) -> Result<(), HotkeyError> {
        chord.validate()?;
        if let Some((_, existing)) = self.bindings.iter().find(|(c, _)| *c == chord) {
            return Err(HotkeyError::DuplicateChord {
                chord,
                existing: *existing,
            });
        }
        if let Some((existing, _)) = self.bindings.iter().find(|(_, a)| *a == action) {
            return Err(HotkeyError::DuplicateAction {
                action,
                existing: *existing,
            });
        }
        self.bindings.push((chord, action));
        Ok(())
    }

    /// The action a chord triggers.
    #[must_use]
    pub fn action_for(&self, chord: Chord) -> Option<HotkeyAction> {
        self.bindings
            .iter()
            .find(|(c, _)| *c == chord)
            .map(|(_, a)| *a)
    }

    /// The chord bound to an action.
    #[must_use]
    pub fn chord_for(&self, action: HotkeyAction) -> Option<Chord> {
        self.bindings
            .iter()
            .find(|(_, a)| *a == action)
            .map(|(c, _)| *c)
    }

    /// Every binding, in bind order.
    pub fn bindings(&self) -> impl Iterator<Item = (Chord, HotkeyAction)> + '_ {
        self.bindings.iter().copied()
    }

    /// How many chords are bound.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Whether nothing is bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}
