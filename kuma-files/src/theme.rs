use gpui::Rgba;
use kuma_platform::palette::{Palette, Slot};

// Every color is a static so the palette can swap live: the wallpaper
// path (or a picked accent) calls `apply_palette`/`set_accent` and the
// next render picks it up, no restart, no gpui global plumbing. The
// slot machinery is the platform's; the set is ours.

static BG: Slot = Slot::new(0x14161a);
static SIDEBAR: Slot = Slot::new(0x181b21);
static ROW: Slot = Slot::new(0x1d2129);
static ROW_HOVER: Slot = Slot::new(0x272d38);
static ROW_SELECTED: Slot = Slot::new(0x2d4260);
static DRAG_OVER: Slot = Slot::new(0x3a4a5f);
static BORDER: Slot = Slot::new(0x2a2f3a);
static TEXT: Slot = Slot::new(0xd8dce3);
static TEXT_DIM: Slot = Slot::new(0x8a93a3);
static ACCENT: Slot = Slot::new(0x4f8cc9);
static ERROR: Slot = Slot::new(0xc96a4f);

pub(crate) fn bg() -> Rgba {
    BG.get()
}
pub(crate) fn sidebar() -> Rgba {
    SIDEBAR.get()
}
pub(crate) fn row() -> Rgba {
    ROW.get()
}
pub(crate) fn row_hover() -> Rgba {
    ROW_HOVER.get()
}
pub(crate) fn row_selected() -> Rgba {
    ROW_SELECTED.get()
}
pub(crate) fn drag_over() -> Rgba {
    DRAG_OVER.get()
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
pub(crate) fn ghost() -> Rgba {
    ACCENT.faded(0xCC)
}
pub(crate) fn error() -> Rgba {
    ERROR.get()
}
pub(crate) fn clear() -> Rgba {
    gpui::rgba(0x00000000)
}
/// The rubber selection band fill: a whisper of accent over the grid.
pub(crate) fn rubber_band() -> Rgba {
    gpui::rgba((kuma_platform::palette::derive_accent(ACCENT.hex(), 1.76, 0.706) << 8) | 0x26)
}

/// Test-only peek at the raw selection color (unit tests assert the
/// accent-driven hue); the bin build never calls it.
#[cfg(test)]
pub(crate) fn row_selected_hex() -> u32 {
    ROW_SELECTED.hex()
}

/// Swap the chrome for a wallpaper palette (kuma-shell) wholesale:
/// the session's own look, so the file manager belongs to it. The
/// mapping follows what each slot means: our window backing is the
/// shell's darkest inset, the places rail mirrors the bar, listing
/// rows are cards.
pub(crate) fn apply_palette(palette: &Palette) {
    BG.set(palette.inset);
    SIDEBAR.set(palette.panel_bg & 0xFFFFFF);
    ROW.set(palette.surface);
    ROW_HOVER.set(palette.surface_hover);
    BORDER.set(palette.divider);
    TEXT.set(palette.text);
    TEXT_DIM.set(palette.text_dim);
    set_accent(palette.accent);
}

/// Put every selection-tinted color in the accent's hue. The
/// coefficients are the relationship today's blues have to today's
/// accent, so the default look survives the refactor untouched; the
/// HSL derivation itself is the platform's (one home, no drift).
pub(crate) fn set_accent(hex: u32) {
    ACCENT.set(hex);
    ROW_SELECTED.set(kuma_platform::palette::derive_accent(hex, 0.68, 0.276));
    DRAG_OVER.set(kuma_platform::palette::derive_accent(hex, 0.456, 0.300));
}
