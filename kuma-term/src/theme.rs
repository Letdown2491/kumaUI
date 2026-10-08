//! The theme: palette and font, read from kuma-term's own config over
//! built-in defaults. The config lives at
//! $XDG_CONFIG_HOME/kuma-term/kuma-term.conf (default ~/.config), and the
//! grammar is line-based key value with kitty-compatible key names, so a
//! colors block copied from a kitty theme pastes in unchanged. On kumaOS
//! the session's look flows from kuma-shell's published wallpaper
//! palette, which wins for the chrome colors; without kuma-shell the
//! config file is the whole story, and with neither the built-ins hold.

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
    /// "monospace" alias resolves at view creation; see font.rs.
    pub font_family: Option<String>,
    /// Font size in points, rendered as pt * 96/72.
    pub font_size_pt: f32,
    /// Terminal background alpha when the window has focus. The focused
    /// window shows the most wallpaper; inactive windows dim toward solid.
    pub background_opacity: f32,
    /// Terminal background alpha when it does not.
    pub background_opacity_unfocused: f32,
    /// Families tried when the primary mono font lacks a glyph: emoji,
    /// CJK, misc symbols. Missing families are dropped when the chain
    /// loads, so naming a face a distro does not ship is harmless.
    pub font_fallbacks: Vec<String>,
    /// Scrolled-off lines the grid keeps. Read once at startup: changing
    /// it needs a restart, the grid is sized at term creation.
    pub scrollback_lines: usize,
}

impl Theme {
    pub fn builtin() -> Self {
        // The noctalia pastels: the palette the author's terminal settled
        // on through kumaOS's daily life, now the shipped look. A
        // kuma-term.conf overrides any of it, and on kumaOS the shell's
        // wallpaper palette still wins for the chrome colors.
        Self {
            named: [
                Rgb8(0x45, 0x46, 0x4f),
                Rgb8(0xff, 0xb4, 0xab),
                Rgb8(0xb5, 0xc5, 0xfd),
                Rgb8(0xc0, 0xc5, 0xde),
                Rgb8(0xe7, 0xb6, 0xed),
                Rgb8(0xb5, 0xc5, 0xfd),
                Rgb8(0xc0, 0xc5, 0xde),
                Rgb8(0xe4, 0xe2, 0xe6),
                Rgb8(0x8f, 0x90, 0x9a),
                Rgb8(0xff, 0xb4, 0xab),
                Rgb8(0xb5, 0xc5, 0xfd),
                Rgb8(0xc0, 0xc5, 0xde),
                Rgb8(0xe7, 0xb6, 0xed),
                Rgb8(0xb5, 0xc5, 0xfd),
                Rgb8(0xc0, 0xc5, 0xde),
                Rgb8(0xe4, 0xe2, 0xe6),
            ],
            foreground: Rgb8(0xe4, 0xe2, 0xe6),
            background: Rgb8(0x13, 0x13, 0x17),
            cursor: Rgb8(0xe4, 0xe2, 0xe6),
            cursor_text: Rgb8(0x13, 0x13, 0x17),
            font_family: None,
            font_size_pt: 11.0,
            background_opacity: 0.80,
            background_opacity_unfocused: 0.90,
            // emoji and CJK are the glyphs every mono font lacks; DejaVu
            // Sans picks up the remaining math and arrow odds and ends.
            font_fallbacks: vec![
                "Noto Color Emoji".to_string(),
                "Noto Sans CJK JP".to_string(),
                "DejaVu Sans".to_string(),
            ],
            scrollback_lines: 10_000,
        }
    }

    pub fn load() -> Self {
        let mut theme = Self::builtin();
        if let Some(path) = config_path() {
            apply_config(&mut theme, &path, 0);
        }
        // the session's look: kuma-shell publishes the wallpaper palette
        // for its neighbors and it wins for the chrome colors (the ANSI 16
        // stay the terminal's own; the palette carries none of them)
        if let Some(text) = Self::shell_palette_text() {
            apply_shell_palette(&mut theme, &text);
        }
        // spike knob: blow the font up for shape debugging
        if let Ok(pt) = std::env::var("KUMA_TERM_FONT_PT") {
            if let Ok(pt) = pt.parse::<f32>() {
                if pt > 0.0 {
                    theme.font_size_pt = pt;
                }
            }
        }
        // spike knobs: demo translucency without touching configs
        if let Ok(alpha) = std::env::var("KUMA_TERM_OPACITY") {
            if let Ok(alpha) = alpha.parse::<f32>() {
                if (0.0..=1.0).contains(&alpha) {
                    theme.background_opacity = alpha;
                }
            }
        }
        if let Ok(alpha) = std::env::var("KUMA_TERM_OPACITY_UNFOCUSED") {
            if let Ok(alpha) = alpha.parse::<f32>() {
                if (0.0..=1.0).contains(&alpha) {
                    theme.background_opacity_unfocused = alpha;
                }
            }
        }
        theme
    }

    /// The wallpaper palette kuma-shell publishes for session neighbors,
    /// with its file's mtime so a two second tick can spot republishes.
    pub fn shell_palette_text() -> Option<String> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
        let path = std::path::PathBuf::from(dir).join("kuma-shell/palette");
        std::fs::read_to_string(path).ok()
    }

    pub fn palette_mtime() -> Option<std::time::SystemTime> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
        let path = std::path::PathBuf::from(dir).join("kuma-shell/palette");
        std::fs::metadata(path).ok()?.modified().ok()
    }

    /// The fallback chain the grid's font carries: the config's list (or
    /// the builtin default), verbatim.
    pub fn font_fallbacks(&self) -> gpui::FontFallbacks {
        gpui::FontFallbacks::from_fonts(self.font_fallbacks.clone())
    }

    pub fn font_size_px(&self) -> f32 {
        self.font_size_pt * 96.0 / 72.0
    }
}

/// The config file: $XDG_CONFIG_HOME/kuma-term/kuma-term.conf, or
/// ~/.config/kuma-term/kuma-term.conf when XDG_CONFIG_HOME is unset (a
/// non-absolute XDG value is not the spec's path and is ignored).
fn config_path() -> Option<PathBuf> {
    config_path_for(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// The pure rule behind `config_path`, so the XDG handling is testable
/// without mutating process environment.
fn config_path_for(xdg: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let explicit = xdg.map(PathBuf::from).filter(|dir| dir.is_absolute());
    let dir = match explicit {
        Some(dir) => dir,
        None => PathBuf::from(home?).join(".config"),
    };
    Some(dir.join("kuma-term/kuma-term.conf"))
}

/// Parse the key value grammar (and its includes, one branch at a time,
/// relative to the including file). Unknown keys are ignored; values that
/// fail to parse leave the built-in in place. The key names follow
/// kitty's theme block on purpose: a kitty theme pastes in unedited.
fn apply_config(theme: &mut Theme, path: &Path, depth: u8) {
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
                    apply_config(theme, &included, depth + 1);
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
            // kuma-term's own key, comma-separated family list. "none"
            // disables fallback entirely: missing glyphs then draw as
            // notdef boxes, which is sometimes what a purist wants.
            "font_fallbacks" => {
                if value.trim().eq_ignore_ascii_case("none") {
                    theme.font_fallbacks = Vec::new();
                } else {
                    theme.font_fallbacks = value
                        .split(',')
                        .map(|name| name.trim_matches('"').trim().to_string())
                        .filter(|name| !name.is_empty())
                        .collect();
                }
            }
            // kitty's key name. A value of 0 keeps no history at all.
            "scrollback_lines" => {
                if let Ok(lines) = value.trim().parse::<usize>() {
                    // the grid is preallocated: cap the knob so a fat
                    // finger cannot ask the terminal for gigabytes
                    theme.scrollback_lines = lines.clamp(0, 1_000_000);
                }
            }
            "background_opacity" => {
                if let Ok(alpha) = value.parse::<f32>() {
                    if (0.0..=1.0).contains(&alpha) {
                        theme.background_opacity = alpha;
                    }
                }
            }
            // kuma-term's own key: unfocused alpha
            "background_opacity_unfocused" => {
                if let Ok(alpha) = value.parse::<f32>() {
                    if (0.0..=1.0).contains(&alpha) {
                        theme.background_opacity_unfocused = alpha;
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

/// Swap the chrome for the wallpaper palette kuma-shell publishes
/// (key=value, RRGGBB or RRGGBBAA). Same mapping kuma-files uses: the
/// window backing is the shell's darkest inset, text follows the shell's
/// text, and the cursor takes the accent. The ANSI 16 are untouched.
pub fn apply_shell_palette(theme: &mut Theme, text: &str) {
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(hex) = palette_hex(value) else {
            continue;
        };
        match key.trim() {
            "inset" => theme.background = hex,
            "text" => theme.foreground = hex,
            "accent" => theme.cursor = hex,
            "accent_text" => theme.cursor_text = hex,
            _ => {}
        }
    }
}

/// The shell's panel colors arrive as RRGGBBAA (alpha in the low byte);
/// the terminal's chrome is opaque, so keep the top 24 bits. Pure RGB
/// values pass through untouched.
fn palette_hex(value: &str) -> Option<Rgb8> {
    let v = value.trim();
    let n = u32::from_str_radix(v, 16).ok()?;
    let n = if n > 0xFFFFFF { n >> 8 } else { n };
    Some(Rgb8((n >> 16) as u8, (n >> 8) as u8, n as u8))
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
        apply_config(&mut theme, &path, 0);
        std::fs::remove_file(&path).ok();
        assert_eq!(theme.font_size_pt, 11.0);
        assert_eq!(theme.named[0], Rgb8(0x45, 0x46, 0x4f));
    }

    #[test]
    fn config_keys_parse() {
        let mut theme = Theme::builtin();
        let path = std::env::temp_dir().join(format!("kuma-term-test2-{}.conf", std::process::id()));
        std::fs::write(
            &path,
            "# comment\nbackground #131317\nforeground  #e4e2e6 \ncolor1 #ffb4ab\nfont_size 12.5\nfont_family \"JetBrains Mono\"\nbackground_opacity 0.85\n",
        )
        .unwrap();
        apply_config(&mut theme, &path, 0);
        std::fs::remove_file(&path).ok();
        assert_eq!(theme.background, Rgb8(0x13, 0x13, 0x17));
        assert_eq!(theme.foreground, Rgb8(0xe4, 0xe2, 0xe6));
        assert_eq!(theme.named[1], Rgb8(0xff, 0xb4, 0xab));
        assert_eq!(theme.font_size_pt, 12.5);
        assert_eq!(theme.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(theme.background_opacity, 0.85);
    }

    #[test]
    fn fallback_and_scrollback_keys_parse() {
        let mut theme = Theme::builtin();
        let path = std::env::temp_dir().join(format!("kuma-term-test4-{}.conf", std::process::id()));
        std::fs::write(
            &path,
            "font_fallbacks \"Noto Color Emoji\", Noto Sans CJK SC, none-of-these\nscrollback_lines 250000\n",
        )
        .unwrap();
        apply_config(&mut theme, &path, 0);
        std::fs::remove_file(&path).ok();
        assert_eq!(
            theme.font_fallbacks,
            vec!["Noto Color Emoji", "Noto Sans CJK SC", "none-of-these"]
        );
        assert_eq!(theme.scrollback_lines, 250_000);
        // the chain a font loads from is the parsed list, verbatim
        assert_eq!(
            theme.font_fallbacks().fallback_list(),
            theme.font_fallbacks
        );
    }

    #[test]
    fn fallback_none_disables_and_scrollback_clamps() {
        let mut theme = Theme::builtin();
        let path = std::env::temp_dir().join(format!("kuma-term-test5-{}.conf", std::process::id()));
        std::fs::write(&path, "font_fallbacks none\nscrollback_lines 999999999\n").unwrap();
        apply_config(&mut theme, &path, 0);
        std::fs::remove_file(&path).ok();
        assert!(theme.font_fallbacks.is_empty());
        assert_eq!(theme.scrollback_lines, 1_000_000);
        // a bogus value leaves the default
        let mut theme = Theme::builtin();
        std::fs::write(&path, "scrollback_lines lots\n").unwrap();
        apply_config(&mut theme, &path, 0);
        std::fs::remove_file(&path).ok();
        assert_eq!(theme.scrollback_lines, 10_000);
    }

    #[test]
    fn includes_resolve_relative_to_the_config() {
        let dir = std::env::temp_dir().join(format!("kuma-term-test3-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("themes")).unwrap();
        std::fs::write(dir.join("themes/t.conf"), "color3 #112233\n").unwrap();
        std::fs::write(dir.join("main.conf"), "include themes/t.conf\n").unwrap();
        let mut theme = Theme::builtin();
        apply_config(&mut theme, &dir.join("main.conf"), 0);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(theme.named[3], Rgb8(0x11, 0x22, 0x33));
    }

    #[test]
    fn config_path_follows_the_xdg_rule() {
        use std::ffi::OsStr;
        use std::path::PathBuf;
        // an absolute XDG_CONFIG_HOME wins
        assert_eq!(
            config_path_for(Some(OsStr::new("/x")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/x/kuma-term/kuma-term.conf"))
        );
        // without it, HOME's .config
        assert_eq!(
            config_path_for(None, Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.config/kuma-term/kuma-term.conf"))
        );
        // a relative XDG value is not the spec's path: ignored
        assert_eq!(
            config_path_for(Some(OsStr::new("rel")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.config/kuma-term/kuma-term.conf"))
        );
        // neither set: no path at all
        assert_eq!(config_path_for(None, None), None);
    }

    #[test]
    fn shell_palette_swaps_the_chrome() {
        let mut theme = Theme::builtin();
        apply_shell_palette(
            &mut theme,
            "panel_bg=161c1d\ninset=0d1111\ntext=dbe0e1\naccent=90cfdf\naccent_text=0d1111\n",
        );
        // the window backing is the shell's darkest inset, text follows the
        // shell, the cursor takes the accent; ANSI 16 stay untouched
        assert_eq!(theme.background, Rgb8(0x0d, 0x11, 0x11));
        assert_eq!(theme.foreground, Rgb8(0xdb, 0xe0, 0xe1));
        assert_eq!(theme.cursor, Rgb8(0x90, 0xcf, 0xdf));
        assert_eq!(theme.cursor_text, Rgb8(0x0d, 0x11, 0x11));
        assert_eq!(theme.named[1], Theme::builtin().named[1]);
    }

    #[test]
    fn shell_palette_accepts_alpha_bytes() {
        let mut theme = Theme::builtin();
        apply_shell_palette(&mut theme, "inset=0d1111ff\n");
        // RRGGBBAA: the alpha byte drops, the top 24 bits stay
        assert_eq!(theme.background, Rgb8(0x0d, 0x11, 0x11));
    }
}
