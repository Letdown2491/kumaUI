//! Terminal palette: the xterm 256 default table plus runtime overrides.
//!
//! The emulator stores per-cell colors as indexed or named references and
//! keeps an override table (OSC 4/10/11) inside the term. Resolving all of
//! that to plain RGB happens here so the rest of kuma-term never touches the
//! emulator's color types. The kumaOS theme replaces the constants later;
//! a config file overrides them on other distros.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

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

// Placeholder kuma theme values. On kumaOS these become the shell's palette;
// standalone they are the defaults a config file would override.
const FOREGROUND: Rgb8 = Rgb8(224, 226, 228);
const BACKGROUND: Rgb8 = Rgb8(24, 26, 31);

pub fn default_fg() -> Rgb8 {
    FOREGROUND
}

pub fn default_bg() -> Rgb8 {
    BACKGROUND
}

fn rgb_of(c: Rgb) -> Rgb8 {
    Rgb8(c.r, c.g, c.b)
}

/// The 16 ANSI colors (xterm's classic values).
const NAMED: [Rgb8; 16] = [
    Rgb8(0, 0, 0),
    Rgb8(205, 0, 0),
    Rgb8(0, 205, 0),
    Rgb8(205, 205, 0),
    Rgb8(0, 0, 238),
    Rgb8(205, 0, 205),
    Rgb8(0, 205, 205),
    Rgb8(229, 229, 229),
    Rgb8(127, 127, 127),
    Rgb8(255, 0, 0),
    Rgb8(0, 255, 0),
    Rgb8(255, 255, 0),
    Rgb8(92, 92, 255),
    Rgb8(255, 0, 255),
    Rgb8(0, 255, 255),
    Rgb8(255, 255, 255),
];

/// The 6x6x6 color cube and grayscale ramp (indices 16..256).
pub fn indexed_color(index: u8) -> Rgb8 {
    match index {
        0..=15 => NAMED[index as usize],
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

fn named_color(nc: NamedColor) -> Rgb8 {
    match nc {
        NamedColor::Black => NAMED[0],
        NamedColor::Red => NAMED[1],
        NamedColor::Green => NAMED[2],
        NamedColor::Yellow => NAMED[3],
        NamedColor::Blue => NAMED[4],
        NamedColor::Magenta => NAMED[5],
        NamedColor::Cyan => NAMED[6],
        NamedColor::White => NAMED[7],
        NamedColor::BrightBlack => NAMED[8],
        NamedColor::BrightRed => NAMED[9],
        NamedColor::BrightGreen => NAMED[10],
        NamedColor::BrightYellow => NAMED[11],
        NamedColor::BrightBlue => NAMED[12],
        NamedColor::BrightMagenta => NAMED[13],
        NamedColor::BrightCyan => NAMED[14],
        NamedColor::BrightWhite => NAMED[15],
        NamedColor::Foreground | NamedColor::Cursor => FOREGROUND,
        NamedColor::Background => BACKGROUND,
        NamedColor::BrightForeground => NAMED[7],
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
                NamedColor::DimBlack => NAMED[0],
                NamedColor::DimRed => NAMED[1],
                NamedColor::DimGreen => NAMED[2],
                NamedColor::DimYellow => NAMED[3],
                NamedColor::DimBlue => NAMED[4],
                NamedColor::DimMagenta => NAMED[5],
                NamedColor::DimCyan => NAMED[6],
                NamedColor::DimForeground => FOREGROUND,
                _ => NAMED[7],
            };
            base.scale(2.0 / 3.0)
        }
    }
}

/// Resolve a cell color: the term's override table first (OSC 4/10/11 writes
/// land there), the default table underneath.
pub fn resolve(color: Color, overrides: &Colors) -> Rgb8 {
    match color {
        Color::Named(nc) => {
            overrides[nc].map(rgb_of).unwrap_or_else(|| named_color(nc))
        }
        Color::Indexed(i) => {
            overrides[i as usize].map(rgb_of).unwrap_or_else(|| indexed_color(i))
        }
        // truecolor (SGR 38;2): the color rides in the sequence itself
        Color::Spec(rgb) => rgb_of(rgb),
    }
}

/// Resolve a raw palette index (0..269) for OSC 4 replies: same lookup order
/// as `resolve`, with the special indices 256+ (fg, bg, cursor, dims).
pub fn resolve_index(index: usize, overrides: &Colors) -> Rgb8 {
    if index < 256 {
        indexed_color(index as u8)
    } else {
        match index {
            256 => overrides[NamedColor::Foreground].map(rgb_of).unwrap_or(FOREGROUND),
            257 => overrides[NamedColor::Background].map(rgb_of).unwrap_or(BACKGROUND),
            258 => overrides[NamedColor::Cursor].map(rgb_of).unwrap_or(FOREGROUND),
            259..=266 => overrides[index].map(rgb_of).unwrap_or_else(|| {
                NAMED[(index - 259) as usize].scale(2.0 / 3.0)
            }),
            267 => overrides[NamedColor::BrightForeground].map(rgb_of).unwrap_or(NAMED[7]),
            _ => overrides[index].map(rgb_of).unwrap_or(BACKGROUND),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_matches_xterm_reference_values() {
        // spot checks against xterm's documented cube
        assert_eq!(indexed_color(16), Rgb8(0, 0, 0));
        assert_eq!(indexed_color(196), Rgb8(255, 0, 0));
        assert_eq!(indexed_color(46), Rgb8(0, 255, 0));
        assert_eq!(indexed_color(21), Rgb8(0, 0, 255));
        // grayscale ramp: 232 is 8,8,8; 255 is 238
        assert_eq!(indexed_color(232), Rgb8(8, 8, 8));
        assert_eq!(indexed_color(255), Rgb8(238, 238, 238));
    }

    #[test]
    fn overrides_win_over_defaults() {
        let mut overrides = Colors::default();
        overrides[NamedColor::Foreground] = Some(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(resolve(Color::Named(NamedColor::Foreground), &overrides), Rgb8(1, 2, 3));
        overrides[42] = Some(Rgb { r: 9, g: 8, b: 7 });
        assert_eq!(resolve(Color::Indexed(42), &overrides), Rgb8(9, 8, 7));
        assert_eq!(resolve(Color::Indexed(41), &overrides), indexed_color(41));
    }
}
