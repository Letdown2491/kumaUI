//! Keystroke to PTY bytes encoding.
//!
//! The spike encodes the legacy format (the one xterm has spoken for
//! decades): plain text as UTF-8, control chords as C0 bytes, named keys as
//! SS3/CSI sequences. The kitty keyboard protocol layers on top of this
//! later; legacy stays the fallback every terminal must get right.

use gpui::Keystroke;

/// xterm's modifier parameter for CSI sequences: 1 + shift(1) + alt(2) + ctrl(4).
fn modifier_param(k: &Keystroke) -> u8 {
    let m = &k.modifiers;
    1 + (m.shift as u8) + ((m.alt as u8) << 1) + ((m.control as u8) << 2)
}

/// CSI n~ keys (insert, delete, page keys, f5+), with the modifier parameter
/// appended when modifiers are held.
fn csi_tilde(n: u8, k: &Keystroke) -> Vec<u8> {
    let m = modifier_param(k);
    if m > 1 {
        format!("\x1b[{};{}~", n, m).into_bytes()
    } else {
        format!("\x1b[{}~", n).into_bytes()
    }
}

/// Arrow and home/end keys. These honor the application cursor keys mode
/// (DECCKM): SS3 when the program requested it, CSI otherwise. When
/// modifiers are held the CSI form carries the modifier parameter, which
/// supersedes the mode choice (xterm behavior).
fn cursor_key(c: char, k: &Keystroke, app_cursor: bool) -> Vec<u8> {
    let m = modifier_param(k);
    if m > 1 {
        format!("\x1b[1;{}{}", m, c).into_bytes()
    } else if app_cursor {
        format!("\x1bO{}", c).into_bytes()
    } else {
        format!("\x1b[{}", c).into_bytes()
    }
}

/// f1 through f4: SS3 letters, or CSI 1;m<letter> with modifiers.
fn top_function_key(c: char, k: &Keystroke) -> Vec<u8> {
    let m = modifier_param(k);
    if m > 1 {
        format!("\x1b[1;{}{}", m, c).into_bytes()
    } else {
        format!("\x1bO{}", c).into_bytes()
    }
}

/// Encode a keystroke into the bytes the shell should receive. Returns None
/// for chords kuma-term handles itself (clipboard) or ignores.
pub fn encode(k: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let m = &k.modifiers;
    let ctrl = m.control;
    let alt = m.alt;

    let named: Option<Vec<u8>> = match k.key.as_str() {
        "escape" => Some(b"\x1b".to_vec()),
        "enter" => Some(b"\r".to_vec()),
        "tab" if m.shift => Some(b"\x1b[Z".to_vec()),
        "tab" => Some(b"\t".to_vec()),
        "backspace" => Some(vec![if ctrl { 0x08 } else { 0x7f }]),
        "delete" => Some(csi_tilde(3, k)),
        "insert" => Some(csi_tilde(2, k)),
        "home" => Some(cursor_key('H', k, app_cursor)),
        "end" => Some(cursor_key('F', k, app_cursor)),
        "pageup" => Some(csi_tilde(5, k)),
        "pagedown" => Some(csi_tilde(6, k)),
        "up" => Some(cursor_key('A', k, app_cursor)),
        "down" => Some(cursor_key('B', k, app_cursor)),
        "right" => Some(cursor_key('C', k, app_cursor)),
        "left" => Some(cursor_key('D', k, app_cursor)),
        "f1" => Some(top_function_key('P', k)),
        "f2" => Some(top_function_key('Q', k)),
        "f3" => Some(top_function_key('R', k)),
        "f4" => Some(top_function_key('S', k)),
        "f5" => Some(csi_tilde(15, k)),
        "f6" => Some(csi_tilde(17, k)),
        "f7" => Some(csi_tilde(18, k)),
        "f8" => Some(csi_tilde(19, k)),
        "f9" => Some(csi_tilde(20, k)),
        "f10" => Some(csi_tilde(21, k)),
        "f11" => Some(csi_tilde(23, k)),
        "f12" => Some(csi_tilde(24, k)),
        _ => None,
    };

    if let Some(mut bytes) = named {
        // alt prefixes everything except keys that already encoded it in
        // their modifier parameter
        if alt && !matches!(k.key.as_str(), "up" | "down" | "left" | "right" | "home" | "end" | "f1" | "f2" | "f3" | "f4") {
            bytes.insert(0, 0x1b);
        }
        return Some(bytes);
    }

    if ctrl {
        // control maps printable keys into C0, which is how the shell sees
        // ctrl+a and friends; the key name stays lowercase under ctrl
        if let Some(b) = ctrl_byte(k.key.as_str()) {
            let mut bytes = vec![b];
            if alt {
                bytes.insert(0, 0x1b);
            }
            return Some(bytes);
        }
        return None;
    }

    if let Some(text) = &k.key_char {
        if !text.is_empty() {
            let mut bytes = text.as_bytes().to_vec();
            if alt {
                bytes.insert(0, 0x1b);
            }
            return Some(bytes);
        }
    }

    None
}

fn ctrl_byte(key: &str) -> Option<u8> {
    let b = match key {
        "space" | "@" => 0x00,
        "a" => 0x01,
        "b" => 0x02,
        "c" => 0x03,
        "d" => 0x04,
        "e" => 0x05,
        "f" => 0x06,
        "g" => 0x07,
        "h" => 0x08,
        "i" => 0x09,
        "j" => 0x0a,
        "k" => 0x0b,
        "l" => 0x0c,
        "m" => 0x0d,
        "n" => 0x0e,
        "o" => 0x0f,
        "p" => 0x10,
        "q" => 0x11,
        "r" => 0x12,
        "s" => 0x13,
        "t" => 0x14,
        "u" => 0x15,
        "v" => 0x16,
        "w" => 0x17,
        "x" => 0x18,
        "y" => 0x19,
        "z" => 0x1a,
        "[" => 0x1b,
        "\\" => 0x1c,
        "]" => 0x1d,
        "^" => 0x1e,
        "_" | "-" => 0x1f,
        _ => return None,
    };
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(s: &str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    #[test]
    fn plain_text_passes_through() {
        let mut k = ks("a");
        k.key_char = Some("a".to_string());
        assert_eq!(encode(&k, false).unwrap(), b"a");

        let mut k = ks("shift-a");
        k.key_char = Some("A".to_string());
        assert_eq!(encode(&k, false).unwrap(), b"A");

        // unicode text rides through as utf-8
        let mut k = ks("a");
        k.key_char = Some("λ".to_string());
        assert_eq!(encode(&k, false).unwrap(), "λ".as_bytes());
    }

    #[test]
    fn control_chords_map_to_c0() {
        assert_eq!(encode(&ks("ctrl-a"), false).unwrap(), vec![0x01]);
        assert_eq!(encode(&ks("ctrl-c"), false).unwrap(), vec![0x03]);
        assert_eq!(encode(&ks("ctrl-space"), false).unwrap(), vec![0x00]);
        assert_eq!(encode(&ks("ctrl-["), false).unwrap(), vec![0x1b]);
        // alt wraps the C0 byte in an escape
        assert_eq!(encode(&ks("alt-ctrl-a"), false).unwrap(), vec![0x1b, 0x01]);
    }

    #[test]
    fn named_keys_get_their_sequences() {
        assert_eq!(encode(&ks("enter"), false).unwrap(), b"\r");
        assert_eq!(encode(&ks("escape"), false).unwrap(), b"\x1b");
        assert_eq!(encode(&ks("tab"), false).unwrap(), b"\t");
        assert_eq!(encode(&ks("shift-tab"), false).unwrap(), b"\x1b[Z");
        assert_eq!(encode(&ks("backspace"), false).unwrap(), vec![0x7f]);
        assert_eq!(encode(&ks("delete"), false).unwrap(), b"\x1b[3~");
        assert_eq!(encode(&ks("pageup"), false).unwrap(), b"\x1b[5~");
        assert_eq!(encode(&ks("f5"), false).unwrap(), b"\x1b[15~");
        assert_eq!(encode(&ks("f12"), false).unwrap(), b"\x1b[24~");
    }

    #[test]
    fn arrows_respect_application_cursor_mode() {
        assert_eq!(encode(&ks("up"), false).unwrap(), b"\x1b[A");
        assert_eq!(encode(&ks("up"), true).unwrap(), b"\x1bOA");
        // modifiers switch to the parameterized form
        assert_eq!(encode(&ks("ctrl-up"), false).unwrap(), b"\x1b[1;5A");
        assert_eq!(encode(&ks("alt-left"), false).unwrap(), b"\x1b[1;3D");
        assert_eq!(encode(&ks("shift-right"), true).unwrap(), b"\x1b[1;2C");
    }

    #[test]
    fn alt_prefixes_printable_keys() {
        let mut k = ks("alt-x");
        k.key_char = Some("x".to_string());
        assert_eq!(encode(&k, false).unwrap(), vec![0x1b, b'x']);
    }
}
