//! The app dock: pinned favorites plus running windows, its own layer-shell
//! surface at a screen edge. Click cycles an app's windows (or focuses its
//! first), right-click pins/unpins, and the surface syncs with the dock
//! settings (enable/disable and position) live.

use std::collections::HashMap;
use std::path::Path;

use gpui::{
    App, AppContext, Bounds, Context, Div, Entity, ImageSource, Render, SharedString, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, img,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    point,
    prelude::*,
    px, rgb, rgba, size, svg,
};

use crate::imaging::IconImage;
use crate::launcher::{AppEntry, launch, load_apps};
use crate::session::{SessionState, SessionWindow};
use crate::settings::{DockPosition, Settings};
use crate::theme::*;

/// Icon cell and gutter: the dock's size language.
const CELL: f32 = 48.;
const GAP: f32 = 8.;
const PADDING: f32 = 8.;
/// The window's fixed axis (64 = 48 + 2×8 padding).
const STRIP: f32 = CELL + 2. * PADDING;

/// The always-first fixture: opens the launcher panel. Not a pin, not
/// draggable: the dock's launch path.
const LAUNCHER_KEY: &str = "__kuma-launcher__";

/// The drag payload between dock cells: what's being carried, plus enough
/// to render the ghost (the icon itself).
#[derive(Clone)]
struct DockDrag {
    key: String,
    label: String,
    icon: Option<IconImage>,
}

/// What follows the cursor during a dock drag: the app's icon and label.
struct DragGhost {
    label: String,
    icon: Option<IconImage>,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgba(PANEL_BG))
            .border_1()
            .border_color(rgb(DIVIDER))
            .child(match &self.icon {
                Some(IconImage::Raster(raster)) => img(ImageSource::Render(raster.clone()))
                    .size(px(24.))
                    .into_any_element(),
                Some(IconImage::Svg(bytes)) => svg().data(bytes).size(px(24.)).into_any_element(),
                None => svg()
                    .path("icons/dock.svg")
                    .size(px(24.))
                    .text_color(rgb(TEXT_DIM))
                    .into_any_element(),
            })
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(TEXT))
                    .child(self.label.clone()),
            )
    }
}

/// One dock entry: an app (matched to a desktop file) with its windows, or
/// an unmatched running window shown as a raw entry.
#[derive(Clone, Debug, PartialEq)]
pub struct DockEntry {
    /// The desktop-file path (matched apps) or raw app_id (unmatched).
    pub key: String,
    pub label: String,
    /// niri window ids, ascending.
    pub windows: Vec<u64>,
    /// The window of this entry that currently holds focus, if any.
    pub focused_window: Option<u64>,
    pub pinned: bool,
    /// (exec, terminal) for launching; None for unmatched raw entries.
    pub exec: Option<(String, bool)>,
}

impl DockEntry {
    pub fn is_running(&self) -> bool {
        !self.windows.is_empty()
    }
}

/// Which window a click should focus: the app's first window, unless one of
/// its windows already holds focus, then the next around the cycle.
fn cycle_target(windows: &[u64], focused: Option<u64>) -> Option<u64> {
    let first = windows.first().copied()?;
    Some(
        match focused.and_then(|current| windows.iter().position(|&id| id == current)) {
            Some(position) => windows[(position + 1) % windows.len()],
            None => first,
        },
    )
}

/// The dock's list: pinned apps in pin order first (present even when not
/// running), then everything else in first-seen order. Windows join their
/// app's entry by app_id match; unmatched windows group as raw entries.
/// Pure: the surface renders it, the tests pin it down.
pub fn dock_entries(
    apps: &[AppEntry],
    windows: &[&SessionWindow],
    focused_window: Option<u64>,
    pinned: &[String],
) -> Vec<DockEntry> {
    let mut entries: Vec<DockEntry> = Vec::new();

    // pinned skeletons, in pin order
    for key in pinned {
        if let Some(app) = apps.iter().find(|app| &app.desktop_path == key)
            && !entries.iter().any(|entry| &entry.key == key)
        {
            entries.push(DockEntry {
                key: app.desktop_path.clone(),
                label: app.name.clone(),
                windows: Vec::new(),
                focused_window: None,
                pinned: true,
                exec: Some((app.exec.clone(), app.terminal)),
            });
        }
    }

    // windows join their entries in first-seen order
    for window in windows {
        let matched = window
            .app_id
            .as_deref()
            .and_then(|app_id| match_app(apps, app_id));
        let (key, label, exec) = match matched {
            Some(app) => (
                app.desktop_path.clone(),
                app.name.clone(),
                Some((app.exec.clone(), app.terminal)),
            ),
            None => {
                let app_id = window
                    .app_id
                    .clone()
                    .unwrap_or_else(|| "window".to_string());
                (app_id.clone(), app_id, None)
            }
        };
        match entries.iter().position(|entry| entry.key == key) {
            Some(index) => {
                entries[index].windows.push(window.id);
                if focused_window == Some(window.id) {
                    entries[index].focused_window = Some(window.id);
                }
            }
            None => entries.push(DockEntry {
                key,
                label,
                windows: vec![window.id],
                focused_window: focused_window.filter(|id| *id == window.id),
                pinned: false,
                exec,
            }),
        }
    }

    entries
}

/// app_id → desktop entry: exact desktop-file stem first, name as fallback.
fn match_app<'a>(apps: &'a [AppEntry], app_id: &str) -> Option<&'a AppEntry> {
    apps.iter()
        .find(|app| {
            Path::new(&app.desktop_path)
                .file_stem()
                .is_some_and(|stem| stem == app_id)
        })
        .or_else(|| {
            apps.iter()
                .find(|app| app.name.eq_ignore_ascii_case(app_id))
        })
}

/// The dock's window: always stretched along its screen edge (the bar's
/// pattern: sized layer surfaces never learn their own screen position,
/// but a stretched one's coordinates along the stretch axis ARE screen
/// coordinates, which the context menu's placement depends on). The visible
/// card is centered by the view; the input region covers only the card.
/// A 0 dimension on the doubly-anchored axis lets the compositor stretch.
pub fn dock_window_options(position: DockPosition) -> WindowOptions {
    let (anchor, width, height) = match position {
        DockPosition::Bottom => (Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT, 0., STRIP),
        DockPosition::Top => (Anchor::TOP | Anchor::LEFT | Anchor::RIGHT, 0., STRIP),
        DockPosition::Left => (Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM, STRIP, 0.),
        DockPosition::Right => (Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM, STRIP, 0.),
    };
    WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(width), px(height)),
        })),
        app_id: Some("kuma-shell-dock".into()),
        window_background: WindowBackgroundAppearance::Transparent,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: "kuma-shell-dock".into(),
            layer: Layer::Top,
            exclusive_zone: Some(px(STRIP)),
            anchor,
            keyboard_interactivity: KeyboardInteractivity::None,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Owns the dock window's lifecycle: opened iff enabled, recreated when the
/// position changes (a layer surface's anchor is fixed at creation).
pub struct DockHost {
    window: Option<WindowHandle<DockView>>,
    position: DockPosition,
}

impl DockHost {
    fn sync(
        &mut self,
        niri: &Entity<SessionState>,
        settings: &Entity<Settings>,
        cx: &mut Context<Self>,
    ) {
        let (enabled, position) = {
            let settings = settings.read(cx);
            (settings.dock.enabled, settings.dock.position)
        };
        // with no displays there is nothing to attach to: the compositor
        // closes the surface immediately, so wait for the surfaces watch
        // to call again when one appears
        if cx.displays().is_empty() {
            return;
        }
        // a compositor-side close (the output the dock was on went away)
        // leaves a dead handle: the probe reads it as gone so this sync
        // recreates instead of guarding a ghost
        if !self
            .window
            .as_ref()
            .is_some_and(|window| window.update(cx, |_, _, _| {}).is_ok())
        {
            self.window = None;
        }
        if !enabled {
            self.close(cx);
            return;
        }
        if self.window.is_some() && self.position == position {
            return;
        }
        self.close(cx);
        log::info!("dock: recreating window at {position:?}");
        let niri = niri.clone();
        let settings = settings.clone();
        match cx.open_window(dock_window_options(position), |_, cx| {
            cx.new(|cx| DockView::new(niri, settings, cx))
        }) {
            Ok(handle) => {
                self.window = Some(handle);
                self.position = position;
            }
            Err(err) => log::error!("failed to open dock window: {err:#}"),
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.window.take() {
            if let Err(err) = handle.update(cx, |_, window, _| window.remove_window()) {
                log::error!("closing dock window failed: {err:#}");
            }
        }
    }
}

/// Keeps the `DockHost` entity alive for the whole session: entities drop
/// with their last handle, and a dropped host takes its settings observer
/// with it; position changes would flip the view's orientation while the
/// window itself never moved. The field is intentionally never read: holding
/// the handle is the whole job.
struct DockHostGlobal(#[allow(dead_code)] Entity<DockHost>);
impl gpui::Global for DockHostGlobal {}
/// Start the dock: one host entity observing settings, syncing the window.
pub fn run(niri: Entity<SessionState>, settings: Entity<Settings>, cx: &mut App) {
    let observer_niri = niri.clone();
    let host = cx.new(|cx| {
        cx.observe(
            &settings,
            move |this: &mut DockHost, settings: Entity<Settings>, cx| {
                this.sync(&observer_niri, &settings, cx);
            },
        )
        .detach();
        DockHost {
            window: None,
            position: DockPosition::default(),
        }
    });
    cx.set_global(DockHostGlobal(host.clone()));
    let _ = host.update(cx, |host, cx| host.sync(&niri, &settings, cx));
}

/// The surfaces watch's dock arm: re-run the sync, which no-ops while
/// the window is alive and recreates it when it died with its output.
pub fn ensure(niri: &Entity<SessionState>, settings: &Entity<Settings>, cx: &mut App) {
    let Some(host) = cx
        .try_global::<DockHostGlobal>()
        .map(|global| global.0.clone())
    else {
        return;
    };
    let _ = host.update(cx, |host, cx| host.sync(niri, settings, cx));
}

/// The dock surface: a rounded card of entries centered on a stretched,
/// otherwise transparent strip at the screen edge.
pub struct DockView {
    niri: Entity<SessionState>,
    settings: Entity<Settings>,
    apps: Vec<AppEntry>,
    icons: HashMap<String, Option<IconImage>>,
    /// The card's last-known rect (offset along the stretch axis, length):
    /// gates the input region update.
    applied_card: Option<(f32, f32)>,
}

impl DockView {
    pub fn new(
        niri: Entity<SessionState>,
        settings: Entity<Settings>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&niri, |_, _, cx| cx.notify()).detach();
        cx.observe(&settings, |_, _, cx| cx.notify()).detach();

        let apps = load_apps();
        let icon_keys: Vec<(String, String)> = apps
            .iter()
            .map(|app| (app.desktop_path.clone(), app.icon.clone()))
            .collect();
        cx.spawn(async move |this, cx| {
            // the cache makes recreations instant; the first build walks
            // the theme once, in the background
            let decoded: Vec<(String, Option<IconImage>)> = cx
                .background_spawn(async move {
                    icon_keys
                        .into_iter()
                        .map(|(desktop_path, icon)| {
                            (desktop_path, crate::imaging::cached_icon(&icon))
                        })
                        .collect()
                })
                .await;
            for (desktop_path, icon) in decoded {
                let _ = this.update(cx, |this, cx| {
                    this.icons.insert(desktop_path, icon);
                    cx.notify();
                });
            }
        })
        .detach();

        Self {
            niri,
            settings,
            apps,
            icons: HashMap::new(),
            applied_card: None,
        }
    }
}

impl Render for DockView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings.read(cx);
        let position = settings.dock.position;
        let pinned = settings.dock.pinned.clone();

        let niri = self.niri.read(cx);
        let windows = niri.windows();
        let focused = niri.focused_window().map(|window| window.id);

        let entries = dock_entries(&self.apps, &windows, focused, &pinned);
        let count = entries.len() + 1; // the launcher fixture leads, always
        let mut all_entries = vec![launcher_entry()];
        all_entries.extend(entries);

        // the card is centered on the stretched strip; window coordinates
        // along the stretch axis are screen coordinates
        let length =
            (count as f32 * CELL + count.saturating_sub(1) as f32 * GAP + 2. * PADDING).max(16.);
        let viewport = window.viewport_size();
        let card_offset = if position.is_horizontal() {
            ((viewport.width - px(length)) / 2.).max(px(0.))
        } else {
            ((viewport.height - px(length)) / 2.).max(px(0.))
        };
        let card = (f32::from(card_offset), length);
        if self.applied_card != Some(card) && viewport.width > px(0.) {
            // only the card takes input; the transparent stretch passes clicks
            window.set_input_region(Some(&[if position.is_horizontal() {
                Bounds {
                    origin: point(card_offset, px(0.)),
                    size: size(px(length), px(STRIP)),
                }
            } else {
                Bounds {
                    origin: point(px(0.), card_offset),
                    size: size(px(STRIP), px(length)),
                }
            }]));
            self.applied_card = Some(card);
        }

        let horizontal = position.is_horizontal();

        div().id("dock").size_full().relative().child(
            div()
                .absolute()
                .when(horizontal, |el| {
                    el.top(px(0.)).left(card_offset).w(px(length)).h(px(STRIP))
                })
                .when(!horizontal, |el| {
                    el.top(card_offset).left(px(0.)).w(px(STRIP)).h(px(length))
                })
                .flex()
                .when(horizontal, |el| el.flex_row())
                .when(!horizontal, |el| el.flex_col())
                .items_center()
                .gap(px(GAP))
                .p(px(PADDING))
                .rounded_xl()
                .bg(rgba(PANEL_BG))
                .border_1()
                .border_color(rgb(DIVIDER))
                .overflow_hidden()
                .children(
                    all_entries
                        .into_iter()
                        .map(|entry| dock_icon(entry, &self.icons, position, cx)),
                ),
        )
    }
}

/// The launcher fixture: the dock's permanent first cell.
fn launcher_entry() -> DockEntry {
    DockEntry {
        key: LAUNCHER_KEY.to_string(),
        label: "Apps".to_string(),
        windows: Vec::new(),
        focused_window: None,
        pinned: false,
        exec: None,
    }
}

/// The right-click target on a dock cell, consumed by [`DockMenuView`].
#[derive(Clone, Debug)]
pub struct DockMenuContext {
    pub key: String,
    pub label: String,
    pub pinned: bool,
}

/// The right-click menu on a dock cell: "Pin to dock" (or unpin) hanging
/// beside the dock at the pointer. A small plain card (the toast's
/// language): menus are transient, not drawers.
pub struct DockMenuView {
    settings: Entity<Settings>,
    context: Option<DockMenuContext>,
}

impl DockMenuView {
    pub fn new(
        context: Option<DockMenuContext>,
        settings: Entity<Settings>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
        _geometry: crate::panel::PanelGeometry,
    ) -> Self {
        Self { settings, context }
    }
}

impl Render for DockMenuView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("dock-menu")
            .size_full()
            .flex()
            .flex_col()
            .p(px(4.))
            .rounded_xl()
            .bg(rgba(PANEL_BG))
            .border_1()
            .border_color(rgb(DIVIDER))
            .when_some(self.context.clone(), |el, context| {
                let settings = self.settings.clone();
                el.child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(TEXT_DIM))
                        .px_2()
                        .py_1()
                        .truncate()
                        .child(context.label),
                )
                .child({
                    let key = context.key.clone();
                    let pinned = context.pinned;
                    menu_row(
                        if context.pinned {
                            "Unpin from dock"
                        } else {
                            "Pin to dock"
                        },
                        move |cx| {
                            settings.update(cx, |settings, cx| {
                                if pinned {
                                    settings.dock_unpin(&key, cx);
                                } else {
                                    settings.dock_pin(&key, cx);
                                }
                            });
                            crate::panel::close_panels(cx);
                        },
                    )
                })
            })
    }
}

/// One menu row: hover-tinted, click runs the action and closes the menu.
fn menu_row(label: &'static str, on_click: impl Fn(&mut App) + 'static) -> gpui::Stateful<Div> {
    div()
        .id(SharedString::from(label.replace(' ', "-")))
        .px_2()
        .py_1p5()
        .rounded_md()
        .cursor_pointer()
        .hover(|el| el.bg(rgb(SURFACE)))
        .text_size(px(12.))
        .text_color(rgb(TEXT))
        .on_click(move |_, _, cx| on_click(cx))
        .child(label)
}

/// Open the dock context menu beside the dock at the pointer. `x`/`y` are
/// the pointer in dock-window coordinates.
pub fn open_dock_menu(context: DockMenuContext, x: f32, y: f32, dock: DockPosition, cx: &mut App) {
    cx.global_mut::<crate::panel::PanelHost>()
        .set_dock_menu(Some(context));
    crate::panel::toggle_panel_at(
        crate::panel::PanelKind::DockMenu,
        x,
        y,
        dock,
        STRIP + 8.,
        cx,
    );
}

/// One dock cell: icon, running dot, click-to-cycle-or-launch, right-click
/// for the pin menu, drag to reorder. The launcher fixture is a special
/// cell: it opens the launcher panel and neither drags nor receives drops.
fn dock_icon(
    entry: DockEntry,
    icons: &HashMap<String, Option<IconImage>>,
    dock: DockPosition,
    cx: &mut Context<DockView>,
) -> gpui::Stateful<Div> {
    let DockEntry {
        key,
        label,
        windows,
        focused_window,
        pinned,
        exec,
    } = entry;
    let is_fixture = key == LAUNCHER_KEY;
    let running = !windows.is_empty();
    let focused = focused_window.is_some();
    let icon = icons.get(&key).cloned().flatten();

    let click_windows = windows.clone();
    let click_focused = focused_window;
    let click_exec = exec.clone();
    let aux_exec = exec;
    let pin_key = key.clone();
    let pin_label = label.clone();
    let pin_pinned = pinned;
    let pin_dock = dock;

    let tooltip = if is_fixture {
        "Apps".to_string()
    } else if windows.is_empty() {
        label.clone()
    } else {
        format!(
            "{label} ({} window{})",
            windows.len(),
            if windows.len() == 1 { "" } else { "s" }
        )
    };

    let drag_payload = if is_fixture {
        None
    } else {
        Some(DockDrag {
            key: key.clone(),
            label: label.clone(),
            icon: icon.clone(),
        })
    };

    let mut cell = div()
        .id(gpui::SharedString::from(format!("dock-{key}")))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .size(px(CELL))
        .rounded_xl()
        .cursor_pointer()
        .hover(|el| el.bg(rgb(SURFACE_HOVER)))
        .when(focused, |el| el.bg(rgb(SURFACE)))
        .tooltip(crate::panel_kit::text_tooltip(tooltip.into()))
        .on_click(cx.listener(move |_, _, _, cx| {
            if is_fixture {
                crate::panel::toggle_panel(crate::panel::PanelKind::Launcher, cx);
            } else if let Some(target) = cycle_target(&click_windows, click_focused) {
                cx.background_spawn(async move {
                    if let Err(err) = crate::session::focus_window(target) {
                        log::error!("focus-window failed: {err:#}");
                    }
                })
                .detach();
            } else if let Some((exec, terminal)) = click_exec.clone()
                && let Err(err) = launch(&exec, terminal)
            {
                log::error!("launch {exec} failed: {err:#}");
            }
        }))
        .on_aux_click(cx.listener(move |_, event: &gpui::ClickEvent, _, cx| {
            // aux covers middle and right; only right opens the menu, and
            // raw unmatched entries have no desktop file to pin
            if !matches!(
                event,
                gpui::ClickEvent::Mouse(e) if e.down.button == gpui::MouseButton::Right
            ) || aux_exec.is_none()
            {
                return;
            }
            let context = DockMenuContext {
                key: pin_key.clone(),
                label: pin_label.clone(),
                pinned: pin_pinned,
            };
            let (x, y) = (f32::from(event.position().x), f32::from(event.position().y));
            // the menu hangs beside the dock at the cell; which axis it
            // centers on depends on the dock's edge
            open_dock_menu(context, x, y, pin_dock, cx);
        }))
        .child(match icon {
            _ if is_fixture => svg()
                .path("icons/apps.svg")
                .size(px(32.))
                .text_color(rgb(TEXT))
                .into_any_element(),
            Some(IconImage::Raster(raster)) => img(ImageSource::Render(raster))
                .size(px(32.))
                .into_any_element(),
            Some(IconImage::Svg(bytes)) => svg().data(&bytes).size(px(32.)).into_any_element(),
            None => svg()
                .path("icons/dock.svg")
                .size(px(32.))
                .text_color(rgb(TEXT_DIM))
                .into_any_element(),
        });

    if running {
        cell = cell.child(div().size(px(4.)).rounded_full().bg(rgb(if focused {
            ACCENT
        } else {
            TEXT_DIM
        })));
    }

    // drag to reorder (fixture cells don't participate)
    if let Some(payload) = drag_payload {
        cell = cell
            .on_drag(payload, move |drag: &DockDrag, _, _, cx| {
                cx.new(|_| DragGhost {
                    label: drag.label.clone(),
                    icon: drag.icon.clone(),
                })
            })
            .on_drop::<DockDrag>(cx.listener({
                let drop_key = key.clone();
                move |this, drag: &DockDrag, _, cx| {
                    this.settings.update(cx, |settings, cx| {
                        settings.dock_reorder(&drag.key, &drop_key, cx);
                    });
                }
            }));
    }
    cell
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(desktop_path: &str, name: &str, exec: &str) -> AppEntry {
        AppEntry {
            name: name.to_string(),
            exec: exec.to_string(),
            terminal: false,
            icon: String::new(),
            desktop_path: desktop_path.to_string(),
            usage: 0,
        }
    }

    fn window(id: u64, app_id: Option<&str>, focused: bool) -> SessionWindow {
        SessionWindow {
            id,
            title: Some(format!("win-{id}")),
            app_id: app_id.map(String::from),
            workspace_id: Some(1),
            is_focused: focused,
            is_urgent: false,
        }
    }

    #[test]
    fn pinned_lead_the_list_even_when_not_running() {
        let apps = vec![
            app(
                "/usr/share/applications/firefox.desktop",
                "Firefox",
                "firefox",
            ),
            app(
                "/usr/share/applications/org.gnome.Nautilus.desktop",
                "Files",
                "nautilus",
            ),
        ];
        let entries = dock_entries(
            &apps,
            &[],
            None,
            &["/usr/share/applications/firefox.desktop".to_string()],
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "/usr/share/applications/firefox.desktop");
        assert!(entries[0].pinned);
        assert!(!entries[0].is_running());
        assert!(entries[0].exec.is_some());
    }

    #[test]
    fn windows_join_entries_by_stem_then_name() {
        let apps = vec![
            app(
                "/usr/share/applications/org.gnome.Nautilus.desktop",
                "Files",
                "nautilus",
            ),
            app("/usr/share/applications/kitty.desktop", "kitty", "kitty"),
        ];
        // org.gnome.Nautilus app_id matches by desktop-file stem
        let raw_windows = vec![
            window(1, Some("org.gnome.Nautilus"), false),
            window(2, Some("kitty"), true),
        ];
        let windows: Vec<&SessionWindow> = raw_windows.iter().collect();
        let entries = dock_entries(&apps, &windows, Some(2), &[]);
        assert_eq!(entries.len(), 2);
        let nautilus = entries.iter().find(|e| e.label == "Files").unwrap();
        assert_eq!(nautilus.windows, vec![1]);
        let kitty = entries.iter().find(|e| e.label == "kitty").unwrap();
        assert_eq!(kitty.windows, vec![2]);
        assert_eq!(kitty.focused_window, Some(2));
    }

    #[test]
    fn unmatched_windows_group_as_raw_entries() {
        let raw_windows = vec![
            window(3, Some("mystery"), false),
            window(4, Some("mystery"), false),
            window(5, None, false),
        ];
        let windows: Vec<&SessionWindow> = raw_windows.iter().collect();
        let entries = dock_entries(&[], &windows, None, &[]);
        assert_eq!(entries.len(), 2);
        let mystery = entries.iter().find(|e| e.key == "mystery").unwrap();
        assert_eq!(mystery.windows, vec![3, 4]);
        assert!(mystery.exec.is_none());
        // no app_id at all: one bucket
        let unnamed = entries.iter().find(|e| e.key == "window").unwrap();
        assert_eq!(unnamed.windows, vec![5]);
    }

    #[test]
    fn click_cycles_through_an_apps_windows() {
        let windows = vec![10, 11, 12];
        assert_eq!(cycle_target(&windows, None), Some(10));
        assert_eq!(cycle_target(&windows, Some(10)), Some(11));
        assert_eq!(cycle_target(&windows, Some(12)), Some(10)); // wraps
        assert_eq!(cycle_target(&[], None), None);
        // focused window not in the list: fall back to first
        assert_eq!(cycle_target(&windows, Some(99)), Some(10));
    }
}
