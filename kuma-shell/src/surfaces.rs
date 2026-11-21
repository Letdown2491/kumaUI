//! The shell's persistent surfaces and the watch that keeps them alive.
//!
//! The surfaces the shell cannot work without (the wallpaper, the bar,
//! the lock screens, the dock) are owned here: the adapters' output
//! change event (ADR-0018) reconciles them on hotplug, and a watch
//! heartbeats the same ensure pass so a missed event heals within a
//! tick. This is the contract noctalia idled by: a compositor closes
//! every layer surface the moment its last output goes away (a dock
//! unplug on an external-monitor laptop, the CI smoke's output-less
//! qemu), and the shell answers by idling with zero windows, still
//! holding its DBus names (org.freedesktop.Notifications) and
//! deliberately taking no part in the suspend path, then bringing the
//! surfaces back when outputs return. The process exits only when the
//! compositor's connection itself breaks, which is session end.
//!
//! Output visibility needs no window: gpui binds `wl_output` globals as
//! they appear, and `cx.displays()` reads them live. Surface liveness is
//! a probe: `AnyWindowHandle::update` answers a closed window with a
//! quiet `Err` (the same `window not found` the journal shows), so
//! detection costs nothing and logs nothing when all is well.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, DisplayId, Entity, Global, PlatformDisplay,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    point, px, size,
};

use crate::bar::ShellBar;
use crate::lock;
use crate::session::{OutputsChanged, SessionState};
use crate::nostr::NostrState;
use crate::notifications::NotificationState;
use crate::settings::{BarConfig, BarPosition, Settings};
use crate::sysmon::SysMon;
use crate::tray::TrayState;

/// How often the watch heartbeats the surfaces: event-driven hotplug
/// (ADR-0018) does the real reacting, and the tick exists to catch
/// what the event stream missed (a downed stream, a lost event). The
/// survey is a display count plus a few probes, nothing at all when
/// everything is alive.
const WATCH_TICK: Duration = Duration::from_secs(2);

/// The entities the persistent surfaces render from, held so the watch
/// can rebuild any of them long after startup ran.
#[derive(Clone)]
pub struct SurfaceDeps {
    pub niri: Entity<SessionState>,
    pub sysmon: Entity<SysMon>,
    pub settings: Entity<Settings>,
    pub notifications: Entity<NotificationState>,
    pub tray: Entity<TrayState>,
    pub nostr: Entity<NostrState>,
    pub weather: Entity<crate::weather::WeatherState>,
    pub widgets: Entity<crate::widgets::CustomWidgets>,
    pub updates: Entity<crate::updates::UpdatesState>,
    pub lock: Entity<lock::LockState>,
}

struct SurfaceHost {
    deps: SurfaceDeps,
    /// One surface per output, keyed by the compositor's own output
    /// name (`display_key`): the maps are the reconcile's memory of
    /// what it opened, diffed against `cx.displays()` every pass.
    wallpaper: HashMap<String, AnyWindowHandle>,
    bar: HashMap<String, AnyWindowHandle>,
    /// The position the live bar surfaces were created for: layer-shell
    /// anchors are fixed at creation, so a position change is not a
    /// live change; the observer closes every bar and ensure reopens
    /// them.
    bar_position: Option<BarPosition>,
    /// The last surface-creation failure, kept as its message: a
    /// permanent failure (a compositor without layer-shell support,
    /// say) must log once per distinct error, not once per watch tick.
    last_error: Option<String>,
}

impl Global for SurfaceHost {}

impl SurfaceHost {
    /// The deps, cloned out of the global: every ensure path starts
    /// by handing the entities to a new surface, and the tuple-of-
    /// clones prologue used to be re-spelled at each one.
    fn deps(cx: &App) -> SurfaceDeps {
        cx.global::<SurfaceHost>().deps.clone()
    }
}

/// The map key for a display: the compositor's own output name when
/// gpui carries one (the spelling the session mirror reports for niri
/// and sway), else a stable fallback spelled from the display id.
pub(crate) fn display_key(display: &Rc<dyn PlatformDisplay>) -> String {
    display
        .name()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("display-{}", u64::from(display.id())))
}

/// Take ownership of the persistent surfaces. Nothing opens here: the
/// shell's first `ensure` does, so startup and every recovery run the
/// same code.
pub fn init(deps: SurfaceDeps, cx: &mut App) {
    let settings = deps.settings.clone();
    let niri = deps.niri.clone();
    cx.set_global(SurfaceHost {
        deps,
        wallpaper: HashMap::new(),
        bar: HashMap::new(),
        bar_position: None,
        last_error: None,
    });
    // A bar position change cannot apply by re-render: the surfaces'
    // anchors are fixed at creation. Drop every bar so the ensure pass
    // reopens them at the new edge; every other bar setting applies
    // live.
    cx.observe(&settings, |settings, cx| {
        let position = settings.read(cx).bar.position;
        let bars = {
            let host = cx.global_mut::<SurfaceHost>();
            if host.bar_position == Some(position) {
                return;
            }
            host.bar_position = None;
            std::mem::take(&mut host.bar).into_values().collect::<Vec<_>>()
        };
        for bar in bars {
            if let Err(err) = bar.update(cx, |_, window, _| window.remove_window()) {
                log::error!("closing a bar for its position change failed: {err:#}");
            }
        }
        ensure(cx);
    })
    .detach();
    // The hotplug trigger (ADR-0018): the adapters report an output
    // change as an event, and the reconcile is the same ensure pass
    // the watch heartbeats. The event is a trigger, never a command:
    // ensure diffs before it acts, and a spurious hint costs a no-op.
    cx.subscribe(&niri, |_, _: &OutputsChanged, cx| {
        ensure(cx);
    })
    .detach();
}

/// Recreate whatever persistent surface is missing, per output. A
/// no-op while there is nothing to attach to; probes only when
/// everything is alive. This is the reconcile the hotplug event and
/// the heartbeat share (ADR-0018): a diff, never a rebuild, so a
/// spurious call costs nothing.
pub fn ensure(cx: &mut App) {
    if cx.displays().is_empty() {
        return;
    }
    // The truth (cx.displays()) spelled as map keys, with the display
    // id each name targets (the layer surface's binding level).
    let displays: Vec<(String, DisplayId)> = cx
        .displays()
        .iter()
        .map(|display| (display_key(display), display.id()))
        .collect();

    // Prune: a surface whose window died (its output went away and the
    // compositor closed it) or whose output is no longer in the truth
    // drops out of the map and its handle, if still probed alive, is
    // closed for good measure. The maps are cloned out first so the
    // liveness probes run without the global borrow held.
    let (walls, bars): (HashMap<String, AnyWindowHandle>, HashMap<String, AnyWindowHandle>) = {
        let host = cx.global_mut::<SurfaceHost>();
        (host.wallpaper.clone(), host.bar.clone())
    };
    let mut dead = Vec::new();
    let mut live_walls = HashMap::new();
    for (name, handle) in walls {
        let gone = !displays.iter().any(|(key, _)| *key == name);
        if gone || !handle.update(cx, |_, _, _| {}).is_ok() {
            dead.push(handle);
        } else {
            live_walls.insert(name, handle);
        }
    }
    let mut live_bars = HashMap::new();
    for (name, handle) in bars {
        let gone = !displays.iter().any(|(key, _)| *key == name);
        if gone || !handle.update(cx, |_, _, _| {}).is_ok() {
            dead.push(handle);
        } else {
            live_bars.insert(name, handle);
        }
    }
    for handle in dead {
        if let Err(err) = handle.update(cx, |_, window, _| window.remove_window()) {
            log::debug!("pruning an already-closed surface: {err:#}");
        }
    }
    let (missing_walls, missing_bars) = {
        let host = cx.global_mut::<SurfaceHost>();
        host.wallpaper = live_walls;
        host.bar = live_bars;
        let missing_walls: Vec<(String, DisplayId)> = displays
            .iter()
            .filter(|(name, _)| !host.wallpaper.contains_key(name))
            .cloned()
            .collect();
        let missing_bars: Vec<(String, DisplayId)> = displays
            .iter()
            .filter(|(name, _)| !host.bar.contains_key(name))
            .cloned()
            .collect();
        (missing_walls, missing_bars)
    };

    for (name, display_id) in missing_walls {
        let settings = SurfaceHost::deps(cx).settings;
        match cx.open_window(wallpaper_options(display_id), |_, cx| {
            cx.new(|cx| crate::wallpaper::WallpaperView::new(settings, cx))
        }) {
            Ok(handle) => {
                let host = cx.global_mut::<SurfaceHost>();
                host.wallpaper.insert(name.clone(), *handle);
                host.last_error = None;
                log::info!("surfaces: wallpaper opened on {name}");
            }
            Err(err) => surface_error(&err.to_string(), cx),
        }
    }
    for (name, display_id) in missing_bars {
        let SurfaceDeps {
            niri,
            sysmon,
            settings,
            notifications,
            tray,
            nostr,
            weather,
            widgets,
            updates,
            lock: _,
        } = SurfaceHost::deps(cx);
        let config: BarConfig = settings.read(cx).bar.clone();
        match cx.open_window(bar_window_options(&config, display_id), |_, cx| {
            cx.new(|cx| {
                ShellBar::new(
                    name.clone(),
                    niri, sysmon, settings, notifications, tray, nostr, weather, widgets, updates,
                    cx,
                )
            })
        }) {
            Ok(handle) => {
                let host = cx.global_mut::<SurfaceHost>();
                host.bar.insert(name.clone(), *handle);
                host.bar_position = Some(config.position);
                host.last_error = None;
                log::info!("surfaces: bar opened on {name}");
            }
            Err(err) => surface_error(&err.to_string(), cx),
        }
    }

    // the lock screen and the dock own their surfaces; the watch just
    // gives them the tick
    let lock = SurfaceHost::deps(cx).lock;
    lock::ensure_surfaces(&lock, cx);
    let (niri, settings) = {
        let deps = SurfaceHost::deps(cx);
        (deps.niri, deps.settings)
    };
    crate::dock::ensure(&niri, &settings, cx);
}

/// Run the watch: heartbeat the survey every tick, recreating whatever
/// event-driven hotplug (ADR-0018) did not already reconcile. The loop
/// runs until the process does; the compositor's death is what ends
/// the process.
pub fn watch(cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut last_displays: Option<usize> = None;
        loop {
            cx.background_executor().timer(WATCH_TICK).await;
            cx.update(|cx| {
                let displays = cx.displays().len();
                if last_displays != Some(displays) {
                    log::info!("surfaces: displays now {displays}");
                    last_displays = Some(displays);
                }
                ensure(cx);
            });
        }
    })
    .detach();
}

/// Log a creation failure once per distinct message: the watch retries
/// every tick, and a permanent failure must not flood the journal.
fn surface_error(message: &str, cx: &mut App) {
    let host = cx.global_mut::<SurfaceHost>();
    let changed = host.last_error.as_deref() != Some(message);
    host.last_error = Some(message.to_string());
    if changed {
        log::error!("surface creation failed: {message}");
    }
}

fn wallpaper_options(display_id: DisplayId) -> WindowOptions {
    WindowOptions {
        titlebar: None,
        // client decorations: layer surfaces have no server side
        window_decorations: Some(gpui::WindowDecorations::Client),
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(0.), px(0.)),
        })),
        display_id: Some(display_id),
        app_id: Some("kuma-shell-wallpaper".into()),
        window_background: WindowBackgroundAppearance::Opaque,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: "kuma-shell-wallpaper".into(),
            layer: Layer::Background,
            anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            exclusive_zone: Some(px(-1.)),
            keyboard_interactivity: KeyboardInteractivity::None,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn bar_window_options(config: &BarConfig, display_id: DisplayId) -> WindowOptions {
    // the anchors pick the edge the bar hangs from; the offset, the
    // tooltip room, and the strip's placement inside the surface are
    // the view's business, so every other geometry setting changes live
    let anchor = match config.position {
        BarPosition::Top => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
        BarPosition::Bottom => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
    };
    WindowOptions {
        titlebar: None,
        // client decorations: layer surfaces have no server side
        window_decorations: Some(gpui::WindowDecorations::Client),
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(0.), px(config.height + config.offset_top)),
        })),
        display_id: Some(display_id),
        app_id: Some("kuma-shell".into()),
        window_background: WindowBackgroundAppearance::Transparent,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: "kuma-shell".into(),
            layer: Layer::Top,
            // always stretched full-width; width/align/offset are applied by the
            // view's content div so every geometry setting can change live
            anchor,
            exclusive_zone: Some(px(config.height + config.offset_top)),
            keyboard_interactivity: KeyboardInteractivity::None,
            ..Default::default()
        }),
        ..Default::default()
    }
}
