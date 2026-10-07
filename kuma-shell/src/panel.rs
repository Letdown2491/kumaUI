use std::sync::Arc;

use gpui::{
    AnyElement, App, Bounds, Context, Div, Element, ElementId, Global, GlobalElementId,
    InspectorElementId, KeyDownEvent, LayoutId, Pixels, Render, Size, Stateful, Style, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    point,
    prelude::*,
    px, rgba, size, svg,
};

pub const COVE: f32 = 16.;
pub const CORNER_RADIUS: f32 = 12.;

/// One continuous drawer silhouette: concave coves flaring out toward
/// the bar on one side, straight sides, convex rounded corners on the
/// other. `bar_at_top` faces the coves up (a top bar's panels open
/// downward); a bottom bar faces them down so the drawer opens upward.
pub fn drawer_silhouette(
    width: f32,
    height: f32,
    cove: f32,
    radius: f32,
    bar_at_top: bool,
) -> Arc<[u8]> {
    let body_left = cove;
    let body_right = width - cove;
    let path = if bar_at_top {
        let body_bottom = height - radius;
        format!(
            "M 0 0 A {c} {c} 0 0 1 {c} {c} L {bl} {bb} A {r} {r} 0 0 0 {bl2} {h} L {br2} {h} A {r} {r} 0 0 0 {br} {bb} L {br} {c} A {c} {c} 0 0 1 {w} 0 Z",
            c = cove,
            bl = body_left,
            bl2 = body_left + radius,
            bb = body_bottom,
            br2 = body_right - radius,
            br = body_right,
            r = radius,
            h = height,
            w = width,
        )
    } else {
        let body_top = radius;
        // the mirror of the top-hung path: y runs the other way, so
        // every arc's sweep flag flips too
        format!(
            "M 0 {h} A {c} {c} 0 0 0 {c} {c2} L {bl} {r2} A {r} {r} 0 0 1 {bl2} 0 L {br2} 0 A {r} {r} 0 0 1 {br} {r2} L {br} {c2} A {c} {c} 0 0 0 {w} {h} Z",
            c = cove,
            c2 = height - cove,
            bl = body_left,
            bl2 = body_left + radius,
            br2 = body_right - radius,
            br = body_right,
            r = radius,
            r2 = body_top,
            h = height,
            w = width,
        )
    };
    format!(
        r#"<svg viewBox="0 0 {w} {h}" xmlns="http://www.w3.org/2000/svg"><path d="{path}" fill="currentColor"/></svg>"#,
        w = width,
        h = height,
    )
    .into_bytes()
    .into()
}

/// Where the next panel opens. The bar centers panels on its content; a
/// widget's mini panel hangs under the bar at the widget; a dock cell's
/// menu hangs beside the dock at the cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PanelPlacement {
    /// Under the bar, centered on the bar content.
    Bar,
    /// Under the bar, centered at x: a widget's mini panel.
    Widget { x: f32 },
    /// Beside the dock: x/y are the pointer in dock-window coordinates
    /// (screen coordinates along the dock's long axis), `offset` is the
    /// distance from the dock's edge (strip + gap).
    At {
        x: f32,
        y: f32,
        dock: crate::settings::DockPosition,
        offset: f32,
    },
}

/// The geometry a panel view receives from the host; views never re-derive it.
/// `height` is the live surface height: panels wrapped in [`MeasureHeight`]
/// report their intrinsic height and the view refines this value (see
/// `CalendarView`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelGeometry {
    pub width: f32,
    pub height: f32,
    pub cove: f32,
}

/// Measures its child's laid-out height and reports it every frame at
/// prepaint. The alternative to hardcoded panel heights: the view's
/// `on_measure` decides what the number means (typically: refine the stored
/// `PanelGeometry.height` and resize the layer surface). Convergence is the
/// view's job: fire only on a real delta.
pub struct MeasureHeight {
    child: AnyElement,
    on_measure: Box<dyn Fn(f32, &mut Window, &mut App) + 'static>,
}

impl MeasureHeight {
    pub fn new(
        child: impl IntoElement,
        on_measure: impl Fn(f32, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            child: child.into_any_element(),
            on_measure: Box::new(on_measure),
        }
    }
}

impl IntoElement for MeasureHeight {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MeasureHeight {
    type RequestLayoutState = LayoutId; // the child's
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // pass-through layout node: the child is the only contribution
        let child_layout_id = self.child.request_layout(window, cx);
        let layout_id = window.request_layout(Style::default(), [child_layout_id], cx);
        (layout_id, child_layout_id)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        child_layout_id: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let child_height = f32::from(window.layout_bounds(*child_layout_id).size.height);
        (self.on_measure)(child_height, window, cx);
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

/// The Bar's live content rect and edge, reported by the bar view into
/// the host. Panels center on the content's center line and sit flush
/// against the bar's inner face (`bar_edge`), measured from the edge
/// the bar hangs from; `position` says which edge that is and which
/// way the drawer faces.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct BarGeometry {
    pub content_x: f32,
    pub content_width: f32,
    pub bar_edge: f32,
    pub position: crate::settings::BarPosition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKind {
    Settings,
    Launcher,
    Calendar,
    Notifications,
    Volume,
    Brightness,
    Mic,
    PowerProfile,
    Cpu,
    Ram,
    Temp,
    Disk,
    Battery,
    DockMenu,
    Nostr,
    Weather,
    Wifi,
    Bluetooth,
}

impl PanelKind {
    /// The single table of panel dimensions and keyboard modes.
    fn geometry(self) -> (f32, f32, KeyboardInteractivity) {
        match self {
            PanelKind::Settings => (560., 640., KeyboardInteractivity::OnDemand),
            PanelKind::Launcher => (560., 460., KeyboardInteractivity::Exclusive),
            PanelKind::Calendar => (400., 330., KeyboardInteractivity::OnDemand),
            PanelKind::Notifications => (560., 520., KeyboardInteractivity::OnDemand),
            PanelKind::Volume => (360., 150., KeyboardInteractivity::OnDemand),
            PanelKind::Brightness => (360., 150., KeyboardInteractivity::OnDemand),
            PanelKind::Mic => (360., 150., KeyboardInteractivity::OnDemand),
            PanelKind::PowerProfile => (280., 170., KeyboardInteractivity::OnDemand),
            PanelKind::Cpu => (320., 220., KeyboardInteractivity::OnDemand),
            PanelKind::Ram | PanelKind::Temp | PanelKind::Disk => {
                (320., 220., KeyboardInteractivity::OnDemand)
            }
            PanelKind::Battery => (320., 220., KeyboardInteractivity::OnDemand),
            PanelKind::DockMenu => (180., 96., KeyboardInteractivity::OnDemand),
            PanelKind::Nostr => (560., 520., KeyboardInteractivity::OnDemand),
            // measured height refines this: the forecast strip sets it
            PanelKind::Weather => (320., 240., KeyboardInteractivity::OnDemand),
            PanelKind::Wifi => (360., 320., KeyboardInteractivity::OnDemand),
            PanelKind::Bluetooth => (360., 320., KeyboardInteractivity::OnDemand),
        }
    }

    pub fn namespace(self) -> &'static str {
        match self {
            PanelKind::Settings => "settings",
            PanelKind::Launcher => "launcher",
            PanelKind::Calendar => "calendar",
            PanelKind::Notifications => "notifications",
            PanelKind::Volume => "volume",
            PanelKind::Brightness => "brightness",
            PanelKind::Mic => "mic",
            PanelKind::PowerProfile => "power-profile",
            PanelKind::Cpu => "cpu",
            PanelKind::Ram => "ram",
            PanelKind::Temp => "temp",
            PanelKind::Disk => "disk",
            PanelKind::Battery => "battery",
            PanelKind::DockMenu => "dock-menu",
            PanelKind::Nostr => "nostr",
            PanelKind::Weather => "weather",
            PanelKind::Wifi => "wifi",
            PanelKind::Bluetooth => "bluetooth",
        }
    }

    /// The row of the panel table the compiler can't check for us: building
    /// the view. `geometry()` and `namespace()` cover the rest of the row.
    fn open_view(
        self,
        settings: gpui::Entity<crate::settings::Settings>,
        sysmon: gpui::Entity<crate::sysmon::SysMon>,
        notifications: gpui::Entity<crate::notifications::NotificationState>,
        nostr: gpui::Entity<crate::nostr::NostrState>,
        weather: gpui::Entity<crate::weather::WeatherState>,
        window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyView {
        let (width, height, _) = self.geometry();
        let geometry = PanelGeometry {
            width,
            height,
            cove: COVE,
        };
        match self {
            PanelKind::Settings => cx
                .new(|cx| {
                    crate::settings_view::SettingsView::new(settings, sysmon, window, cx, geometry)
                })
                .into(),
            PanelKind::Launcher => cx
                .new(|cx| {
                    crate::launcher::LauncherView::new(
                        crate::launcher::load_apps(),
                        window,
                        cx,
                        geometry,
                    )
                })
                .into(),
            PanelKind::Calendar => cx
                .new(|cx| crate::calendar::CalendarView::new(window, cx, geometry))
                .into(),
            PanelKind::Notifications => cx
                .new(|cx| {
                    crate::notifications_view::NotificationsView::new(
                        notifications,
                        settings,
                        window,
                        cx,
                        geometry,
                    )
                })
                .into(),
            PanelKind::Volume => cx
                .new(|cx| {
                    crate::slider_panel::SliderPanelView::new(
                        crate::slider_panel::SliderKind::Volume,
                        sysmon,
                        window,
                        cx,
                        geometry,
                    )
                })
                .into(),
            PanelKind::Brightness => cx
                .new(|cx| {
                    crate::slider_panel::SliderPanelView::new(
                        crate::slider_panel::SliderKind::Brightness,
                        sysmon,
                        window,
                        cx,
                        geometry,
                    )
                })
                .into(),
            PanelKind::Mic => cx
                .new(|cx| {
                    crate::slider_panel::SliderPanelView::new(
                        crate::slider_panel::SliderKind::Mic,
                        sysmon,
                        window,
                        cx,
                        geometry,
                    )
                })
                .into(),
            PanelKind::PowerProfile => cx
                .new(|cx| crate::power_panel::PowerProfileView::new(sysmon, window, cx, geometry))
                .into(),
            PanelKind::Cpu => cx
                .new(|cx| {
                    crate::sysinfo_panel::SysPanelView::new(
                        crate::sysinfo_panel::SysPanel::Cpu,
                        sysmon,
                        window,
                        cx,
                        geometry,
                    )
                })
                .into(),
            PanelKind::Ram => cx
                .new(|cx| {
                    crate::sysinfo_panel::SysPanelView::new(
                        crate::sysinfo_panel::SysPanel::Ram,
                        sysmon,
                        window,
                        cx,
                        geometry,
                    )
                })
                .into(),
            PanelKind::Temp => cx
                .new(|cx| {
                    crate::sysinfo_panel::SysPanelView::new(
                        crate::sysinfo_panel::SysPanel::Temp,
                        sysmon,
                        window,
                        cx,
                        geometry,
                    )
                })
                .into(),
            PanelKind::Disk => cx
                .new(|cx| {
                    crate::sysinfo_panel::SysPanelView::new(
                        crate::sysinfo_panel::SysPanel::Disk,
                        sysmon,
                        window,
                        cx,
                        geometry,
                    )
                })
                .into(),
            PanelKind::Battery => cx
                .new(|cx| {
                    crate::sysinfo_panel::SysPanelView::new(
                        crate::sysinfo_panel::SysPanel::Battery,
                        sysmon,
                        window,
                        cx,
                        geometry,
                    )
                })
                .into(),
            PanelKind::DockMenu => {
                let context = cx.global::<PanelHost>().dock_menu();
                cx.new(|cx| crate::dock::DockMenuView::new(context, settings, window, cx, geometry))
                    .into()
            }
            PanelKind::Nostr => cx
                .new(|cx| crate::nostr_panel::NostrSignerView::new(nostr, window, cx, geometry))
                .into(),
            PanelKind::Weather => cx
                .new(|cx| {
                    crate::weather_panel::WeatherPanelView::new(weather, settings, window, cx, geometry)
                })
                .into(),
            PanelKind::Wifi => cx
                .new(|cx| crate::wifi_panel::WifiPanelView::new(sysmon, window, cx, geometry))
                .into(),
            PanelKind::Bluetooth => cx
                .new(|cx| {
                    crate::bluetooth_panel::BluetoothPanelView::new(sysmon, window, cx, geometry)
                })
                .into(),
        }
    }
}

/// The seam every panel consumer talks to. Lives as a gpui Global so views in
/// any window (scrim clicks, escape keys, the bar's gear) reach the same host.
/// Borrowing rule: the global is only held in short blocks, never across a
/// call that also needs `cx`.
pub struct PanelHost {
    settings: gpui::Entity<crate::settings::Settings>,
    sysmon: gpui::Entity<crate::sysmon::SysMon>,
    notifications: gpui::Entity<crate::notifications::NotificationState>,
    /// The signer's shared snapshot: the Nostr panel's state, and the
    /// bar widget's.
    nostr: gpui::Entity<crate::nostr::NostrState>,
    /// The weather's slow snapshot: the panel's state and the bar
    /// widget's.
    weather: gpui::Entity<crate::weather::WeatherState>,
    /// The bar view, told about panel transitions so it can suppress the
    /// tooltips the compositor's re-entry would otherwise resurrect.
    bar_view: Option<gpui::WeakEntity<crate::bar::ShellBar>>,
    open: Option<OpenPanel>,
    bar: BarGeometry,
    /// Where the next open panel hangs, set by the toggle flavors.
    placement: PanelPlacement,
    /// The context menu target a dock cell right-clicked: consumed by the
    /// `DockMenuView` construction.
    dock_menu: Option<crate::dock::DockMenuContext>,
}

struct OpenPanel {
    kind: PanelKind,
    /// The session's windows, kept for liveness probes: a session whose
    /// surfaces the compositor closed (its output went away) reads as
    /// closed, so the next toggle opens instead of eating the press.
    windows: Vec<gpui::AnyWindowHandle>,
    close: Box<dyn Fn(&mut App)>,
}

/// The one view `open_window` sees, whichever concrete view the panel table
/// built: renders the wrapped view.
struct PanelView(gpui::AnyView);

impl Render for PanelView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.0.clone()
    }
}

impl Global for PanelHost {}

impl PanelHost {
    pub fn new(
        settings: gpui::Entity<crate::settings::Settings>,
        sysmon: gpui::Entity<crate::sysmon::SysMon>,
        notifications: gpui::Entity<crate::notifications::NotificationState>,
        nostr: gpui::Entity<crate::nostr::NostrState>,
        weather: gpui::Entity<crate::weather::WeatherState>,
    ) -> Self {
        Self {
            settings,
            sysmon,
            notifications,
            nostr,
            weather,
            bar_view: None,
            open: None,
            bar: BarGeometry::default(),
            placement: PanelPlacement::Bar,
            dock_menu: None,
        }
    }

    /// The bar reports its identity at construction (it reports geometry
    /// later, at render).
    pub fn set_bar(&mut self, bar: gpui::WeakEntity<crate::bar::ShellBar>) {
        self.bar_view = Some(bar);
    }

    /// The bar reports its content rect whenever it changes; panels center
    /// against it.
    pub fn report_bar_geometry(&mut self, bar: BarGeometry) {
        self.bar = bar;
    }

    /// Live bar geometry: toasts hang under the bar, panels center on it.
    pub fn bar(&self) -> BarGeometry {
        self.bar
    }

    /// The dock right-click target: read by the dock menu's construction.
    pub fn set_dock_menu(&mut self, context: Option<crate::dock::DockMenuContext>) {
        self.dock_menu = context;
    }

    pub fn dock_menu(&self) -> Option<crate::dock::DockMenuContext> {
        self.dock_menu.clone()
    }

    fn take_close(&mut self) -> Option<Box<dyn Fn(&mut App)>> {
        self.open.take().map(|session| session.close)
    }
}

/// Whether a panel of this kind is open with its surfaces actually
/// alive: a session whose windows the compositor closed (its output
/// went away) reads as closed. Free function because the probe needs
/// `cx`; the plain kind check alone would leave a stale session
/// reporting itself open forever.
pub fn is_open(kind: &PanelKind, cx: &mut App) -> bool {
    let Some(windows) = cx
        .global::<PanelHost>()
        .open
        .as_ref()
        .filter(|session| session.kind == *kind)
        .map(|session| session.windows.clone())
    else {
        return false;
    };
    windows
        .iter()
        .any(|window| window.update(cx, |_, _, _| {}).is_ok())
}

/// Toggle a panel open/closed. Free function because the host lives in `cx`.
pub fn toggle_panel(kind: PanelKind, cx: &mut App) {
    // centered placement: clear any stale placement
    cx.global_mut::<PanelHost>().placement = PanelPlacement::Bar;
    open_panel(kind, cx);
}

/// Toggle a panel open/closed, hanging it centered under `anchor_x` (the
/// pointer x at widget-click time) instead of centered on the bar content.
pub fn toggle_panel_anchored(kind: PanelKind, anchor_x: f32, cx: &mut App) {
    cx.global_mut::<PanelHost>().placement = PanelPlacement::Widget { x: anchor_x };
    open_panel(kind, cx);
}

/// Toggle a panel open/closed beside the dock at the pointer: the dock's
/// context menu placement. `offset` is the distance from the dock's edge
/// (its strip plus a gap).
pub fn toggle_panel_at(
    kind: PanelKind,
    x: f32,
    y: f32,
    dock: crate::settings::DockPosition,
    offset: f32,
    cx: &mut App,
) {
    cx.global_mut::<PanelHost>().placement = PanelPlacement::At { x, y, dock, offset };
    open_panel(kind, cx);
}

fn open_panel(kind: PanelKind, cx: &mut App) {
    // a session whose surfaces the compositor closed (its output went
    // away) reads as closed: running the stale close would only log
    // "window not found", and the toggle would eat its own press
    let open_windows = cx
        .global::<PanelHost>()
        .open
        .as_ref()
        .map(|session| session.windows.clone())
        .unwrap_or_default();
    let dead = !open_windows.is_empty()
        && open_windows
            .iter()
            .all(|window| window.update(cx, |_, _, _| {}).is_err());
    let closed = {
        let host = cx.global_mut::<PanelHost>();
        if dead {
            host.open = None;
            (None, None)
        } else {
            let closed_kind = host.open.as_ref().map(|session| session.kind);
            log::info!("toggle_panel({kind:?}): open={:?}", closed_kind);
            (closed_kind, host.take_close())
        }
    };
    if let (Some(closed_kind), Some(close)) = closed {
        defer_close(close, cx);
        if closed_kind == kind {
            return;
        }
    }

    let (settings, sysmon, notifications, nostr, weather, bar, placement) = {
        let host = cx.global_mut::<PanelHost>();
        (
            host.settings.clone(),
            host.sysmon.clone(),
            host.notifications.clone(),
            host.nostr.clone(),
            host.weather.clone(),
            host.bar,
            host.placement,
        )
    };
    let (width, height, keyboard) = kind.geometry();
    let namespace = kind.namespace();

    let mut close_steps: Vec<Box<dyn Fn(&mut App)>> = Vec::new();
    let mut windows: Vec<gpui::AnyWindowHandle> = Vec::new();

    match cx.open_window(scrim_options(), |_, cx| cx.new(|_| ScrimView)) {
        Ok(handle) => {
            windows.push(*handle);
            close_steps.push(Box::new(move |cx| {
                if let Err(err) = handle.update(cx, |_, window, _| window.remove_window()) {
                    log::error!("closing {namespace} scrim failed: {err:#}");
                }
            }));
        }
        Err(err) => log::error!("failed to open {namespace} scrim: {err:#}"),
    }

    let panel_close: Box<dyn Fn(&mut App)> = match cx.open_window(
        panel_window_options(namespace, width, height, bar, placement, keyboard),
        |window, cx| {
            let view = kind.open_view(
                settings.clone(),
                sysmon.clone(),
                notifications.clone(),
                nostr.clone(),
                weather.clone(),
                window,
                cx,
            );
            cx.new(|_| PanelView(view))
        },
    ) {
        Ok(handle) => {
            windows.push(*handle);
            Box::new(move |cx| {
                if let Err(err) = handle.update(cx, |_, window, _| window.remove_window()) {
                    log::error!("closing {namespace} panel failed: {err:#}");
                }
            })
        }
        Err(err) => {
            log::error!("failed to open {namespace} panel: {err:#}");
            Box::new(|_| {})
        }
    };
    close_steps.push(panel_close);

    {
        let host = cx.global_mut::<PanelHost>();
        host.open = Some(OpenPanel {
            kind,
            windows,
            close: Box::new(move |cx| {
                for close_step in &close_steps {
                    close_step(cx);
                }
            }),
        });
        log::info!("toggle_panel({kind:?}): opened");
    }
    notify_bar(cx);
}

/// Run a captured close sequence after the current dispatch completes:
/// closing a window from inside its own event dispatch fails: the dispatch
/// holds the window out of the app's window slab, so the re-entrant
/// `handle.update` reports "window not found" and the surface ghosts,
/// permanently eating clicks.
pub(crate) fn defer_close(close: Box<dyn Fn(&mut App)>, cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.update(|cx| close(cx));
    })
    .detach();
}

/// Re-render open panel surfaces so their drawers re-anchor to freshly
/// reported bar geometry (a mode change, or a bar align/width change).
/// Deferred like `defer_close`: report_bar_geometry runs inside the
/// bar's own render dispatch, and refreshing another window re-entrantly
/// there would fail the same way.
pub(crate) fn refresh_open_panels(cx: &mut App) {
    let windows = cx
        .global::<PanelHost>()
        .open
        .as_ref()
        .map(|session| session.windows.clone());
    let Some(windows) = windows else {
        return;
    };
    cx.spawn(async move |cx| {
        cx.update(|cx| {
            for window in windows {
                // a panel closing mid-refresh is the expected race:
                // there is nothing left to re-anchor
                window.update(cx, |_, window, _| window.refresh()).ok();
            }
        });
    })
    .detach();
}

/// Dismiss whatever panel is open, if any.
pub fn close_panels(cx: &mut App) {
    let close = cx.global_mut::<PanelHost>().take_close();
    log::info!(
        "close_panels: {}",
        if close.is_some() {
            "had a panel"
        } else {
            "nothing open"
        }
    );
    notify_bar(cx);
    if let Some(close) = close {
        defer_close(close, cx);
    }
}

/// Tell the bar a panel transition happened: it suppresses tooltips until
/// the pointer genuinely moves, because the compositor re-enters the bar
/// with a synthesized MouseMove (no real motion) once the scrim dies, and
/// that synthesized move resurrects whatever tooltip was last hovered.
///
/// Deferred via spawn: toggle_panel is often called from a bar click
/// listener, which runs inside the bar's own entity update; updating the
/// bar re-entrantly there would panic. The spawned task runs after the
/// current dispatch completes.
fn notify_bar(cx: &mut App) {
    let bar = cx.global::<PanelHost>().bar_view.clone();
    if let Some(bar) = bar {
        cx.spawn(async move |cx| {
            if let Err(err) = bar.update(cx, crate::bar::ShellBar::suppress_tooltips) {
                log::error!("notifying bar of a panel transition failed: {err:#}");
            }
        })
        .detach();
    }
}

/// The drawer's offset along the surface's stretched axis.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum DrawerAxis {
    /// Offset from the surface's left edge (horizontal stretch).
    Horizontal(f32),
    /// Offset from the surface's top edge (vertical stretch: a left or
    /// right dock's menu).
    Vertical(f32),
}

/// Where the drawer hangs along the stretched axis: centered on the bar
/// content's center line (Bar), on the anchor x (Widget), or on the
/// pointer along the dock's long axis (At). Computed at render from
/// live geometry: the surface merely spans the output, so mode, align,
/// and bar width changes reposition an open panel instead of stranding
/// it where creation-time margins put it.
fn drawer_axis(
    placement: PanelPlacement,
    bar: BarGeometry,
    viewport: Size<Pixels>,
    width: f32,
    height: f32,
) -> DrawerAxis {
    let on_screen_right = (f32::from(viewport.width) - width).max(0.);
    match placement {
        PanelPlacement::Bar => DrawerAxis::Horizontal(
            (bar.content_x + (bar.content_width - width) / 2.)
                .max(0.)
                .min(on_screen_right),
        ),
        PanelPlacement::Widget { x } => {
            let content_right = bar.content_x + bar.content_width;
            DrawerAxis::Horizontal(
                (x - width / 2.)
                    .max(0.)
                    .min((content_right - width).max(0.)),
            )
        }
        PanelPlacement::At { x, y, dock, .. } => match dock {
            // horizontal dock: menu above/below it, centered on the cell
            crate::settings::DockPosition::Bottom | crate::settings::DockPosition::Top => {
                DrawerAxis::Horizontal((x - width / 2.).max(0.).min(on_screen_right))
            }
            // vertical dock: menu beside it, centered on the cell
            crate::settings::DockPosition::Left | crate::settings::DockPosition::Right => {
                let on_screen_bottom = (f32::from(viewport.height) - height).max(0.);
                DrawerAxis::Vertical((y - height / 2.).max(0.).min(on_screen_bottom))
            }
        },
    }
}

/// The host-owned wrapper every Panel wears (ADR-0004): paints the drawer
/// silhouette, maps the input region to the body rect (cove corners stay
/// click-through), insets the content by the cove, and dismisses on Esc.
/// Panel views render content only.
///
/// The drawer's offset along the stretched axis comes from `drawer_axis`
/// at render time, so the input region tracks it too.
pub fn chrome(
    geometry: PanelGeometry,
    window: &mut Window,
    cx: &mut App,
    content: impl IntoElement,
) -> Stateful<Div> {
    let PanelGeometry {
        width,
        height,
        cove,
    } = geometry;
    let (axis, bar_at_top) = {
        let host = cx.global::<PanelHost>();
        let bar = host.bar();
        (
            drawer_axis(host.placement, bar, window.viewport_size(), width, height),
            bar.position == crate::settings::BarPosition::Top,
        )
    };
    let (drawer_left, drawer_top) = match axis {
        DrawerAxis::Horizontal(left) => (left, 0.),
        DrawerAxis::Vertical(top) => (0., top),
    };
    window.set_input_region(Some(&[Bounds {
        origin: point(px(drawer_left + cove), px(drawer_top)),
        size: size(px(width - 2. * cove), px(height)),
    }]));
    div()
        .id("panel-chrome")
        .size_full()
        .relative()
        .on_key_down(|event: &KeyDownEvent, _, cx| {
            if event.keystroke.key == "escape" {
                close_panels(cx);
            }
        })
        .child(
            svg()
                .data(&drawer_silhouette(width, height, cove, CORNER_RADIUS, bar_at_top))
                .absolute()
                .top(px(drawer_top))
                .left(px(drawer_left))
                .w(px(width))
                .h(px(height))
                .text_color(rgba(crate::theme::current().panel_bg)),
        )
        .child(
            div()
                .absolute()
                .top(px(drawer_top))
                .left(px(drawer_left))
                .w(px(width))
                .h(px(height))
                .px(px(cove))
                .child(content),
        )
}

pub fn panel_window_options(
    namespace: &str,
    width: f32,
    height: f32,
    bar: BarGeometry,
    placement: PanelPlacement,
    keyboard: KeyboardInteractivity,
) -> WindowOptions {
    // The surface stretches along the placement axis, the bar's own
    // trick: the compositor keeps it spanning the output through mode
    // changes, and the drawer is positioned in-view at render time (see
    // drawer_axis), so resolution, align, and bar width changes
    // re-anchor an open panel instead of stranding it at stale margins.
    // Only the cross-axis offset (the bar's inner edge, or the gap to
    // the dock) is a fixed margin; a bottom bar hangs its panels above
    // it, opening upward.
    let (anchor, margin) = match placement {
        PanelPlacement::Bar | PanelPlacement::Widget { .. } => match bar.position {
            crate::settings::BarPosition::Top => (
                Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
                (px(bar.bar_edge), px(0.), px(0.), px(0.)),
            ),
            crate::settings::BarPosition::Bottom => (
                Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                (px(0.), px(0.), px(bar.bar_edge), px(0.)),
            ),
        },
        PanelPlacement::At { dock, offset, .. } => match dock {
            crate::settings::DockPosition::Bottom => (
                Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                (px(0.), px(0.), px(offset), px(0.)),
            ),
            crate::settings::DockPosition::Top => (
                Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
                (px(offset), px(0.), px(0.), px(0.)),
            ),
            crate::settings::DockPosition::Left => (
                Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
                (px(0.), px(0.), px(0.), px(offset)),
            ),
            crate::settings::DockPosition::Right => (
                Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
                (px(0.), px(offset), px(0.), px(0.)),
            ),
        },
    };
    WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(width), px(height)),
        })),
        app_id: Some(format!("kuma-shell-{namespace}")),
        window_background: WindowBackgroundAppearance::Transparent,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: namespace.to_string(),
            layer: Layer::Overlay,
            exclusive_zone: Some(px(-1.)),
            anchor,
            keyboard_interactivity: keyboard,
            margin: Some(margin),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn scrim_options() -> WindowOptions {
    WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(0.), px(0.)),
        })),
        app_id: Some("kuma-shell-scrim".into()),
        window_background: WindowBackgroundAppearance::Transparent,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: "kuma-shell-scrim".into(),
            layer: Layer::Overlay,
            anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            exclusive_zone: Some(px(-1.)),
            keyboard_interactivity: KeyboardInteractivity::None,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Fullscreen transparent surface below an open panel: any click on it
/// dismisses the panel.
pub struct ScrimView;

impl Render for ScrimView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("scrim")
            .size_full()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| close_panels(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer_of(options: &WindowOptions) -> &LayerShellOptions {
        match &options.kind {
            WindowKind::LayerShell(layer) => layer,
            _ => panic!("expected a layer-shell window"),
        }
    }

    fn margins(layer: &LayerShellOptions) -> (f32, f32, f32, f32) {
        let (top, right, bottom, left) = layer.margin.unwrap();
        (
            f32::from(top),
            f32::from(right),
            f32::from(bottom),
            f32::from(left),
        )
    }

    fn axis(
        placement: PanelPlacement,
        bar: BarGeometry,
        viewport: (f32, f32),
        width: f32,
        height: f32,
    ) -> DrawerAxis {
        drawer_axis(
            placement,
            bar,
            size(px(viewport.0), px(viewport.1)),
            width,
            height,
        )
    }

    #[test]
    fn panel_surface_stretches_across_the_output_under_the_bar() {
        let options = panel_window_options(
            "test",
            400.,
            300.,
            BarGeometry {
                content_x: 200.,
                content_width: 600.,
                bar_edge: 40.,
                ..Default::default()
            },
            PanelPlacement::Bar,
            KeyboardInteractivity::OnDemand,
        );
        let layer = layer_of(&options);
        assert_eq!(
            layer.anchor,
            Anchor::TOP | Anchor::LEFT | Anchor::RIGHT
        );
        // the only margin is the bar's bottom edge; the horizontal
        // position is decided at render, from live geometry
        assert_eq!(margins(layer), (40., 0., 0., 0.));
    }

    #[test]
    fn bottom_bar_panels_hang_above_the_bar_and_open_upward() {
        let options = panel_window_options(
            "test",
            400.,
            300.,
            BarGeometry {
                content_x: 200.,
                content_width: 600.,
                bar_edge: 40.,
                position: crate::settings::BarPosition::Bottom,
            },
            PanelPlacement::Bar,
            KeyboardInteractivity::OnDemand,
        );
        let layer = layer_of(&options);
        // the surface pins to the bottom edge, margin holds the bar's
        // inner face away, and the drawer positions at render like any
        // top-bar panel
        assert_eq!(
            layer.anchor,
            Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT
        );
        assert_eq!(margins(layer), (0., 0., 40., 0.));
    }

    #[test]
    fn the_silhouette_faces_its_coves_toward_the_bar() {
        let top = String::from_utf8(
            drawer_silhouette(200., 100., 16., 12., true)
                .to_vec(),
        )
        .unwrap();
        let bottom = String::from_utf8(
            drawer_silhouette(200., 100., 16., 12., false)
                .to_vec(),
        )
        .unwrap();
        // top bar: concave coves at the top (toward the bar), convex
        // rounded corners at the bottom
        assert!(top.contains("M 0 0 A 16 16 0 0 1 16 16"), "{top}");
        assert!(top.contains("A 12 12 0 0 0 28 100"), "{top}");
        // bottom bar: mirrored, coves at the bottom, convex corners at
        // the top
        assert!(bottom.contains("M 0 100 A 16 16 0 0 0 16 84"), "{bottom}");
        assert!(bottom.contains("A 12 12 0 0 1 28 0"), "{bottom}");
    }

    #[test]
    fn panel_centers_on_the_bar_content_center_line() {
        // viewport 1000, content 600 centered → content_x 200; a panel
        // centered on the content's center line is screen-centered.
        let bar = BarGeometry {
            content_x: 200.,
            content_width: 600.,
            bar_edge: 40.,
            ..Default::default()
        };
        assert_eq!(
            axis(PanelPlacement::Bar, bar, (1000., 800.), 400., 300.),
            DrawerAxis::Horizontal(300.)
        );
    }

    #[test]
    fn panel_centers_on_content_even_when_content_is_narrower() {
        // content spans 800–1000 (center 900); panel centers on 900. The
        // viewport is wide enough that no edge clamp applies: a 400-wide
        // panel centered on 900 spans 700–1100.
        let bar = BarGeometry {
            content_x: 800.,
            content_width: 200.,
            bar_edge: 40.,
            ..Default::default()
        };
        assert_eq!(
            axis(PanelPlacement::Bar, bar, (1200., 800.), 400., 300.),
            DrawerAxis::Horizontal(700.)
        );
    }

    #[test]
    fn panel_left_edge_never_goes_negative() {
        let bar = BarGeometry {
            content_x: 0.,
            content_width: 200.,
            bar_edge: 0.,
            ..Default::default()
        };
        assert_eq!(
            axis(PanelPlacement::Bar, bar, (1000., 800.), 560., 300.),
            DrawerAxis::Horizontal(0.)
        );
    }

    #[test]
    fn bar_panel_clamps_to_the_screen_right_edge() {
        // content hugs the right edge (800–1000); a 560-wide drawer
        // centered on it would run 90px past the screen: clamp to the
        // viewport's right edge.
        let bar = BarGeometry {
            content_x: 800.,
            content_width: 200.,
            bar_edge: 40.,
            ..Default::default()
        };
        assert_eq!(
            axis(PanelPlacement::Bar, bar, (1000., 800.), 560., 300.),
            DrawerAxis::Horizontal(440.)
        );
    }

    #[test]
    fn unreported_bar_geometry_defaults_to_no_offset() {
        assert_eq!(
            axis(
                PanelPlacement::Bar,
                BarGeometry::default(),
                (0., 0.),
                560.,
                300.
            ),
            DrawerAxis::Horizontal(0.)
        );
    }

    #[test]
    fn anchored_panel_hangs_under_the_anchor() {
        let bar = BarGeometry {
            content_x: 200.,
            content_width: 600.,
            bar_edge: 40.,
            ..Default::default()
        };
        // widget at x=500; panel 360 wide centers under it
        assert_eq!(
            axis(
                PanelPlacement::Widget { x: 500. },
                bar,
                (1000., 800.),
                360.,
                150.
            ),
            DrawerAxis::Horizontal(320.)
        );
    }

    #[test]
    fn anchored_panel_clamps_to_screen_left_and_content_right() {
        let bar = BarGeometry {
            content_x: 0.,
            content_width: 600.,
            bar_edge: 40.,
            ..Default::default()
        };
        // anchor near the left edge: clamps to 0
        assert_eq!(
            axis(
                PanelPlacement::Widget { x: 50. },
                bar,
                (1000., 800.),
                360.,
                150.
            ),
            DrawerAxis::Horizontal(0.)
        );

        // anchor near the content's right edge: right edge clamps to 600
        assert_eq!(
            axis(
                PanelPlacement::Widget { x: 590. },
                bar,
                (1000., 800.),
                360.,
                150.
            ),
            DrawerAxis::Horizontal(240.)
        );
    }

    #[test]
    fn dock_menu_surface_stretches_along_the_dock_edge() {
        let bar = BarGeometry::default();
        let cases = [
            (
                crate::settings::DockPosition::Bottom,
                Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                (0., 0., 72., 0.),
            ),
            (
                crate::settings::DockPosition::Top,
                Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
                (72., 0., 0., 0.),
            ),
            (
                crate::settings::DockPosition::Left,
                Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
                (0., 0., 0., 72.),
            ),
            (
                crate::settings::DockPosition::Right,
                Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
                (0., 72., 0., 0.),
            ),
        ];
        for (dock, anchor, expected) in cases {
            let options = panel_window_options(
                "test",
                180.,
                96.,
                bar,
                PanelPlacement::At {
                    x: 500.,
                    y: 20.,
                    dock,
                    offset: 72.,
                },
                KeyboardInteractivity::OnDemand,
            );
            let layer = layer_of(&options);
            assert_eq!(layer.anchor, anchor, "{dock:?} anchor");
            assert_eq!(margins(layer), expected, "{dock:?} margins");
        }
    }

    #[test]
    fn dock_menu_hangs_at_the_pointer_along_the_dock_axis() {
        // bottom dock: menu above it, centered on the clicked cell's x
        let placement = PanelPlacement::At {
            x: 500.,
            y: 20.,
            dock: crate::settings::DockPosition::Bottom,
            offset: 72.,
        };
        assert_eq!(
            axis(placement, BarGeometry::default(), (1000., 800.), 180., 96.),
            DrawerAxis::Horizontal(410.)
        );

        // left dock: menu to the right of it, centered on the cell's y
        let placement = PanelPlacement::At {
            x: 20.,
            y: 600.,
            dock: crate::settings::DockPosition::Left,
            offset: 72.,
        };
        assert_eq!(
            axis(placement, BarGeometry::default(), (1000., 1080.), 180., 96.),
            DrawerAxis::Vertical(552.)
        );
    }

    #[test]
    fn dock_menu_clamps_to_the_screen_edge() {
        // bottom dock cell near the screen's right edge: the menu's
        // right edge clamps to the viewport
        let placement = PanelPlacement::At {
            x: 990.,
            y: 20.,
            dock: crate::settings::DockPosition::Bottom,
            offset: 72.,
        };
        assert_eq!(
            axis(placement, BarGeometry::default(), (1000., 800.), 180., 96.),
            DrawerAxis::Horizontal(820.)
        );

        // left dock cell near the screen's bottom: the menu's bottom
        // edge clamps to the viewport
        let placement = PanelPlacement::At {
            x: 20.,
            y: 1050.,
            dock: crate::settings::DockPosition::Left,
            offset: 72.,
        };
        assert_eq!(
            axis(placement, BarGeometry::default(), (1000., 1080.), 180., 96.),
            DrawerAxis::Vertical(984.)
        );
    }

    #[test]
    fn every_panel_kind_has_a_complete_row() {
        for kind in [
            PanelKind::Settings,
            PanelKind::Launcher,
            PanelKind::Calendar,
            PanelKind::Notifications,
            PanelKind::Volume,
            PanelKind::Brightness,
            PanelKind::DockMenu,
            PanelKind::Nostr,
            PanelKind::Weather,
            PanelKind::Wifi,
        ] {
            let (width, height, keyboard) = kind.geometry();
            assert!(width > 0., "{kind:?} geometry");
            assert!(height > 0., "{kind:?} geometry");
            assert!(matches!(
                keyboard,
                KeyboardInteractivity::Exclusive | KeyboardInteractivity::OnDemand
            ));
            assert!(!kind.namespace().is_empty());
        }
        // namespaces are distinct: they key the layer surfaces
        assert_ne!(
            PanelKind::Settings.namespace(),
            PanelKind::Launcher.namespace()
        );
    }
}
