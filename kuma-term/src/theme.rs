//! The theme: palette and font, read from kitty's config for the spike so
//! the terminal matches the look the user already has. Built-in defaults
//! sit underneath. This is the stand-in for the real config story (kumaOS
//! pushes the shell's palette; other distros get a config file): the
//! parsing shape here carries over, only the source changes.

use std::path::{Path, PathBuf};

use crate::palette::Rgb8;

#[derive(Clone, Debug)]
pub struct Theme {
    /// ANSI colors 0..15.
    pub named: [Rgb8; 16],
    pub foreground: Rgb8,
    pub background: Rgb8,
    pub cursor: Rgb8,
    pub cursor_text: Rgb8,
    /// Font family set in the config, if any. Without one the fontconfig
    /// "monospace" alias resolves at view creation (what kitty itself
    /// does); see font.rs.
    pub font_family: Option<String>,
    /// Font size in points (kitty's unit); rendered as pt * 96/72.
    pub font_size_pt: f32,
    /// Terminal background alpha, kitty's background_opacity. 1.0 stays
    /// fully opaque; below that the desktop shows through behind the text.
    pub background_opacity: f32,
}

impl Theme {
    pub fn builtin() -> Self {
        Self {
            named: [
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
            ],
            foreground: Rgb8(224, 226, 228),
            background: Rgb8(24, 26, 31),
            cursor: Rgb8(224, 226, 228),
            cursor_text: Rgb8(24, 26, 31),
            font_family: None,
            font_size_pt: 11.0,
            background_opacity: 1.0,
        }
    }

    pub fn load() -> Self {
        let mut theme = Self::builtin();
        if let Some(path) = kitty_config() {
            apply_kitty_config(&mut theme, &path, 0);
        }
        // spike knob: blow the font up for shape debugging
        if let Ok(pt) = std::env::var("KUMA_TERM_FONT_PT") {
            if let Ok(pt) = pt.parse::<f32>() {
                if pt > 0.0 {
                    theme.font_size_pt = pt;
                }
            }
        }
        // spike knob: demo translucency without touching kitty.conf
        if let Ok(alpha) = std::env::var("KUMA_TERM_OPACITY") {
            if let Ok(alpha) = alpha.parse::<f32>() {
                if (0.0..=1.0).contains(&alpha) {
                    theme.background_opacity = alpha;
                }
            }
        }
        theme
    }

    /// Fallback families for the glyphs a mono terminal leans on (box
    /// drawing, block elements, braille). Noto Sans Mono, the fontconfig
    /// "monospace" answer here, carries none of them; without a chain they
    /// fall through to a proportional face and aligned grids garble. For
    /// the spike these two are picked for coverage on this system; the real
    /// config story owns the list later.
    pub fn font_fallbacks(&self) -> gpui::FontFallbacks {
        gpui::FontFallbacks::from_fonts(vec![
            "DejaVu Sans Mono".to_string(),
            "Adwaita Mono".to_string(),
            "Liberation Mono".to_string(),
        ])
    }

    pub fn font_size_px(&self) -> f32 {
        self.font_size_pt * 96.0 / 72.0
    }
}

fn kitty_config() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".config/kitty/kitty.conf");
    path.exists().then_some(path)
}

/// Parse kitty's key value format (and its includes, one branch at a time,
/// relative to the including file). Unknown keys are ignored; values that
/// fail to parse leave the builtin in place.
fn apply_kitty_config(theme: &mut Theme, path: &Path, depth: u8) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(char::is_whitespace) else { continue };
        let key = key.trim();
        let value = value.trim();

        if key == "include" {
            if depth < 4 {
                let included = path.parent().unwrap_or(Path::new(".")).join(value);
                if included.exists() {
                    apply_kitty_config(theme, &included, depth + 1);
                }
            }
            continue;
        }

        if let Some(rest) = key.strip_prefix("color") {
            if let Ok(index) = rest.parse::<usize>() {
                if index < 16 {
                    if let Some(c) = hex_color(value) {
                        theme.named[index] = c;
                    }
                }
            }
            continue;
        }

        match key {
            "foreground" => {
                if let Some(c) = hex_color(value) {
                    theme.foreground = c;
                }
            }
            "background" => {
                if let Some(c) = hex_color(value) {
                    theme.background = c;
                }
            }
            "cursor" => {
                if let Some(c) = hex_color(value) {
                    theme.cursor = c;
                }
            }
            "cursor_text_color" => {
                if let Some(c) = hex_color(value) {
                    theme.cursor_text = c;
                }
            }
            "font_family" => {
                let family = value.trim_matches('"').trim().to_string();
                if !family.is_empty() && family != "auto" {
                    theme.font_family = Some(family);
                }
            }
            "font_size" => {
                if let Ok(size) = value.parse::<f32>() {
                    if size > 0.0 {
                        theme.font_size_pt = size;
                    }
                }
            }
            "background_opacity" => {
                if let Ok(alpha) = value.parse::<f32>() {
                    if (0.0..=1.0).contains(&alpha) {
                        theme.background_opacity = alpha;
                    }
                }
            }
            _ => {}
        }
    }
}

fn hex_color(value: &str) -> Option<Rgb8> {
    let v = value.trim();
    let v = v.strip_prefix('#')?;
    if v.len() != 6 || !v.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&v[0..2], 16).ok()?;
    let g = u8::from_str_radix(&v[2..4], 16).ok()?;
    let b = u8::from_str_radix(&v[4..6], 16).ok()?;
    Some(Rgb8(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_theme_parses_nothing() {
        let mut theme = Theme::builtin();
        // a bogus file must leave the builtin untouched
        let path = std::env::temp_dir().join(format!("kuma-term-test-{}.conf", std::process::id()));
        std::fs::write(&path, "garbage line\nfont_size zero\ncolor0 #zzz\n").unwrap();
        apply_kitty_config(&mut theme, &path, 0);
        std::fs::remove_file(&path).ok();
        assert_eq!(theme.font_size_pt, 11.0);
        assert_eq!(theme.named[0], Rgb8(0, 0, 0));
    }

    #[test]
    fn kitty_keys_parse() {
        let mut theme = Theme::builtin();
        let path = std::env::temp_dir().join(format!("kuma-term-test2-{}.conf", std::process::id()));
        std::fs::write(
            &path,
            "# comment\nbackground #131317\nforeground  #e4e2e6 \ncolor1 #ffb4ab\nfont_size 12.5\nfont_family \"JetBrains Mono\"\nbackground_opacity 0.85\n",
        )
        .unwrap();
        apply_kitty_config(&mut theme, &path, 0);
        std::fs::remove_file(&path).ok();
        assert_eq!(theme.background, Rgb8(0x13, 0x13, 0x17));
        assert_eq!(theme.foreground, Rgb8(0xe4, 0xe2, 0xe6));
        assert_eq!(theme.named[1], Rgb8(0xff, 0xb4, 0xab));
        assert_eq!(theme.font_size_pt, 12.5);
        assert_eq!(theme.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(theme.background_opacity, 0.85);
    }

    #[test]
    fn includes_resolve_relative_to_the_config() {
        let dir = std::env::temp_dir().join(format!("kuma-term-test3-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("themes")).unwrap();
        std::fs::write(dir.join("themes/t.conf"), "color3 #112233\n").unwrap();
        std::fs::write(dir.join("main.conf"), "include themes/t.conf\n").unwrap();
        let mut theme = Theme::builtin();
        apply_kitty_config(&mut theme, &dir.join("main.conf"), 0);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(theme.named[3], Rgb8(0x11, 0x22, 0x33));
    }
}
