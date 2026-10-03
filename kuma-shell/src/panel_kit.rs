//! The panel kit: the one surface vocabulary every panel shares, the
//! shape the nostr panel's panes wore first and the rest of the shell
//! now borrows. A panel is a rail or sidebar, a pane header, and a
//! column of cards; the pieces here are the card, the button, the
//! empty state, and the header, so no panel grows its own dialect.

use gpui::prelude::*;
use gpui::{
    AnyView, App, ClickEvent, Div, FontWeight, IntoElement, ParentElement, SharedString, Window,
    div, px, rgb, rgba,
};

use crate::theme::*;

/// The card: a column wearing fill and radius, with an id (the
/// reconciler's identity), so a list whose cards come and go reuses
/// the right node. Cards hold one decision or one fact group; a pane
/// is a stack of them, never a bare run of controls.
pub fn card(id: impl Into<SharedString>) -> gpui::Stateful<Div> {
    div()
        .id(id.into())
        .flex()
        .flex_col()
        .gap_2p5()
        .px_3p5()
        .py_3()
        .rounded_lg()
        .bg(rgb(SURFACE))
}

/// A card's title: the 13px semibold line every card opens with, over
/// its 11px dim description (when it has one).
pub fn card_title(title: &str) -> Div {
    div()
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(TEXT))
        .child(title.to_string())
}

/// A card's dim description line: the sentence under a card title
/// that says what the control below it does.
pub fn card_note(note: &str) -> Div {
    div()
        .text_size(px(11.))
        .text_color(rgb(TEXT_DIM))
        .child(note.to_string())
}

/// The setting row: a card that carries one setting, its name on the
/// left and its control on the right. The row twin of the toggle row
/// for answers that are more than on and off; a page of these reads
/// as one aligned column of labels, not a stack of sparse cards.
pub fn setting_row(label: &str, control: impl IntoElement) -> gpui::Stateful<Div> {
    div()
        .id(SharedString::from(format!(
            "setting-{}",
            label.to_lowercase().replace(' ', "-")
        )))
        .flex()
        .items_center()
        .gap_2()
        .px_3p5()
        .py_2p5()
        .rounded_lg()
        .bg(rgb(SURFACE))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.))
                .text_color(rgb(TEXT))
                .truncate()
                .child(label.to_string()),
        )
        .child(control)
}

/// The pane header: the 17px semibold title row, the loudest line in
/// the panel. Callers append their trailing state (a count badge, a
/// lock glyph, nav arrows) onto the returned row; the title takes the
/// flex and the rest rides the right edge.
pub fn pane_header(title: &str) -> Div {
    div().flex().items_center().gap_2p5().child(
        div()
            .flex_1()
            .min_w_0()
            .text_size(px(17.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(TEXT))
            .truncate()
            .child(title.to_string()),
    )
}

/// The pane of a tabbed panel: the header that names the open tab sits
/// above the body alone, never above the tabs, so switching tabs moves
/// the title and not the layout. The rail or sidebar holding the tabs
/// is the caller's; this is the column that hosts what they open, and
/// every panel with tabs builds its pane here.
pub fn tabbed_pane(header: Div, body: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap_2p5()
        .child(header)
        .child(div().h(px(1.)).w_full().bg(rgba(DIVIDER_SOFT)))
        .child(body)
}

/// The count badge that rides a header or a rail tab: a filled accent
/// pill for the number of things waiting on the person.
pub fn count_badge(count: usize) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .bg(rgb(ACCENT))
        .text_size(px(11.))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(ACCENT_TEXT))
        .child(count.to_string())
}

struct TooltipView {
    text: SharedString,
}

impl Render for TooltipView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(rgb(0x11111B))
            .text_size(px(12.))
            .text_color(rgb(TEXT))
            .child(self.text.clone())
    }
}

/// A text tooltip: the dark pill that hangs under a hovered element.
/// The rail tabs use it to name a glyph; the bar uses it for anything
/// it shrinks to an icon.
pub fn text_tooltip(text: SharedString) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_, cx| cx.new(|_| TooltipView { text: text.clone() }).into()
}

/// The rail tab: an icon-only tab for a panel's left rail, named by a
/// tooltip on hover so the glyph never has to speak for itself. The
/// count badge rides under the glyph when something is waiting on the
/// person.
pub fn rail_tab(
    id: impl Into<SharedString>,
    icon: &'static str,
    label: &'static str,
    active: bool,
    badge: Option<usize>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id(id.into())
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_0p5()
        .w(px(44.))
        .py_2()
        .rounded_md()
        .cursor_pointer()
        .when(active, |el| el.bg(rgb(SURFACE)))
        .hover(|el| el.bg(rgb(SURFACE)))
        .tooltip(text_tooltip(label.into()))
        .on_click(on_click)
        .child(
            gpui::svg()
                .path(icon)
                .size(px(16.))
                .text_color(rgb(if active { ACCENT } else { TEXT_DIM })),
        )
        .children(badge.map(|count| {
            div()
                .text_size(px(10.))
                .text_color(rgb(if active { ACCENT } else { TEXT_DIM }))
                .child(count.to_string())
        }))
}

/// An empty pane: the glyph, the title, and the sentence under it.
/// An emptiness that says what will land here beats a blank pane.
pub fn empty_state(icon: &'static str, title: &str, subtitle: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2p5()
        .flex_1()
        .py(px(56.))
        .child(
            gpui::svg()
                .path(icon)
                .size(px(46.))
                .text_color(rgba(0x89B4FA66)),
        )
        .child(
            div()
                .text_size(px(14.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(TEXT))
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(TEXT_DIM))
                .child(subtitle.to_string()),
        )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    /// Fills the accent: the pane's one primary act.
    Primary,
    /// A word that hovers: quiet acts and secondary answers.
    Ghost,
    /// Fills the alarm: the acts that end or destroy something.
    Destructive,
}

/// The pill button: primary fills the accent, destructive wears the
/// alarm, ghost is a word that hovers. An optional glyph rides the
/// label; icons where a word would shout.
pub fn button(
    id: impl Into<SharedString>,
    label: &str,
    icon: Option<&'static str>,
    variant: ButtonVariant,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    let (fg, glyph) = match variant {
        ButtonVariant::Primary => (ACCENT_TEXT, ACCENT_TEXT),
        ButtonVariant::Ghost => (ACCENT, ACCENT),
        ButtonVariant::Destructive => (ACCENT_TEXT, ACCENT_TEXT),
    };
    let base = div()
        .id(id.into())
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_sm()
        .text_size(px(11.))
        .text_color(rgb(fg))
        .cursor_pointer();
    let base = match variant {
        ButtonVariant::Primary => base.bg(rgb(ACCENT)),
        ButtonVariant::Ghost => base.hover(|el| el.bg(rgb(SURFACE))),
        ButtonVariant::Destructive => base.bg(rgb(URGENT)),
    };
    base.children(icon.map(|path| gpui::svg().path(path).size(px(11.)).text_color(rgb(glyph))))
        .child(label.to_string())
        .on_click(on_click)
}

/// The icon-only button: a quiet glyph for row actions (reorder,
/// remove) where a label would crowd the row. Ghost sits dim and
/// brightens on hover; destructive wears the alarm at rest, there is
/// no rest for that act.
pub fn icon_button(
    id: impl Into<SharedString>,
    icon: &'static str,
    variant: ButtonVariant,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    let (rest, hover) = match variant {
        ButtonVariant::Destructive => (URGENT, URGENT),
        _ => (TEXT_DIM, TEXT),
    };
    div()
        .id(id.into())
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .text_color(rgb(rest))
        .hover(move |el| el.bg(rgb(SURFACE)).text_color(rgb(hover)))
        .child(gpui::svg().path(icon).size(px(13.)).text_color(rgb(rest)))
        .on_click(on_click)
}
