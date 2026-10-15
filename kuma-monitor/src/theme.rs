//! The spike's palette: an adapted copy of kuma-files/src/theme.rs
//! fed by the shell's published palette file the same way kuma-files
//! is. The slots trim to what a monitor needs; the handshake struct
//! and the color space they derive from live in kuma-platform.

use std::sync::atomic::{AtomicU32, Ordering};

use gpui::{Rgba, rgb, rgba};
use kuma_platform::{color, palette::Palette};

// Every color is a static so the palette can swap live: the shell's
// palette file applies through `apply_palette` and the next render
// picks it up, no restart, no gpui global plumbing.

static BG: AtomicU32 = AtomicU32::new(0x14161a);
static ROW_SELECTED: AtomicU32 = AtomicU32::new(0x2d4260);
static BORDER: AtomicU32 = AtomicU32::new(0x2a2f3a);
static TEXT: AtomicU32 = AtomicU32::new(0xd8dce3);
static TEXT_DIM: AtomicU32 = AtomicU32::new(0x8a93a3);
static ACCENT: AtomicU32 = AtomicU32::new(0x4f8cc9);
static SURFACE: AtomicU32 = AtomicU32::new(0x1b1e24);
static INSET: AtomicU32 = AtomicU32::new(0x14161a);
static HOVER: AtomicU32 = AtomicU32::new(0x272d38);

/// The window's mass fades to the terminal's 80/90: focused shows the
/// most wallpaper, unfocused dims toward solid for contrast. Stored
/// as the alpha byte the mass colors get OR'd in with, so the palette
/// slots keep holding plain RGB.
static OPACITY: AtomicU32 = AtomicU32::new(0xFF);

/// Background opacity with the window focused (kuma-term's number).
pub(crate) const BG_OPACITY_FOCUSED: f32 = 0.80;
/// Background opacity with the window unfocused (kuma-term's number).
pub(crate) const BG_OPACITY_UNFOCUSED: f32 = 0.90;

pub(crate) fn set_window_opacity(opacity: f32) {
    OPACITY.store((((opacity * 255.) + 0.5) as u32) & 0xFF, Ordering::Relaxed);
}

fn get(slot: &AtomicU32) -> Rgba {
    rgb(slot.load(Ordering::Relaxed))
}

/// A mass color: the palette value under the window's current
/// opacity. Only the window's mass fades (bg, surface, inset, row
/// selection); ink (text, accent, borders) stays opaque over it.
fn fading(slot: &AtomicU32) -> Rgba {
    rgba((slot.load(Ordering::Relaxed) << 8) | OPACITY.load(Ordering::Relaxed))
}

pub(crate) fn bg() -> Rgba {
    fading(&BG)
}
pub(crate) fn row_selected() -> Rgba {
    fading(&ROW_SELECTED)
}
pub(crate) fn border() -> Rgba {
    get(&BORDER)
}
pub(crate) fn text() -> Rgba {
    get(&TEXT)
}
pub(crate) fn text_dim() -> Rgba {
    get(&TEXT_DIM)
}
pub(crate) fn accent() -> Rgba {
    get(&ACCENT)
}
/// The card fill, one step above the background.
pub(crate) fn surface() -> Rgba {
    fading(&SURFACE)
}
/// The hovered row's lift, the shell's own surface_hover: a mass
/// color, faded with the window like the rest of the floors.
pub(crate) fn row_hover() -> Rgba {
    fading(&HOVER)
}
/// The well a fill grows out of (the memory bar's track).
pub(crate) fn inset() -> Rgba {
    fading(&INSET)
}
/// The accent at a whisper: the graph's history fill, loud only as a
/// shape, not as a color.
pub(crate) fn ghost() -> Rgba {
    rgba((ACCENT.load(Ordering::Relaxed) << 8) | 0x66)
}
pub(crate) fn clear() -> Rgba {
    rgba(0x00000000)
}

/// Swap the chrome for the shell's palette wholesale: the session's
/// own look, so the monitor belongs to it. The mapping follows what
/// each slot means: the window backing is the shell's darkest inset,
/// table rows are cards, the focused row wears the selection color.
pub(crate) fn apply_palette(palette: &Palette) {
    BG.store(palette.inset, Ordering::Relaxed);
    SURFACE.store(palette.surface, Ordering::Relaxed);
    HOVER.store(palette.surface_hover, Ordering::Relaxed);
    BORDER.store(palette.divider, Ordering::Relaxed);
    TEXT.store(palette.text, Ordering::Relaxed);
    TEXT_DIM.store(palette.text_dim, Ordering::Relaxed);
    set_accent(palette.accent);
}

/// Put the selection color in the accent's hue. The coefficients are
/// the relationship today's blues have to today's accent, so the
/// default look survives the refactor untouched.
pub(crate) fn set_accent(hex: u32) {
    ACCENT.store(hex, Ordering::Relaxed);
    ROW_SELECTED.store(derive(&hex, 0.68, 0.276), Ordering::Relaxed);
}

/// A low-saturation tone in the accent's hue at lightness `l`.
fn derive(accent: &u32, sat_scale: f32, l: f32) -> u32 {
    let (h, s, _) = color::rgb_to_hsl(*accent);
    color::hsl_to_rgb(h, (s * sat_scale).clamp(0.0, 0.95), l)
}
