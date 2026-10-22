//! Paste for the shell's hand-rolled text fields. Every field owns its
//! key match arms (the launcher's query, the Wi-Fi and lock and
//! greeter passwords, the settings edits), and none of them ran a
//! modified key to text, so Ctrl+V went nowhere: pasting a password
//! or a long query meant typing it by hand. One helper, one paste
//! rule; each field keeps its own insert.

use gpui::App;

/// What a paste inserts: the clipboard's text as one line. Newlines
/// are edge trim, not content: a multi-line copy into a password or a
/// query is almost always a stray trailing newline from the copying
/// app, and the middle of a password never wants one (kuma-files'
/// connect dialog runs the same rule). Whitespace inside survives.
/// None means the clipboard holds nothing insertable.
pub fn sanitize_paste(text: &str) -> Option<String> {
    let trimmed = text.trim_matches(|c: char| c == '\r' || c == '\n');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The clipboard's text, sanitized. Call from a field's ctrl+v arm.
pub fn pasted_text(cx: &App) -> Option<String> {
    let item = cx.read_from_clipboard()?;
    sanitize_paste(&item.text()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_is_one_line_with_interior_whitespace_kept() {
        // ordinary text passes whole
        assert_eq!(sanitize_paste("hello world").as_deref(), Some("hello world"));
        // interior whitespace is content
        assert_eq!(sanitize_paste("two  spaces").as_deref(), Some("two  spaces"));
        // a stray trailing newline from the copying app is edge, not
        // content
        assert_eq!(sanitize_paste("password\n").as_deref(), Some("password"));
        assert_eq!(
            sanitize_paste("first\r\nsecond").as_deref(),
            Some("first\r\nsecond"),
            "interior newlines survive: a middle-of-copy linebreak is the user's paste"
        );
        // only whitespace: nothing to insert
        assert_eq!(sanitize_paste("\n\r\n"), None);
        assert_eq!(sanitize_paste(""), None);
    }
}
