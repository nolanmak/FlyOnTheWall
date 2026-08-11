//! The menu-bar icon.
//!
//! CON-02 requires the menu-bar item to be "in a distinct state" while
//! recording. Because the icons are rasterised in Rust rather than loaded
//! from a `.png`, that is a property a Linux runner can check instead of a
//! claim about an asset nobody diffed.

use fotw_shell::TrayState;
use fotw_shell::icon::{ICON_PIXELS, ICON_POINTS, is_template, tray_icon_rgba};

const ALL_STATES: [TrayState; 4] = [
    TrayState::Idle,
    TrayState::Recording,
    TrayState::Finishing,
    TrayState::Fault,
];

fn pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * ICON_PIXELS + x) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

fn centre(rgba: &[u8]) -> [u8; 4] {
    pixel(rgba, ICON_PIXELS / 2, ICON_PIXELS / 2)
}

#[test]
fn every_icon_is_the_size_the_status_item_expects() {
    assert_eq!(ICON_PIXELS, ICON_POINTS * 2, "menu-bar icons are 22pt @2x");
    for state in ALL_STATES {
        let rgba = tray_icon_rgba(state);
        assert_eq!(
            rgba.len(),
            (ICON_PIXELS * ICON_PIXELS * 4) as usize,
            "{state:?} raster is the wrong length; Icon::from_rgba would reject it"
        );
    }
}

#[test]
fn idle_and_recording_are_visually_distinct() {
    let idle = tray_icon_rgba(TrayState::Idle);
    let recording = tray_icon_rgba(TrayState::Recording);
    assert_ne!(
        idle, recording,
        "CON-02 requires a distinct recording state"
    );

    // Not distinct by a stray pixel: the centre of the glyph is the whole
    // difference between a ring and a disc.
    assert_eq!(
        centre(&idle)[3],
        0,
        "idle is a ring, so its centre is transparent"
    );
    assert_eq!(
        centre(&recording)[3],
        255,
        "recording is a filled disc, so its centre is opaque"
    );
}

#[test]
fn the_recording_icon_is_red_and_is_not_a_template() {
    let recording = tray_icon_rgba(TrayState::Recording);
    let [r, g, b, _] = centre(&recording);
    assert!(
        r > 150 && r > g.saturating_mul(2) && r > b.saturating_mul(2),
        "recording must be unmistakably red, got ({r}, {g}, {b})"
    );

    assert!(
        !is_template(TrayState::Recording),
        "a template image is recoloured to match the menu bar; the recording \
         state would stop being red the moment the user switched appearance"
    );
    assert!(
        is_template(TrayState::Idle),
        "the idle ring must follow the menu bar's appearance"
    );
}

#[test]
fn a_fault_does_not_look_like_a_live_recording() {
    let recording = tray_icon_rgba(TrayState::Recording);
    let fault = tray_icon_rgba(TrayState::Fault);
    assert_ne!(recording, fault);
    assert_eq!(
        centre(&fault)[3],
        0,
        "a fault is a ring: a filled disc would read as a session still running"
    );
}

#[test]
fn finishing_still_reads_as_recording() {
    // Capture has stopped but bytes are still being written, so the menu bar
    // must not go quiet yet.
    let finishing = tray_icon_rgba(TrayState::Finishing);
    assert_eq!(centre(&finishing)[3], 255);
    assert_ne!(finishing, tray_icon_rgba(TrayState::Idle));
}

#[test]
fn every_icon_leaves_the_menu_bar_margin_clear() {
    for state in ALL_STATES {
        let rgba = tray_icon_rgba(state);
        for (x, y) in [
            (0, 0),
            (ICON_PIXELS - 1, 0),
            (0, ICON_PIXELS - 1),
            (ICON_PIXELS - 1, ICON_PIXELS - 1),
        ] {
            assert_eq!(
                pixel(&rgba, x, y)[3],
                0,
                "{state:?} paints into the corner at ({x}, {y}); the menu bar \
                 crops that and the glyph would look clipped"
            );
        }
    }
}

#[test]
fn the_glyph_is_antialiased_rather_than_hard_edged() {
    // A ring drawn with no coverage sampling has only 0 and 255 alphas and
    // looks visibly jagged at 22pt. Assert some partial coverage exists.
    let idle = tray_icon_rgba(TrayState::Idle);
    let partial = idle
        .chunks_exact(4)
        .filter(|px| px[3] > 0 && px[3] < 255)
        .count();
    assert!(
        partial > 40,
        "expected antialiased edges, found only {partial} partial pixels"
    );
}

#[test]
fn every_state_draws_something() {
    for state in ALL_STATES {
        let rgba = tray_icon_rgba(state);
        let opaque = rgba.chunks_exact(4).filter(|px| px[3] == 255).count();
        assert!(
            opaque > 100,
            "{state:?} is nearly empty ({opaque} opaque pixels); an invisible \
             menu-bar item is indistinguishable from a failed launch"
        );
    }
}
