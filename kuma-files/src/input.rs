//! The single-line text input every hand-rolled editor in Koguma
//! rides: one buffer, a caret, and an optional selection. Ctrl+A,
//! paste-over, and the caret rendering behave the same in the connect
//! dialog, the compress dialog, in-place rename, and the path bar.

use gpui::{prelude::*, AnyElement, Keystroke, div, px};

use crate::theme;

/// One line of editable text: the buffer, the caret byte offset, and
/// the active selection span. All offsets are byte offsets into `buf`
/// and are kept on char boundaries by every edit; the clamps in the
/// accessors are belt and braces, not invitations.
#[derive(Default)]
pub(crate) struct Field {
    pub buf: String,
    pub cursor: usize,
    /// live selection, ordered (start, end)
    pub sel: Option<(usize, usize)>,
}

fn clamp_boundary(buf: &str, at: usize) -> usize {
    if buf.is_char_boundary(at) {
        at.min(buf.len())
    } else {
        buf.len()
    }
}

impl Field {
    pub fn new(buf: impl Into<String>) -> Self {
        let buf = buf.into();
        let cursor = buf.len();
        Self { buf, cursor, sel: None }
    }

    pub fn text(&self) -> &str {
        &self.buf
    }

    /// Ctrl+A: select the whole buffer, caret parked at the end.
    pub fn select_all(&mut self) {
        if !self.buf.is_empty() {
            self.sel = Some((0, self.buf.len()));
            self.cursor = self.buf.len();
        }
    }

    pub fn collapse(&mut self) {
        self.sel = None;
    }

    /// The span a delete or insert consumes: the live selection when
    /// it is real, None when only the caret is up.
    fn selected_span(&self) -> Option<(usize, usize)> {
        let (start, end) = self.sel?;
        let start = clamp_boundary(&self.buf, start);
        let end = clamp_boundary(&self.buf, end);
        (end > start).then_some((start, end))
    }

    fn delete_span(&mut self) -> bool {
        if let Some((start, end)) = self.selected_span() {
            self.buf.replace_range(start..end, "");
            self.cursor = start;
            self.sel = None;
            true
        } else {
            false
        }
    }

    /// Typed text or a paste: replaces the selection when one is
    /// live, else inserts at the caret.
    pub fn paste(&mut self, text: &str) {
        self.delete_span();
        let cursor = clamp_boundary(&self.buf, self.cursor);
        self.buf.insert_str(cursor, text);
        self.cursor = cursor + text.len();
    }

    pub fn backspace(&mut self) {
        if self.delete_span() {
            return;
        }
        let cursor = clamp_boundary(&self.buf, self.cursor);
        if let Some((prev, _)) = self.buf[..cursor].char_indices().next_back() {
            self.buf.remove(prev);
            self.cursor = prev;
        }
    }

    pub fn delete_forward(&mut self) {
        if self.delete_span() {
            return;
        }
        let cursor = clamp_boundary(&self.buf, self.cursor);
        if let Some(ch) = self.buf[cursor..].chars().next() {
            self.buf.replace_range(cursor..cursor + ch.len_utf8(), "");
        }
    }

    pub fn left(&mut self) {
        if let Some((start, _)) = self.selected_span().or(self.sel) {
            self.cursor = clamp_boundary(&self.buf, start);
            self.sel = None;
            return;
        }
        let cursor = clamp_boundary(&self.buf, self.cursor);
        if let Some((prev, _)) = self.buf[..cursor].char_indices().next_back() {
            self.cursor = prev;
        }
    }

    pub fn right(&mut self) {
        if let Some((_, end)) = self.selected_span().or(self.sel) {
            self.cursor = clamp_boundary(&self.buf, end);
            self.sel = None;
            return;
        }
        let cursor = clamp_boundary(&self.buf, self.cursor);
        if let Some(ch) = self.buf[cursor..].chars().next() {
            self.cursor = cursor + ch.len_utf8();
        }
    }

    pub fn home(&mut self) {
        self.sel = None;
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.sel = None;
        self.cursor = self.buf.len();
    }

    /// One keystroke's worth of editing: backspace, delete, left,
    /// right, home, end, or a typed character (which replaces any
    /// selection). False for keys the caller handles: Enter, Esc,
    /// Tab, and every Ctrl/Alt/Platform combo.
    pub fn key(&mut self, key: &str, keystroke: &Keystroke) -> bool {
        match key {
            "backspace" => self.backspace(),
            "delete" => self.delete_forward(),
            "left" => self.left(),
            "right" => self.right(),
            "home" => self.home(),
            "end" => self.end(),
            _ if !keystroke.modifiers.control
                && !keystroke.modifiers.alt
                && !keystroke.modifiers.platform
                && !keystroke.modifiers.function =>
            {
                if let Some(character) = keystroke.key_char.as_deref() {
                    self.paste(character);
                }
            }
            _ => return false,
        }
        true
    }
}

/// The three render spans around the selection: before, selected,
/// after. When no selection is live the middle span is empty and the
/// split sits at the caret. Masked fields render one bullet per
/// character in every span.
pub(crate) fn spans(field: &Field, masked: bool) -> (String, Option<String>, String) {
    let mask = |text: &str| {
        if masked {
            "\u{2022}".repeat(text.chars().count())
        } else {
            text.to_string()
        }
    };
    match field.selected_span() {
        Some((start, end)) => {
            let (before, rest) = field.buf.split_at(start);
            let (selected, after) = rest.split_at(end - start);
            (mask(before), Some(mask(selected)), mask(after))
        }
        None => {
            let cursor = clamp_boundary(&field.buf, field.cursor);
            let (before, after) = field.buf.split_at(cursor);
            (mask(before), None, mask(after))
        }
    }
}

/// The input rendered: text spans with the selection highlighted, a
/// 1.5px accent-bar caret in the focused field (a real bar, not a
/// typed character), or the placeholder hint in dim text when the
/// field is empty and unfocused.
pub(crate) fn field_children(
    field: &Field,
    masked: bool,
    caret: bool,
    placeholder: &str,
) -> Vec<AnyElement> {
    let (before, selected, after) = spans(field, masked);
    span_children(&before, selected, &after, caret, placeholder)
}

/// The same render from precomputed spans, for sites that cannot
/// hold the `Field` borrow across a mutation (the icon tile copies
/// its spans out before `request_thumb` re-borrows the view).
pub(crate) fn span_children(
    before: &str,
    selected: Option<String>,
    after: &str,
    caret: bool,
    placeholder: &str,
) -> Vec<AnyElement> {
    let mut children: Vec<AnyElement> = Vec::new();
    let empty = before.is_empty() && selected.is_none() && after.is_empty();
    if !before.is_empty() {
        children.push(div().child(before.to_string()).into_any_element());
    }
    if let Some(selected) = selected {
        children.push(
            div()
                .bg(theme::row_selected())
                .child(selected)
                .into_any_element(),
        );
    }
    if caret {
        // a fixed one-line bar: a percentage height here would size
        // against the row's own auto height and feed back into it
        children.push(
            div()
                .w(px(1.5))
                .h(px(14.))
                .bg(theme::accent())
                .into_any_element(),
        );
    } else if empty {
        children.push(
            div()
                .text_color(theme::text_dim())
                .child(placeholder.to_string())
                .into_any_element(),
        );
    }
    if !after.is_empty() {
        children.push(div().child(after.to_string()).into_any_element());
    }
    children
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_all_then_type_replaces() {
        let mut field = Field::new("notes.txt");
        field.select_all();
        assert_eq!(field.sel, Some((0, 9)));
        field.paste("a");
        assert_eq!(field.text(), "a");
        assert_eq!(field.cursor, 1);
        assert_eq!(field.sel, None);
    }

    #[test]
    fn backspace_deletes_the_selection_first() {
        let mut field = Field::new("notes.txt");
        field.select_all();
        field.backspace();
        assert_eq!(field.text(), "");
        // backspace again is a no-op, not a panic
        field.backspace();
        assert_eq!(field.text(), "");
    }

    #[test]
    fn arrows_collapse_the_selection() {
        let mut field = Field::new("abcd");
        field.select_all();
        field.left();
        assert_eq!(field.cursor, 0);
        assert_eq!(field.sel, None);
        field.select_all();
        field.right();
        assert_eq!(field.cursor, 4);
        assert_eq!(field.sel, None);
    }

    #[test]
    fn multibyte_offsets_stay_on_boundaries() {
        let mut field = Field::new("héllo");
        field.end();
        assert_eq!(field.cursor, 6); // é is two bytes: h(1) é(2) l(1) l(1) o(1)
        field.select_all();
        field.paste("é");
        assert_eq!(field.text(), "é");
        field.left();
        assert_eq!(field.cursor, 0);
        field.right();
        assert_eq!(field.cursor, 2);
    }

    #[test]
    fn masked_spans_bullet_every_char() {
        let mut field = Field::new("secret");
        field.select_all();
        let (before, selected, after) = spans(&field, true);
        assert_eq!(before, "");
        assert_eq!(selected.as_deref(), Some("••••••"));
        assert_eq!(after, "");
    }

    #[test]
    fn ctrl_combos_fall_through_to_the_caller() {
        let mut field = Field::new("abc");
        let mut keystroke = Keystroke::parse("a").unwrap();
        keystroke.modifiers.control = true;
        assert!(!field.key("a", &keystroke));
        assert!(field.key("backspace", &Keystroke::parse("backspace").unwrap()));
        assert_eq!(field.text(), "ab");
    }

    #[test]
    fn empty_unfocused_field_leaves_room_for_the_placeholder() {
        let field = Field::default();
        let (before, selected, after) = spans(&field, false);
        assert_eq!((before, selected, after), ("".to_string(), None, "".to_string()));
        let children = field_children(&field, false, false, "hint");
        assert_eq!(children.len(), 1);
    }
}
