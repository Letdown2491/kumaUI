//! The single source for colors and shared dimensions.

/// The whole palette in one struct: the consts below are the default
/// (Catppuccin Mocha-flavored), and wallpaper-derived theming swaps
/// this struct wholesale when a new palette is generated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
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

impl Default for Theme {
    fn default() -> Self {
        Theme {
            panel_bg: PANEL_BG,
            surface: SURFACE,
            surface_hover: SURFACE_HOVER,
            inset: INSET,
            divider: DIVIDER,
            divider_soft: DIVIDER_SOFT,
            text: TEXT,
            text_dim: TEXT_DIM,
            accent: ACCENT,
            accent_text: ACCENT_TEXT,
        }
    }
}

/// The live palette. A `RwLock` rather than a `OnceLock`: the derived
/// theme swaps when the wallpaper changes, and reads happen on every
/// render, so it must be swappable and cheap to copy out.
static THEME: std::sync::RwLock<Theme> = std::sync::RwLock::new(Theme::new());

impl Theme {
    const fn new() -> Self {
        Theme {
            panel_bg: PANEL_BG,
            surface: SURFACE,
            surface_hover: SURFACE_HOVER,
            inset: INSET,
            divider: DIVIDER,
            divider_soft: DIVIDER_SOFT,
            text: TEXT,
            text_dim: TEXT_DIM,
            accent: ACCENT,
            accent_text: ACCENT_TEXT,
        }
    }
}

/// The palette to render with: a copy, so the lock is never held. A
/// poisoned lock (a panic mid-swap) still yields the last good value.
pub fn current() -> Theme {
    *THEME
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Swap the live palette (wallpaper-derived theming).
pub fn set_current(theme: Theme) {
    *THEME
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = theme;
}

impl Theme {
    /// Replace just the accent (the near-grey fallback keeps the
    /// recognizable blue but still coordinates).
    pub fn with_accent(mut self, accent: u32) -> Self {
        self.accent = accent;
        self
    }
}

pub const PANEL_BG: u32 = 0x181825F2;
pub const SURFACE: u32 = 0x313244;
pub const SURFACE_HOVER: u32 = 0x45475A;
pub const INSET: u32 = 0x11111B;
pub const DIVIDER: u32 = 0x45475A;
pub const DIVIDER_SOFT: u32 = 0x45475A66;
pub const TEXT: u32 = 0xCDD6F4;
pub const TEXT_DIM: u32 = 0x6C7086;
pub const ACCENT: u32 = 0x89B4FA;
pub const ACCENT_TEXT: u32 = 0x11111B;
pub const URGENT: u32 = 0xF38BA8;

pub const TEXT_SIZE: f32 = 12.;
pub const TEXT_SIZE_SMALL: f32 = 11.;
pub const TEXT_SIZE_LABEL: f32 = 10.;
pub const ICON_SIZE: f32 = 14.;

pub const SOFT_DIVIDER: u32 = 0x45475A66;
