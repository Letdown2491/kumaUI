//! The family's client-side title bar. gpui claims client-side
//! decorations on Wayland and draws nothing itself; it hands out the
//! primitives instead (the drag, the zoom, the compositor's
//! wm_capabilities). This module is the shared bar on top of them
//! (ADR-0017): a drag region carrying the app's name, and the button
//! cluster the compositor backs. The platform owns the behavior and
//! the geometry; the app owns the colors, the name, and the glyphs,
//! so the bar wears each app's look without the platform growing a
//! palette or an asset set.

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    div, px, svg, Div, InteractiveElement, MouseButton, MouseDownEvent, ParentElement, Point,
    Pixels, Rgba, SharedString, Stateful, StatefulInteractiveElement, Styled, Tiling, Window,
    WindowControls,
};

/// The bar's height: Zed's own Linux bar number, 1.75rem floored at
/// 34px.
pub const BAR_HEIGHT: f32 = 34.;

/// The double-click window for maximize on a second press, GTK's
/// default.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// How far the second press may drift from the first (in logical
/// pixels) and still count as a double-click.
const DOUBLE_CLICK_DRIFT: f64 = 6.;

/// The bar's colors, from the app's own theme (the platform reads no
/// palette of its own; see the `palette` module for where the values
/// come from).
#[derive(Clone, Copy)]
pub struct TitlebarColors {
    /// The bar's backing.
    pub background: Rgba,
    /// The hairline under the bar.
    pub border: Rgba,
    /// The title's text.
    pub title: Rgba,
    /// The glyphs' ink, resting and hovered (the hover speaks through
    /// the backdrop, Adwaita's way, not through the ink).
    pub ink: Rgba,
    /// The hover backdrop.
    pub hover: Rgba,
}

/// The glyph assets, as the app's asset-source paths. Rendered tinted
/// by the button's ink.
#[derive(Clone)]
pub struct TitlebarIcons {
    pub minimize: SharedString,
    pub maximize: SharedString,
    pub close: SharedString,
}

/// One of the bar's buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Minimize,
    Maximize,
    Close,
}

/// The buttons the compositor's `wm_capabilities` backs, in the
/// conventional order (minimize, maximize, close, left to right).
/// Close is always offered: a window without an exit is a trap,
/// whatever the compositor claims.
pub fn buttons_for(controls: WindowControls) -> Vec<Control> {
    let mut buttons = Vec::with_capacity(3);
    if controls.minimize {
        buttons.push(Control::Minimize);
    }
    if controls.maximize {
        buttons.push(Control::Maximize);
    }
    buttons.push(Control::Close);
    buttons
}

/// Whether the window is currently edge-tiled, fullscreened, or
/// maximized. Read off the xdg-toplevel states the compositor sends
/// on every configure: a floating window carries none of the tiled
/// edges, and the backend reports fullscreen and maximize the same
/// way (there is no separate signal to read). The family folds its
/// bars on this: a tile frames the window already, the drag region
/// has nothing to do there, and the content wants the row. Platforms
/// without the signal report server decorations, which reads as
/// never tiled, so the bar stays.
pub fn window_is_tiled(window: &Window) -> bool {
    match window.window_decorations() {
        gpui::Decorations::Client { tiling } => tiling != Tiling::default(),
        gpui::Decorations::Server => false,
    }
}

/// The bar: a full-width strip, the app's name dead center (the
/// headerbar convention), the buttons on the right. The title centers
/// truly because the bar is two equal flex halves with the title
/// between them; the buttons ride inside the right half's far edge.
/// Everything not a button is drag surface; a second press inside the
/// double-click window maximizes instead of moving (GTK's order,
/// because once the move grab leaves for the compositor the click
/// never lands); a right press opens the compositor's window menu.
/// Render it as the window's first child.
pub fn bar(
    title: SharedString,
    controls: WindowControls,
    icons: TitlebarIcons,
    colors: TitlebarColors,
) -> Div {
    // Fn closures cannot carry mutable state, so the last press rides
    // a Cell shared by every drag region (double-click detection
    // reads and clears it wherever the press lands).
    let last_press: LastPress = Rc::new(Cell::new(None));

    let mut buttons = div().flex().h_full();
    for control in buttons_for(controls) {
        buttons = buttons.child(button(control, &icons, colors));
    }

    div()
        .w_full()
        .h(px(BAR_HEIGHT))
        .flex_none()
        .flex()
        .bg(colors.background)
        .border_b_1()
        .border_color(colors.border)
        .child(draggable(
            &last_press,
            div()
                .id("kuma-titlebar-drag")
                .debug_selector(|| "kuma-titlebar-drag".into())
                .flex_1()
                .h_full(),
        ))
        .child(draggable(
            &last_press,
            div()
                .flex_none()
                .h_full()
                .flex()
                .items_center()
                .px(px(12.))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.title)
                        .child(title),
                ),
        ))
        .child(
            div().flex_1().h_full().flex().child(draggable(
                &last_press,
                div().id("kuma-titlebar-drag-right").flex_1().h_full(),
            ))
            .child(buttons),
        )
}

/// The bar's press state, shared by its drag regions.
type LastPress = Rc<Cell<Option<(Instant, Point<Pixels>)>>>;

/// Attaches the drag behavior to a bar region: left press moves the
/// window (a second press inside the double-click window zooms
/// instead), right press opens the compositor's window menu.
fn draggable<E: InteractiveElement>(check: &LastPress, zone: E) -> E {
    zone.on_mouse_down(
        MouseButton::Left,
        {
            let check = Rc::clone(check);
            move |event: &MouseDownEvent, window, _| {
                let now = Instant::now();
                let repeat = check.take().is_some_and(|(at, at_position)| {
                    now.duration_since(at) <= DOUBLE_CLICK
                        && (event.position - at_position).magnitude() <= DOUBLE_CLICK_DRIFT
                });
                if repeat {
                    window.zoom_window();
                } else {
                    check.set(Some((now, event.position)));
                    window.start_window_move();
                }
            }
        },
    )
    .on_mouse_down(MouseButton::Right, |event: &MouseDownEvent, window, _| {
        window.show_window_menu(event.position);
    })
}

fn button(control: Control, icons: &TitlebarIcons, colors: TitlebarColors) -> Stateful<Div> {
    let (id, selector) = match control {
        Control::Minimize => ("kuma-titlebar-minimize", "kuma-titlebar-minimize"),
        Control::Maximize => ("kuma-titlebar-maximize", "kuma-titlebar-maximize"),
        Control::Close => ("kuma-titlebar-close", "kuma-titlebar-close"),
    };
    div()
        .id(id)
        .debug_selector(|| selector.into())
        .w(px(32.))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .hover(|style| style.bg(colors.hover))
        .on_click(move |_, window, _| match control {
            Control::Minimize => window.minimize_window(),
            Control::Maximize => window.zoom_window(),
            Control::Close => window.remove_window(),
        })
        .child(
            // the tint must sit on the svg itself: a leaf element does
            // not see the parent's text color, and without one it
            // paints nothing at all
            svg()
                .path(match control {
                    Control::Minimize => icons.minimize.clone(),
                    Control::Maximize => icons.maximize.clone(),
                    Control::Close => icons.close.clone(),
                })
                .text_color(colors.ink)
                .w(px(16.))
                .h(px(16.)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buttons_follow_the_compositors_capabilities() {
        let all = WindowControls {
            fullscreen: true,
            maximize: true,
            minimize: true,
            window_menu: true,
        };
        assert_eq!(buttons_for(all), vec![Control::Minimize, Control::Maximize, Control::Close]);

        let no_minimize = WindowControls { minimize: false, ..all };
        assert_eq!(buttons_for(no_minimize), vec![Control::Maximize, Control::Close]);

        let no_maximize = WindowControls { maximize: false, ..all };
        assert_eq!(buttons_for(no_maximize), vec![Control::Minimize, Control::Close]);

        let bare = WindowControls { minimize: false, maximize: false, ..all };
        assert_eq!(buttons_for(bare), vec![Control::Close], "close is always offered");
    }
}
