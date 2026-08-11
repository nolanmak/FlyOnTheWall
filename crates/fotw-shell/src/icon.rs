//! The menu-bar icon, rasterised in pure Rust.
//!
//! No asset files and no image decoder: the icons are two circles, and
//! generating them here means the "idle and recording must be visually
//! distinct" half of CON-02 is a property a Linux CI runner can assert,
//! rather than a claim about a `.png` nobody diffed.
//!
//! - **Idle** is a ring, drawn as a *template* image: macOS recolours
//!   template images to match the menu bar (dark, light, and the inverted
//!   state while the menu is open), so only the alpha channel matters and the
//!   colour bytes are zero.
//! - **Recording** is a filled red disc and is **not** a template. It must
//!   stay red when the menu bar is light, when it is dark, and while the menu
//!   is open. That is the whole point of the state being distinct.

use crate::view::TrayState;

/// Points on a side. macOS menu-bar icons are 22×22pt; the renderer sets the
/// `NSImage` size to this and lets the backing scale handle Retina.
pub const ICON_POINTS: u32 = 22;

/// Backing-store scale the raster is generated at.
pub const ICON_SCALE: u32 = 2;

/// Pixels on a side of the generated raster.
pub const ICON_PIXELS: u32 = ICON_POINTS * ICON_SCALE;

/// The recording disc's colour, RGB.
const RECORD_RGB: [u8; 3] = [0xE0, 0x3A, 0x3A];

/// Samples per axis used for antialiasing. 4×4 is enough that a 22pt circle
/// has no visible stair-stepping and cheap enough to run on every icon swap.
const SUPERSAMPLE: u32 = 4;

/// Radius of the glyph as a fraction of the icon's half-width. Leaves the
/// margin the menu bar expects around a 22pt item.
const OUTER: f32 = 0.72;

/// Inner radius of the idle ring, as a fraction of [`OUTER`].
const INNER: f32 = 0.56;

/// Rasterise the menu-bar icon for a given state as straight (non-premultiplied)
/// RGBA8, row-major, [`ICON_PIXELS`] on a side.
///
/// The returned buffer is what `tray_icon::Icon::from_rgba` wants.
#[must_use]
pub fn tray_icon_rgba(state: TrayState) -> Vec<u8> {
    let side = ICON_PIXELS;
    let mut out = vec![0u8; (side * side * 4) as usize];

    // A ring for idle; a filled disc for every state in which something is
    // being captured or has just been captured. Fault reuses the ring so a
    // failed session cannot be mistaken for a live one at a glance.
    let filled = matches!(state, TrayState::Recording | TrayState::Finishing);
    let rgb = match state {
        TrayState::Recording | TrayState::Finishing | TrayState::Fault => RECORD_RGB,
        // Template image: macOS ignores the colour channels entirely.
        TrayState::Idle => [0, 0, 0],
    };

    #[expect(
        clippy::cast_precision_loss,
        reason = "side is 44; every value here is exactly representable"
    )]
    let half = side as f32 / 2.0;
    let outer_r = half * OUTER;
    let inner_r = outer_r * INNER;

    for y in 0..side {
        for x in 0..side {
            let coverage = coverage_at(x, y, half, outer_r, inner_r, filled);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "coverage is in 0..=1 by construction"
            )]
            let alpha = (coverage * 255.0).round().clamp(0.0, 255.0) as u8;
            let i = ((y * side + x) * 4) as usize;
            out[i] = rgb[0];
            out[i + 1] = rgb[1];
            out[i + 2] = rgb[2];
            out[i + 3] = alpha;
        }
    }
    out
}

/// Fraction of pixel `(x, y)` covered by the glyph, by supersampling.
fn coverage_at(x: u32, y: u32, half: f32, outer_r: f32, inner_r: f32, filled: bool) -> f32 {
    let mut hits = 0u32;
    for sy in 0..SUPERSAMPLE {
        for sx in 0..SUPERSAMPLE {
            #[expect(clippy::cast_precision_loss, reason = "coordinates are small integers")]
            let px = (x as f32) + (sx as f32 + 0.5) / SUPERSAMPLE as f32 - half;
            #[expect(clippy::cast_precision_loss, reason = "coordinates are small integers")]
            let py = (y as f32) + (sy as f32 + 0.5) / SUPERSAMPLE as f32 - half;
            let d = px.hypot(py);
            let inside = d <= outer_r && (filled || d >= inner_r);
            if inside {
                hits += 1;
            }
        }
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "hits is at most SUPERSAMPLE^2 = 16"
    )]
    let coverage = hits as f32 / (SUPERSAMPLE * SUPERSAMPLE) as f32;
    coverage
}

/// Whether macOS should recolour this state's icon to match the menu bar.
///
/// Only the idle ring is a template. The recording disc must stay red.
#[must_use]
pub const fn is_template(state: TrayState) -> bool {
    matches!(state, TrayState::Idle)
}
