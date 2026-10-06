use std::sync::atomic::{AtomicU32, Ordering};

use gpui::{Rgba, rgb, rgba};

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

/// The colors a wallpaper palette hands over; unset fields keep the
/// built-ins. Mirrors kuma-shell's `theme::Theme` field for field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Palette {
    pub panel_bg: u32,
    pub surface: u32,
    pub surface_hover: u32,
    pub inset: u32,
    pub divider: u32,
    pub divider_soft: u32,
    pub text: u32,
    pub text_dim: u32,
    pub accent: u32,
    pub accent_text: u32,
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            panel_bg: 0x181b21,
            surface: 0x1d2129,
            surface_hover: 0x272d38,
            inset: 0x14161a,
            divider: 0x2a2f3a,
            divider_soft: 0x2a2f3a,
            text: 0xd8dce3,
            text_dim: 0x8a93a3,
            accent: 0x4f8cc9,
            accent_text: 0xffffff,
        }
    }
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
    let (h, s, _) = rgb_to_hsl(*accent);
    hsl_to_rgb(h, (s * sat_scale).clamp(0.0, 0.95), l)
}

// ---------- color space (from kuma-shell/src/palette.rs) ----------

/// sRGB u32 (0xRRGGBB) to HSL.
pub(crate) fn rgb_to_hsl(rgb: u32) -> (f32, f32, f32) {
    let r = ((rgb >> 16) & 0xFF) as f32 / 255.;
    let g = ((rgb >> 8) & 0xFF) as f32 / 255.;
    let b = (rgb & 0xFF) as f32 / 255.;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.;
    if max == min {
        return (0., 0., l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2. - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6. } else { 0. }
    } else if max == g {
        (b - r) / d + 2.
    } else {
        (r - g) / d + 4.
    } * 60.;
    (h, s, l)
}

/// HSL to sRGB u32 (0xRRGGBB), clamped.
pub(crate) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> u32 {
    let h = h.rem_euclid(360.) / 360.;
    let s = s.clamp(0., 1.);
    let l = l.clamp(0., 1.);
    if s == 0. {
        let v = (l * 255. + 0.5) as u32;
        return (v << 16) | (v << 8) | v;
    }
    let q = if l < 0.5 { l * (1. + s) } else { l + s - l * s };
    let p = 2. * l - q;
    let channel = |mut t: f32| {
        t = t.rem_euclid(1.);
        let v = if t < 1. / 6. {
            p + (q - p) * 6. * t
        } else if t < 0.5 {
            q
        } else if t < 2. / 3. {
            p + (q - p) * (2. / 3. - t) * 6.
        } else {
            p
        };
        (v * 255. + 0.5) as u32
    };
    (channel(h + 1. / 3.) << 16) | (channel(h) << 8) | channel(h - 1. / 3.)
}
