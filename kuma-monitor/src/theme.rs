//! The session's palette: the slots trim to what a monitor needs, the
//! handshake struct, the color space they derive from, and the slot
//! machinery live in kuma-platform (an adapted copy no more).

use std::sync::atomic::{AtomicU32, Ordering};

use gpui::{Rgba, rgba};
use kuma_platform::palette::{Palette, Slot};

// Every color is a static so the palette can swap live: the shell's
// palette file applies through `apply_palette` and the next render
// picks it up, no restart, no gpui global plumbing. The slot
// machinery is the platform's; the set (and the window opacity the
// mass colors ride) is ours.

static BG: Slot = Slot::new(0x14161a);
static ROW_SELECTED: Slot = Slot::new(0x2d4260);
static BORDER: Slot = Slot::new(0x2a2f3a);
static TEXT: Slot = Slot::new(0xd8dce3);
static TEXT_DIM: Slot = Slot::new(0x8a93a3);
static ACCENT: Slot = Slot::new(0x4f8cc9);
static SURFACE: Slot = Slot::new(0x1b1e24);
static INSET: Slot = Slot::new(0x14161a);
static HOVER: Slot = Slot::new(0x272d38);

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

/// A mass color: the palette value under the window's current
/// opacity. Only the window's mass fades (bg, surface, inset, row
/// selection); ink (text, accent, borders) stays opaque over it.
fn fading(slot: &Slot) -> Rgba {
    slot.faded(OPACITY.load(Ordering::Relaxed))
}

pub(crate) fn bg() -> Rgba {
    fading(&BG)
}
pub(crate) fn row_selected() -> Rgba {
    fading(&ROW_SELECTED)
}
pub(crate) fn border() -> Rgba {
    BORDER.get()
}
pub(crate) fn text() -> Rgba {
    TEXT.get()
}
pub(crate) fn text_dim() -> Rgba {
    TEXT_DIM.get()
}
pub(crate) fn accent() -> Rgba {
    ACCENT.get()
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
    ACCENT.faded(0x66)
}
pub(crate) fn clear() -> Rgba {
    rgba(0x00000000)
}

/// Swap the chrome for the shell's palette wholesale: the session's
/// own look, so the monitor belongs to it. The mapping follows what
/// each slot means: the window backing is the shell's darkest inset,
/// table rows are cards, the focused row wears the selection color.
pub(crate) fn apply_palette(palette: &Palette) {
    BG.set(palette.inset);
    SURFACE.set(palette.surface);
    HOVER.set(palette.surface_hover);
    BORDER.set(palette.divider);
    TEXT.set(palette.text);
    TEXT_DIM.set(palette.text_dim);
    set_accent(palette.accent);
}

/// Put the selection color in the accent's hue. The coefficients are
/// the relationship today's blues have to today's accent, so the
/// default look survives the refactor untouched; the HSL derivation
/// itself is the platform's (one home, no drift).
pub(crate) fn set_accent(hex: u32) {
    ACCENT.set(hex);
    ROW_SELECTED.set(kuma_platform::palette::derive_accent(hex, 0.68, 0.276));
}
