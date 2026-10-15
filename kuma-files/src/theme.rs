use std::sync::atomic::{AtomicU32, Ordering};

use gpui::{Rgba, rgb, rgba};
use kuma_platform::{color, palette::Palette};

// Every color is a static so the palette can swap live: the wallpaper
// path (or a picked accent) calls `apply_palette`/`set_accent` and the
// next render picks it up, no restart, no gpui global plumbing.

static BG: AtomicU32 = AtomicU32::new(0x14161a);
static SIDEBAR: AtomicU32 = AtomicU32::new(0x181b21);
static ROW: AtomicU32 = AtomicU32::new(0x1d2129);
static ROW_HOVER: AtomicU32 = AtomicU32::new(0x272d38);
static ROW_SELECTED: AtomicU32 = AtomicU32::new(0x2d4260);
static DRAG_OVER: AtomicU32 = AtomicU32::new(0x3a4a5f);
static BORDER: AtomicU32 = AtomicU32::new(0x2a2f3a);
static TEXT: AtomicU32 = AtomicU32::new(0xd8dce3);
static TEXT_DIM: AtomicU32 = AtomicU32::new(0x8a93a3);
static ACCENT: AtomicU32 = AtomicU32::new(0x4f8cc9);
static ERROR: AtomicU32 = AtomicU32::new(0xc96a4f);

fn get(slot: &AtomicU32) -> Rgba {
    rgb(slot.load(Ordering::Relaxed))
}

pub(crate) fn bg() -> Rgba {
    get(&BG)
}
pub(crate) fn sidebar() -> Rgba {
    get(&SIDEBAR)
}
pub(crate) fn row() -> Rgba {
    get(&ROW)
}
pub(crate) fn row_hover() -> Rgba {
    get(&ROW_HOVER)
}
pub(crate) fn row_selected() -> Rgba {
    get(&ROW_SELECTED)
}
pub(crate) fn drag_over() -> Rgba {
    get(&DRAG_OVER)
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
pub(crate) fn ghost() -> Rgba {
    rgba((ACCENT.load(Ordering::Relaxed) << 8) | 0xCC)
}
pub(crate) fn error() -> Rgba {
    get(&ERROR)
}
pub(crate) fn clear() -> Rgba {
    rgba(0x00000000)
}
/// The rubber selection band fill: a whisper of accent over the grid.
pub(crate) fn rubber_band() -> Rgba {
    rgba((derive(&ACCENT.load(Ordering::Relaxed), 1.76, 0.706) << 8) | 0x26)
}

/// Test-only peek at the raw selection color (unit tests assert the
/// accent-driven hue); the bin build never calls it.
#[cfg(test)]
pub(crate) fn row_selected_hex() -> u32 {
    ROW_SELECTED.load(Ordering::Relaxed)
}

/// Swap the chrome for a wallpaper palette (kuma-shell) wholesale:
/// the session's own look, so the file manager belongs to it. The
/// mapping follows what each slot means: our window backing is the
/// shell's darkest inset, the places rail mirrors the bar, listing
/// rows are cards.
pub(crate) fn apply_palette(palette: &Palette) {
    BG.store(palette.inset, Ordering::Relaxed);
    SIDEBAR.store(palette.panel_bg & 0xFFFFFF, Ordering::Relaxed);
    ROW.store(palette.surface, Ordering::Relaxed);
    ROW_HOVER.store(palette.surface_hover, Ordering::Relaxed);
    BORDER.store(palette.divider, Ordering::Relaxed);
    TEXT.store(palette.text, Ordering::Relaxed);
    TEXT_DIM.store(palette.text_dim, Ordering::Relaxed);
    set_accent(palette.accent);
}

/// Put every selection-tinted color in the accent's hue. The
/// coefficients are the relationship today's blues have to today's
/// accent, so the default look survives the refactor untouched.
pub(crate) fn set_accent(hex: u32) {
    ACCENT.store(hex, Ordering::Relaxed);
    ROW_SELECTED.store(derive(&hex, 0.68, 0.276), Ordering::Relaxed);
    DRAG_OVER.store(derive(&hex, 0.456, 0.300), Ordering::Relaxed);
}

/// A low-saturation tone in the accent's hue at lightness `l`.
fn derive(accent: &u32, sat_scale: f32, l: f32) -> u32 {
    let (h, s, _) = color::rgb_to_hsl(*accent);
    color::hsl_to_rgb(h, (s * sat_scale).clamp(0.0, 0.95), l)
}
