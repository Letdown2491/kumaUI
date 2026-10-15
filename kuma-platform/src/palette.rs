//! The shell's palette handshake. kuma-shell publishes the session's
//! colors to `$XDG_RUNTIME_DIR/kuma-shell/palette` (settings.rs's
//! publish_palette, one key=value line per slot), and every app that
//! wants to belong to the session reads that file instead of trusting
//! its own built-ins. The locate, the read, and the parse live here;
//! each app keeps its own theme application, because the slots each
//! app derives from these ten colors are its own.

use std::{
    fs,
    path::PathBuf,
    time::SystemTime,
};

/// The published file's path, or None when XDG_RUNTIME_DIR is unset
/// (no session, no shell: the built-ins stand).
pub fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("kuma-shell/palette"))
}

/// The file's mtime, the handshake's change signal: an app notes the
/// mtime it applied, polls cheaply, and re-applies when it moves.
pub fn mtime() -> Option<SystemTime> {
    fs::metadata(path()?).ok()?.modified().ok()
}

/// The file's text, when published.
pub fn read_text() -> Option<String> {
    fs::read_to_string(path()?).ok()
}

/// The colors a wallpaper palette hands over; unset fields keep the
/// built-ins. Mirrors kuma-shell's `theme::Theme` field for field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
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

/// Read the published palette: None when the shell published nothing
/// (not under kuma-shell, or adaptive off), and the built-ins stand.
pub fn read() -> Option<Palette> {
    read_text().map(|text| parse(&text))
}

/// Parse the palette file: key=value hex lines, unknown or broken
/// lines ignored, missing keys keep the built-ins.
pub fn parse(text: &str) -> Palette {
    let mut palette = Palette::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(hex) = parse_hex_color(value) else {
            continue;
        };
        // the shell's panel colors arrive as RRGGBBAA (alpha in the
        // low byte); app surfaces are opaque, so keep the top 24
        // bits. Pure RGB values pass through untouched.
        let hex = if hex > 0xFFFFFF { hex >> 8 } else { hex };
        match key.trim() {
            "panel_bg" => palette.panel_bg = hex,
            "surface" => palette.surface = hex,
            "surface_hover" => palette.surface_hover = hex,
            "inset" => palette.inset = hex,
            "divider" => palette.divider = hex,
            "divider_soft" => palette.divider_soft = hex,
            "text" => palette.text = hex,
            "text_dim" => palette.text_dim = hex,
            "accent" => palette.accent = hex,
            "accent_text" => palette.accent_text = hex,
            _ => {}
        }
    }
    palette
}

/// A color out of "#rrggbb", "rrggbb", "0xrrggbb", or the 8-digit
/// alpha-carrying form the shell publishes for panel colors.
pub fn parse_hex_color(value: &str) -> Option<u32> {
    let text = value.trim().trim_start_matches('#').trim_start_matches("0x");
    if text.len() != 6 && text.len() != 8 {
        return None;
    }
    if !text.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(text, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_published_lines_and_folds_alpha() {
        let text = concat!(
            "panel_bg=181B21FF\n",
            "surface=#1d2129\n",
            "surface_hover=0x272d38\n",
            "inset=14161aAA\n",
            "divider=2a2f3a\n",
            "divider_soft=2a2f3a\n",
            "text=d8dce3\n",
            "text_dim=8a93a3\n",
            "accent=4f8cc9\n",
            "accent_text=ffffff\n",
        );
        let palette = parse(text);
        // the alpha carriers fold to their top 24 bits
        assert_eq!(palette.panel_bg, 0x181b21);
        assert_eq!(palette.inset, 0x14161a);
        assert_eq!(palette.accent, 0x4f8cc9);
        assert_eq!(palette.text, 0xd8dce3);
    }

    #[test]
    fn broken_lines_and_unknown_keys_keep_the_defaults() {
        let palette = parse("accent=not-a-color\nsomething=123456\nno equals here\n\naccent=\n");
        assert_eq!(palette, Palette::default());
        // a good line among the junk still lands
        let palette = parse("junk\naccent=89b4fa\n");
        assert_eq!(palette.accent, 0x89b4fa);
        assert_eq!(palette.text, Palette::default().text);
    }

    #[test]
    fn hex_colors_take_their_spelled_forms() {
        assert_eq!(parse_hex_color("#4f8cc9"), Some(0x4f8cc9));
        assert_eq!(parse_hex_color("0x4f8cc9"), Some(0x4f8cc9));
        assert_eq!(parse_hex_color("  4F8CC9  "), Some(0x4f8cc9));
        assert_eq!(parse_hex_color("4f8cc9ff"), Some(0x4f8cc9ff));
        assert_eq!(parse_hex_color("4f8"), None);
        assert_eq!(parse_hex_color("4f8ccg"), None);
        assert_eq!(parse_hex_color(""), None);
    }
}
