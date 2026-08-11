//! Registering chords with the OS.
//!
//! The mapping in [`to_code`] is the enforcement point for the media-key ban
//! described in [`crate::hotkey`]: it has **no arm that can produce a media
//! `Code`**, so `global-hotkey` cannot be handed one, and therefore cannot
//! silently switch from Carbon `RegisterEventHotKey` (no TCC grant) to
//! `CGEventTapCreate` (Accessibility grant).

use global_hotkey::GlobalHotKeyManager;
use global_hotkey::hotkey::{Code, HotKey, Modifiers as GhModifiers};

use crate::error::ShellError;
use crate::hotkey::{Chord, HotkeyMap, Key, Modifiers};

/// Registered hotkeys, and the manager that owns them.
pub(crate) struct HotkeyRegistrar {
    /// Unregisters everything on drop.
    _manager: GlobalHotKeyManager,
    by_id: Vec<(u32, Chord)>,
}

impl HotkeyRegistrar {
    /// Register every chord in `map`.
    ///
    /// # Errors
    ///
    /// If a chord fails validation, cannot be mapped to a Carbon key code, or
    /// the OS refuses it (usually because another application already holds
    /// it).
    pub(crate) fn register(map: &HotkeyMap) -> Result<Self, ShellError> {
        let manager = GlobalHotKeyManager::new()
            .map_err(|e| ShellError::StatusItem(format!("global hotkeys: {e}")))?;

        let mut by_id = Vec::with_capacity(map.len());
        for (chord, _action) in map.bindings() {
            // Validated again here even though `HotkeyMap::bind` already did.
            // This is the last line before the FFI call, and the cost of the
            // check is nothing against the cost of the failure mode.
            chord.validate()?;
            let code = to_code(chord.key).ok_or_else(|| ShellError::HotkeyRegistration {
                chord,
                reason: "no Carbon virtual key code for this key".to_owned(),
            })?;
            let hotkey = HotKey::new(Some(to_mods(chord.mods)), code);
            manager
                .register(hotkey)
                .map_err(|e| ShellError::HotkeyRegistration {
                    chord,
                    reason: e.to_string(),
                })?;
            by_id.push((hotkey.id(), chord));
        }

        Ok(Self {
            _manager: manager,
            by_id,
        })
    }

    /// The chord a `GlobalHotKeyEvent` id refers to.
    pub(crate) fn chord_for(&self, id: u32) -> Option<Chord> {
        self.by_id
            .iter()
            .find(|(hid, _)| *hid == id)
            .map(|(_, chord)| *chord)
    }
}

fn to_mods(mods: Modifiers) -> GhModifiers {
    let mut out = GhModifiers::empty();
    if mods.contains(Modifiers::CONTROL) {
        out |= GhModifiers::CONTROL;
    }
    if mods.contains(Modifiers::OPTION) {
        out |= GhModifiers::ALT;
    }
    if mods.contains(Modifiers::SHIFT) {
        out |= GhModifiers::SHIFT;
    }
    if mods.contains(Modifiers::COMMAND) {
        // global-hotkey's macOS backend matches `SUPER | META` for Command.
        out |= GhModifiers::SUPER;
    }
    out
}

const LETTERS: [(char, Code); 26] = [
    ('a', Code::KeyA),
    ('b', Code::KeyB),
    ('c', Code::KeyC),
    ('d', Code::KeyD),
    ('e', Code::KeyE),
    ('f', Code::KeyF),
    ('g', Code::KeyG),
    ('h', Code::KeyH),
    ('i', Code::KeyI),
    ('j', Code::KeyJ),
    ('k', Code::KeyK),
    ('l', Code::KeyL),
    ('m', Code::KeyM),
    ('n', Code::KeyN),
    ('o', Code::KeyO),
    ('p', Code::KeyP),
    ('q', Code::KeyQ),
    ('r', Code::KeyR),
    ('s', Code::KeyS),
    ('t', Code::KeyT),
    ('u', Code::KeyU),
    ('v', Code::KeyV),
    ('w', Code::KeyW),
    ('x', Code::KeyX),
    ('y', Code::KeyY),
    ('z', Code::KeyZ),
];

const DIGITS: [Code; 10] = [
    Code::Digit0,
    Code::Digit1,
    Code::Digit2,
    Code::Digit3,
    Code::Digit4,
    Code::Digit5,
    Code::Digit6,
    Code::Digit7,
    Code::Digit8,
    Code::Digit9,
];

const FUNCTION_KEYS: [Code; 20] = [
    Code::F1,
    Code::F2,
    Code::F3,
    Code::F4,
    Code::F5,
    Code::F6,
    Code::F7,
    Code::F8,
    Code::F9,
    Code::F10,
    Code::F11,
    Code::F12,
    Code::F13,
    Code::F14,
    Code::F15,
    Code::F16,
    Code::F17,
    Code::F18,
    Code::F19,
    Code::F20,
];

/// Map a validated key onto a `global-hotkey` `Code`.
///
/// **There is deliberately no arm producing a media code.** See the module
/// docs; this is the structural half of the media-key ban.
fn to_code(key: Key) -> Option<Code> {
    match key {
        Key::Letter(c) => LETTERS
            .iter()
            .find(|(letter, _)| *letter == c)
            .map(|(_, code)| *code),
        Key::Digit(d) => DIGITS.get(usize::from(d)).copied(),
        Key::Function(n) => FUNCTION_KEYS.get(usize::from(n).checked_sub(1)?).copied(),
        Key::Space => Some(Code::Space),
        Key::Return => Some(Code::Enter),
        Key::Escape => Some(Code::Escape),
        Key::Comma => Some(Code::Comma),
        Key::Period => Some(Code::Period),
        Key::Slash => Some(Code::Slash),
        // No arm. A media key would take global-hotkey's CGEventTap path,
        // which needs the Accessibility grant we promised not to require.
        Key::Media(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::MediaKey;

    #[test]
    fn no_media_key_can_reach_global_hotkey() {
        for media in MediaKey::ALL {
            assert!(
                to_code(Key::Media(media)).is_none(),
                "{media:?} mapped to a global-hotkey Code; that is the CGEventTap path"
            );
        }
    }

    #[test]
    fn every_default_chord_maps() {
        for (chord, _) in HotkeyMap::defaults().bindings() {
            assert!(to_code(chord.key).is_some(), "{chord} has no Code");
        }
    }

    #[test]
    fn command_maps_to_super() {
        assert!(to_mods(Modifiers::COMMAND).contains(GhModifiers::SUPER));
        assert!(to_mods(Modifiers::OPTION).contains(GhModifiers::ALT));
    }
}
