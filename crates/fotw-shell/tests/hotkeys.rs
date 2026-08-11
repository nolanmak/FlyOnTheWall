//! Global hotkey rules.
//!
//! The load-bearing test here is [`every_media_key_is_rejected`]. Registering
//! a media key switches `global-hotkey` from Carbon `RegisterEventHotKey`
//! (no TCC grant) to `CGEventTapCreate` (Accessibility grant), and the
//! symptom is either an unexplained System Settings prompt or a hotkey that
//! never fires and reports nothing. See `crate::hotkey` for the source
//! citation.

use fotw_shell::{Chord, HotkeyAction, HotkeyError, HotkeyMap, Key, MediaKey, Modifiers};

fn cmd_shift() -> Modifiers {
    Modifiers::COMMAND | Modifiers::SHIFT
}

#[test]
fn every_media_key_is_rejected() {
    // Exhaustive over `MediaKey::ALL`, so a new media key cannot be added
    // without deciding why it is refused.
    for media in MediaKey::ALL {
        let chord = Chord::new(cmd_shift(), Key::Media(media));
        let err = chord
            .validate()
            .expect_err("no media key may reach global-hotkey");

        let expected = match media {
            MediaKey::PlayPause
            | MediaKey::NextTrack
            | MediaKey::PreviousTrack
            | MediaKey::FastForward
            | MediaKey::Rewind => HotkeyError::MediaKeyNeedsAccessibility { key: media },
            MediaKey::Stop => HotkeyError::UnsupportedKey { key: media },
            MediaKey::VolumeUp | MediaKey::VolumeDown | MediaKey::Mute => {
                HotkeyError::SystemReserved { key: media }
            }
        };
        assert_eq!(err, expected, "{media:?} was refused for the wrong reason");
    }
}

#[test]
fn the_event_tap_set_matches_global_hotkeys_source() {
    // `is_media_key` in global-hotkey 0.8.0's macOS backend, verbatim:
    //   MediaPlayPause | MediaTrackNext | MediaTrackPrevious
    //   | MediaFastForward | MediaRewind
    // Everything in that set takes CGEventTapCreate; nothing else does.
    let tapped: Vec<MediaKey> = MediaKey::ALL
        .into_iter()
        .filter(|key| key.needs_event_tap())
        .collect();
    assert_eq!(
        tapped,
        vec![
            MediaKey::PlayPause,
            MediaKey::NextTrack,
            MediaKey::PreviousTrack,
            MediaKey::FastForward,
            MediaKey::Rewind,
        ]
    );

    // Volume keys take the *Carbon* path (scancodes 0x48/0x49/0x4a), which is
    // why they are refused for a different reason. Getting this backwards
    // would put a misleading message in front of the user.
    for volume in [MediaKey::VolumeUp, MediaKey::VolumeDown, MediaKey::Mute] {
        assert!(!volume.needs_event_tap());
    }
}

#[test]
fn a_media_key_cannot_be_bound_even_deliberately() {
    let mut map = HotkeyMap::empty();
    let err = map
        .bind(
            Chord::new(cmd_shift(), Key::Media(MediaKey::PlayPause)),
            HotkeyAction::ToggleRecording,
        )
        .expect_err("bind must validate");
    assert!(matches!(
        err,
        HotkeyError::MediaKeyNeedsAccessibility { .. }
    ));
    assert!(map.is_empty(), "a rejected bind must not be recorded");
}

#[test]
fn a_bare_key_is_rejected() {
    for key in [
        Key::Letter('r'),
        Key::Digit(1),
        Key::Space,
        Key::Escape,
        Key::Function(1),
        Key::Function(12),
    ] {
        let err = Chord::new(Modifiers::NONE, key)
            .validate()
            .expect_err("a bare global hotkey steals the key from every app");
        assert_eq!(err, HotkeyError::NoModifier { key });
    }
}

#[test]
fn bare_f13_through_f20_are_allowed() {
    for n in 13..=20 {
        Chord::new(Modifiers::NONE, Key::Function(n))
            .validate()
            .expect("F13-F20 exist to be bound bare and are not claimed by the system");
    }
    assert!(Key::Function(13).is_safe_bare());
    assert!(!Key::Function(12).is_safe_bare());
    assert!(!Key::Letter('r').is_safe_bare());
}

#[test]
fn out_of_range_keys_are_rejected() {
    for key in [Key::Digit(10), Key::Function(0), Key::Function(21)] {
        assert_eq!(
            Chord::new(cmd_shift(), key).validate(),
            Err(HotkeyError::InvalidKey { key })
        );
    }
    // An unnormalised letter cannot be mapped to a Code.
    assert_eq!(
        Chord::new(cmd_shift(), Key::Letter('R')).validate(),
        Err(HotkeyError::InvalidKey {
            key: Key::Letter('R')
        })
    );
}

#[test]
fn letter_normalises_case() {
    assert_eq!(Key::letter('R'), Some(Key::Letter('r')));
    assert_eq!(Key::letter('r'), Some(Key::Letter('r')));
    assert_eq!(Key::letter('1'), None);
    assert_eq!(Key::letter('/'), None);
}

#[test]
fn the_shipped_defaults_are_registrable() {
    let map = HotkeyMap::defaults();
    assert_eq!(map.len(), 2);
    for (chord, action) in map.bindings() {
        chord
            .validate()
            .unwrap_or_else(|e| panic!("default {chord} for {action:?} is not registrable: {e}"));
    }
}

#[test]
fn the_shipped_defaults_bind_the_two_actions_we_document() {
    let map = HotkeyMap::defaults();
    let toggle = map
        .chord_for(HotkeyAction::ToggleRecording)
        .expect("toggle must have a default");
    assert_eq!(toggle.to_string(), "⇧⌘R");
    assert_eq!(
        map.action_for(toggle),
        Some(HotkeyAction::ToggleRecording),
        "the lookup must round-trip"
    );

    let notes = map
        .chord_for(HotkeyAction::OpenNotes)
        .expect("notes must have a default");
    assert_eq!(notes.to_string(), "⇧⌘N");

    for action in HotkeyAction::ALL {
        assert!(map.chord_for(action).is_some(), "{action:?} has no default");
    }
}

#[test]
fn an_unbound_chord_resolves_to_nothing() {
    let map = HotkeyMap::defaults();
    assert_eq!(
        map.action_for(Chord::new(Modifiers::CONTROL, Key::Letter('q'))),
        None
    );
}

#[test]
fn a_duplicate_chord_is_refused() {
    let mut map = HotkeyMap::empty();
    let chord = Chord::new(cmd_shift(), Key::Letter('r'));
    map.bind(chord, HotkeyAction::ToggleRecording).unwrap();

    let err = map
        .bind(chord, HotkeyAction::OpenNotes)
        .expect_err("one action per chord");
    assert_eq!(
        err,
        HotkeyError::DuplicateChord {
            chord,
            existing: HotkeyAction::ToggleRecording
        }
    );
    assert_eq!(map.len(), 1);
}

#[test]
fn a_duplicate_action_is_refused() {
    let mut map = HotkeyMap::empty();
    let first = Chord::new(cmd_shift(), Key::Letter('r'));
    map.bind(first, HotkeyAction::ToggleRecording).unwrap();

    let err = map
        .bind(
            Chord::new(Modifiers::CONTROL, Key::Letter('r')),
            HotkeyAction::ToggleRecording,
        )
        .expect_err("one chord per action");
    assert_eq!(
        err,
        HotkeyError::DuplicateAction {
            action: HotkeyAction::ToggleRecording,
            existing: first
        }
    );
}

#[test]
fn modifiers_render_in_apples_order() {
    let all = Modifiers::COMMAND | Modifiers::SHIFT | Modifiers::OPTION | Modifiers::CONTROL;
    assert_eq!(all.to_string(), "⌃⌥⇧⌘");
    assert_eq!(Modifiers::NONE.to_string(), "");
    assert_eq!(
        Chord::new(Modifiers::CONTROL | Modifiers::OPTION, Key::Slash).to_string(),
        "⌃⌥/"
    );
    assert_eq!(
        Chord::new(Modifiers::COMMAND, Key::Function(5)).to_string(),
        "⌘F5"
    );
}

#[test]
fn modifier_bits_do_not_collide() {
    let each = [
        Modifiers::CONTROL,
        Modifiers::OPTION,
        Modifiers::SHIFT,
        Modifiers::COMMAND,
    ];
    for (i, a) in each.iter().enumerate() {
        assert!(!a.is_empty());
        for (j, b) in each.iter().enumerate() {
            assert_eq!(i == j, a.contains(*b), "modifier bits overlap");
        }
    }
    assert!(Modifiers::NONE.is_empty());
    assert!(cmd_shift().contains(Modifiers::COMMAND));
    assert!(cmd_shift().contains(Modifiers::SHIFT));
    assert!(!cmd_shift().contains(Modifiers::CONTROL));
}
