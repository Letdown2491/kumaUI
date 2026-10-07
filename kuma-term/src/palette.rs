//! Terminal palette: the theme's 16-color table plus runtime overrides.
//!
//! The emulator stores per-cell colors as indexed or named references and
//! keeps an override table (OSC 4/10/11) inside the term. Resolving all of
//! that to plain RGB happens here so the rest of kuma-term never touches
//! the emulator's color types.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use crate::theme::Theme;

/// A resolved RGB triplet. Plain data on purpose: this is what crosses the
/// engine seam into the renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb8(pub u8, pub u8, pub u8);

impl Rgb8 {
    pub fn scale(self, f: f32) -> Self {
        Self(
            (self.0 as f32 * f).round() as u8,
            (self.1 as f32 * f).round() as u8,
            (self.2 as f32 * f).round() as u8,
        )
    }
}

fn rgb_of(c: Rgb) -> Rgb8 {
    Rgb8(c.r, c.g, c.b)
}

/// The 6x6x6 color cube and grayscale ramp (indices 16..256).
pub fn indexed_color(index: u8, theme: &Theme) -> Rgb8 {
    match index {
        0..=15 => theme.named[index as usize],
        16..=231 => {
            let c = index as usize - 16;
            let ch = |v: usize| if v == 0 { 0 } else { 55 + 40 * v } as u8;
            Rgb8(ch(c / 36), ch((c / 6) % 6), ch(c % 6))
        }
        _ => {
            let v = 8 + 10 * (index - 232) as i32;
            Rgb8(v as u8, v as u8, v as u8)
        }
    }
}

fn named_color(nc: NamedColor, theme: &Theme) -> Rgb8 {
    match nc {
        NamedColor::Black
        | NamedColor::Red
        | NamedColor::Green
        | NamedColor::Yellow
        | NamedColor::Blue
        | NamedColor::Magenta
        | NamedColor::Cyan
        | NamedColor::White
        | NamedColor::BrightBlack
        | NamedColor::BrightRed
        | NamedColor::BrightGreen
        | NamedColor::BrightYellow
        | NamedColor::BrightBlue
        | NamedColor::BrightMagenta
        | NamedColor::BrightCyan
        | NamedColor::BrightWhite => theme.named[nc as usize],
        NamedColor::Foreground | NamedColor::Cursor => theme.foreground,
        NamedColor::Background => theme.background,
        NamedColor::BrightForeground => theme.foreground,
        nc @ (NamedColor::DimBlack
        | NamedColor::DimRed
        | NamedColor::DimGreen
        | NamedColor::DimYellow
        | NamedColor::DimBlue
        | NamedColor::DimMagenta
        | NamedColor::DimCyan
        | NamedColor::DimWhite
        | NamedColor::DimForeground) => {
            // dim variants are their base color at two thirds intensity
            let base = match nc {
                NamedColor::DimBlack => theme.named[0],
                NamedColor::DimRed => theme.named[1],
                NamedColor::DimGreen => theme.named[2],
                NamedColor::DimYellow => theme.named[3],
                NamedColor::DimBlue => theme.named[4],
                NamedColor::DimMagenta => theme.named[5],
                NamedColor::DimCyan => theme.named[6],
                NamedColor::DimForeground => theme.foreground,
                _ => theme.named[7],
            };
            base.scale(2.0 / 3.0)
        }
    }
}

/// Resolve a cell color: the term's override table first (OSC 4/10/11
/// writes land there), the theme underneath.
pub fn resolve(color: Color, overrides: &Colors, theme: &Theme) -> Rgb8 {
    match color {
        Color::Named(nc) => overrides[nc].map(rgb_of).unwrap_or_else(|| named_color(nc, theme)),
        Color::Indexed(i) => overrides[i as usize].map(rgb_of).unwrap_or_else(|| indexed_color(i, theme)),
        // truecolor (SGR 38;2): the color rides in the sequence itself
        Color::Spec(rgb) => rgb_of(rgb),
    }
}

/// Resolve a raw palette index (0..269) for OSC 4 replies: same lookup
/// order as `resolve`, with the special indices 256+ (fg, bg, cursor, dims).
pub fn resolve_index(index: usize, overrides: &Colors, theme: &Theme) -> Rgb8 {
    if index < 256 {
        indexed_color(index as u8, theme)
    } else {
        match index {
            256 => overrides[NamedColor::Foreground].map(rgb_of).unwrap_or(theme.foreground),
            257 => overrides[NamedColor::Background].map(rgb_of).unwrap_or(theme.background),
            258 => overrides[NamedColor::Cursor].map(rgb_of).unwrap_or(theme.cursor),
            259..=266 => overrides[index].map(rgb_of).unwrap_or_else(|| {
                theme.named[(index - 259) as usize].scale(2.0 / 3.0)
            }),
            267 => overrides[NamedColor::BrightForeground].map(rgb_of).unwrap_or(theme.foreground),
            _ => overrides[index].map(rgb_of).unwrap_or(theme.background),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn cube_matches_xterm_reference_values() {
        let theme = Theme::builtin();
        // spot checks against xterm's documented cube
        assert_eq!(indexed_color(16, &theme), Rgb8(0, 0, 0));
        assert_eq!(indexed_color(196, &theme), Rgb8(255, 0, 0));
        assert_eq!(indexed_color(46, &theme), Rgb8(0, 255, 0));
        assert_eq!(indexed_color(21, &theme), Rgb8(0, 0, 255));
        // grayscale ramp: 232 is 8,8,8; 255 is 238
        assert_eq!(indexed_color(232, &theme), Rgb8(8, 8, 8));
        assert_eq!(indexed_color(255, &theme), Rgb8(238, 238, 238));
    }

    #[test]
    fn overrides_win_over_defaults() {
        let theme = Theme::builtin();
        let mut overrides = Colors::default();
        overrides[NamedColor::Foreground] = Some(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(resolve(Color::Named(NamedColor::Foreground), &overrides, &theme), Rgb8(1, 2, 3));
        overrides[42] = Some(Rgb { r: 9, g: 8, b: 7 });
        assert_eq!(resolve(Color::Indexed(42), &overrides, &theme), Rgb8(9, 8, 7));
        assert_eq!(resolve(Color::Indexed(41), &overrides, &theme), indexed_color(41, &theme));
    }
}
