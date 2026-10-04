//! The panel kit: the one surface vocabulary every panel shares, the
//! shape the nostr panel's panes wore first and the rest of the shell
//! now borrows. A panel is a rail or sidebar, a pane header, and a
//! column of cards; the pieces here are the card, the button, the
//! empty state, and the header, so no panel grows its own dialect.

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    AnyView, App, ClickEvent, Context, Div, Entity, FontWeight, IntoElement, ParentElement,
    Render, SharedString, Window, div, px, rgb, rgba,
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
        .bg(rgb(crate::theme::current().surface))
}

/// A card's title: the 13px semibold line every card opens with, over
/// its 11px dim description (when it has one).
pub fn card_title(title: &str) -> Div {
    div()
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(crate::theme::current().text))
        .child(title.to_string())
}

/// A card's dim description line: the sentence under a card title
/// that says what the control below it does.
pub fn card_note(note: &str) -> Div {
    div()
        .text_size(px(11.))
        .text_color(rgb(crate::theme::current().text_dim))
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
        .bg(rgb(crate::theme::current().surface))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::current().text))
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
            .text_color(rgb(crate::theme::current().text))
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
        .child(div().h(px(1.)).w_full().bg(rgba(crate::theme::current().divider_soft)))
        .child(body)
}

/// The count badge that rides a header or a rail tab: a filled accent
/// pill for the number of things waiting on the person.
pub fn count_badge(count: usize) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .bg(rgb(crate::theme::current().accent))
        .text_size(px(11.))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(crate::theme::current().accent_text))
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
            .text_color(rgb(crate::theme::current().text))
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
        .when(active, |el| el.bg(rgb(crate::theme::current().surface)))
        .hover(|el| el.bg(rgb(crate::theme::current().surface)))
        .tooltip(text_tooltip(label.into()))
        .on_click(on_click)
        .child(
            gpui::svg()
                .path(icon)
                .size(px(16.))
                .text_color(rgb(if active { crate::theme::current().accent } else { crate::theme::current().text_dim })),
        )
        .children(badge.map(|count| {
            div()
                .text_size(px(10.))
                .text_color(rgb(if active { crate::theme::current().accent } else { crate::theme::current().text_dim }))
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
                .text_color(rgb(crate::theme::current().text))
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::current().text_dim))
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
        ButtonVariant::Primary => (crate::theme::current().accent_text, crate::theme::current().accent_text),
        ButtonVariant::Ghost => (crate::theme::current().accent, crate::theme::current().accent),
        ButtonVariant::Destructive => (crate::theme::current().accent_text, crate::theme::current().accent_text),
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
        ButtonVariant::Primary => base.bg(rgb(crate::theme::current().accent)),
        ButtonVariant::Ghost => base.hover(|el| el.bg(rgb(crate::theme::current().surface))),
        ButtonVariant::Destructive => base.bg(rgb(crate::theme::URGENT)),
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
        ButtonVariant::Destructive => (crate::theme::URGENT, crate::theme::URGENT),
        _ => (crate::theme::current().text_dim, crate::theme::current().text),
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
        .hover(move |el| el.bg(rgb(crate::theme::current().surface)).text_color(rgb(hover)))
        .child(gpui::svg().path(icon).size(px(13.)).text_color(rgb(rest)))
        .on_click(on_click)
}

/// How long an armed confirm row waits before it disarms itself.
const CONFIRM_DISARM: Duration = Duration::from_secs(5);

/// One act in a confirm row: what it says at rest, what it says armed,
/// and what fires on the second click.
pub struct ConfirmAction {
    pub id: &'static str,
    /// The label at rest.
    pub label: &'static str,
    /// The label while armed: the question the second click answers.
    pub confirm_label: &'static str,
    pub icon: Option<&'static str>,
    /// Runs with the App, only on the confirm click.
    pub on_confirm: Arc<dyn Fn(&mut App) + 'static>,
}

impl ConfirmAction {
    pub fn new(
        id: &'static str,
        label: &'static str,
        confirm_label: &'static str,
        icon: Option<&'static str>,
        on_confirm: impl Fn(&mut App) + 'static,
    ) -> Self {
        Self {
            id,
            label,
            confirm_label,
            icon,
            on_confirm: Arc::new(on_confirm),
        }
    }
}

/// The arm-then-confirm row: the kit piece for acts that end something.
/// Swap-and-dim: the clicked button swaps its label for the confirm
/// phrase and fills the alarm, the row's other buttons dim and ignore
/// clicks, and the arm expires after 5s. One armed act at a time.
///
/// The armed state lives in the row's own view, so it dies with the
/// panel: closing the panel (or Esc, which closes it) disarms for
/// free, no listener wiring on the host.
pub struct ConfirmActions {
    actions: Vec<ConfirmAction>,
    armed: Option<usize>,
    /// Bumped on every arm; a pending timer only disarms the arm it
    /// was minted for, so re-arming outlives the stale wake.
    arm: u64,
}

/// The arm-then-confirm row, minted as its own view. Render the
/// entity where the buttons belong; it brings its own flex row.
pub fn confirm_actions(
    actions: Vec<ConfirmAction>,
    cx: &mut App,
) -> Entity<ConfirmActions> {
    cx.new(|_| ConfirmActions {
        actions,
        armed: None,
        arm: 0,
    })
}

impl ConfirmActions {
    fn arm(&mut self, index: usize, cx: &mut Context<Self>) {
        self.armed = Some(index);
        self.arm += 1;
        let stamp = self.arm;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CONFIRM_DISARM).await;
            let _ = this.update(cx, |this, cx| {
                if this.arm == stamp {
                    this.armed = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for ConfirmActions {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let armed = self.armed;
        div().flex().flex_wrap().gap_2().children(
            self.actions.iter().enumerate().map(|(index, action)| {
                let id = format!("confirm-{}", action.id);
                match armed {
                    // the armed act: fills the alarm, asks its question
                    Some(a) if a == index => button(
                        id,
                        action.confirm_label,
                        action.icon,
                        ButtonVariant::Destructive,
                        cx.listener(move |this, _, _, cx| {
                            if this.armed == Some(index) {
                                this.armed = None;
                                (this.actions[index].on_confirm.clone())(cx);
                                cx.notify();
                            }
                        }),
                    ),
                    // the row's other acts: dimmed, deaf while one is armed
                    Some(_) => button(id, action.label, action.icon, ButtonVariant::Ghost, {
                        let _ = cx;
                        move |_, _, _| {}
                    })
                    .text_color(rgb(crate::theme::current().text_dim))
                    .bg(rgb(crate::theme::current().surface)),
                    // at rest: quiet words that hover
                    None => button(
                        id,
                        action.label,
                        action.icon,
                        ButtonVariant::Ghost,
                        cx.listener(move |this, _, _, cx| this.arm(index, cx)),
                    ),
                }
            }),
        )
    }
}
