//! The shell's persistent surfaces and the watch that keeps them alive.
//!
//! The surfaces the shell cannot work without (the wallpaper, the bar,
//! the lock screens, the dock) are owned here: a watch probes their
//! handles and recreates the missing ones while displays exist. This is
//! the contract noctalia idled by: a compositor closes every layer
//! surface the moment its last output goes away (a dock unplug on an
//! external-monitor laptop, the CI smoke's output-less qemu), and the
//! shell answers by idling with zero windows, still holding its DBus
//! names (org.freedesktop.Notifications) and deliberately taking no
//! part in the suspend path, then bringing the surfaces back when
//! outputs return. The process exits only when the
//! compositor's connection itself breaks, which is session end.
//!
//! Output visibility needs no window: gpui binds `wl_output` globals as
//! they appear, and `cx.displays()` reads them live. Surface liveness is
//! a probe: `AnyWindowHandle::update` answers a closed window with a
//! quiet `Err` (the same `window not found` the journal shows), so
//! detection costs nothing and logs nothing when all is well.

use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Entity, Global, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    point, px, size,
};

use crate::bar::ShellBar;
use crate::lock;
use crate::session::SessionState;
use crate::nostr::NostrState;
use crate::notifications::NotificationState;
use crate::settings::{BarConfig, BarPosition, Settings};
use crate::sysmon::SysMon;
use crate::tray::TrayState;

/// How often the watch surveys the surfaces: hotplug lands within about
/// one tick, and the survey is a display count plus a few probes,
/// nothing at all when everything is alive.
const WATCH_TICK: Duration = Duration::from_secs(2);

/// The entities the persistent surfaces render from, held so the watch
/// can rebuild any of them long after startup ran.
pub struct SurfaceDeps {
    pub niri: Entity<SessionState>,
    pub sysmon: Entity<SysMon>,
    pub settings: Entity<Settings>,
    pub notifications: Entity<NotificationState>,
    pub tray: Entity<TrayState>,
    pub nostr: Entity<NostrState>,
    pub weather: Entity<crate::weather::WeatherState>,
    pub lock: Entity<lock::LockState>,
}

struct SurfaceHost {
    deps: SurfaceDeps,
    wallpaper: Option<AnyWindowHandle>,
    bar: Option<AnyWindowHandle>,
    /// The position the live bar surface was created for: layer-shell
    /// anchors are fixed at creation, so a position change is not a
    /// live change; the observer closes the bar and ensure reopens it.
    bar_position: Option<BarPosition>,
    /// The last surface-creation failure, kept as its message: a
    /// permanent failure (a compositor without layer-shell support,
    /// say) must log once per distinct error, not once per watch tick.
    last_error: Option<String>,
}

impl Global for SurfaceHost {}

/// Take ownership of the persistent surfaces. Nothing opens here: the
/// shell's first `ensure` does, so startup and every recovery run the
/// same code.
pub fn init(deps: SurfaceDeps, cx: &mut App) {
    let settings = deps.settings.clone();
    cx.set_global(SurfaceHost {
        deps,
        wallpaper: None,
        bar: None,
        bar_position: None,
        last_error: None,
    });
    // A bar position change cannot apply by re-render: the surface's
    // anchors are fixed at creation. Drop the bar so the ensure pass
    // reopens it at the new edge; every other bar setting applies live.
    cx.observe(&settings, |settings, cx| {
        let position = settings.read(cx).bar.position;
        let bar = {
            let host = cx.global_mut::<SurfaceHost>();
            if host.bar_position == Some(position) {
                return;
            }
            host.bar_position = None;
            host.bar.take()
        };
        if let Some(bar) = bar {
            if let Err(err) = bar.update(cx, |_, window, _| window.remove_window()) {
                log::error!("closing the bar for its position change failed: {err:#}");
            }
        }
        ensure(cx);
    })
    .detach();
}

/// Recreate whatever persistent surface is missing. A no-op while there
/// is nothing to attach to; probes only when everything is alive.
pub fn ensure(cx: &mut App) {
    if cx.displays().is_empty() {
        return;
    }
    let (wallpaper, bar) = {
        let host = cx.global_mut::<SurfaceHost>();
        (host.wallpaper, host.bar)
    };
    let wallpaper_gone = wallpaper.is_none_or(|window| window.update(cx, |_, _, _| {}).is_err());
    let bar_gone = bar.is_none_or(|window| window.update(cx, |_, _, _| {}).is_err());

    if wallpaper_gone {
        let settings = cx.global::<SurfaceHost>().deps.settings.clone();
        match cx.open_window(wallpaper_options(), |_, cx| {
            cx.new(|cx| crate::wallpaper::WallpaperView::new(settings, cx))
        }) {
            Ok(handle) => {
                let host = cx.global_mut::<SurfaceHost>();
                host.wallpaper = Some(*handle);
                host.last_error = None;
                log::info!("surfaces: wallpaper opened");
            }
            Err(err) => surface_error(&err.to_string(), cx),
        }
    }
    if bar_gone {
        let (niri, sysmon, settings, notifications, tray, nostr, weather) = {
            let deps = &cx.global::<SurfaceHost>().deps;
            (
                deps.niri.clone(),
                deps.sysmon.clone(),
                deps.settings.clone(),
                deps.notifications.clone(),
                deps.tray.clone(),
                deps.nostr.clone(),
                deps.weather.clone(),
            )
        };
        let config: BarConfig = settings.read(cx).bar.clone();
        match cx.open_window(bar_window_options(&config), |_, cx| {
            cx.new(|cx| {
                ShellBar::new(
                    niri, sysmon, settings, notifications, tray, nostr, weather, cx,
                )
            })
        }) {
            Ok(handle) => {
                let host = cx.global_mut::<SurfaceHost>();
                host.bar = Some(*handle);
                host.bar_position = Some(config.position);
                host.last_error = None;
                log::info!("surfaces: bar opened");
            }
            Err(err) => surface_error(&err.to_string(), cx),
        }
    }

    // the lock screen and the dock own their surfaces; the watch just
    // gives them the tick
    let lock = cx.global::<SurfaceHost>().deps.lock.clone();
    lock::ensure_surfaces(&lock, cx);
    let (niri, settings) = {
        let deps = &cx.global::<SurfaceHost>().deps;
        (deps.niri.clone(), deps.settings.clone())
    };
    crate::dock::ensure(&niri, &settings, cx);
}

/// Run the watch: survey and recreate every tick. The loop runs until
/// the process does; the compositor's death is what ends the process.
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

fn wallpaper_options() -> WindowOptions {
    WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(0.), px(0.)),
        })),
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

fn bar_window_options(config: &BarConfig) -> WindowOptions {
    // the anchors pick the edge the bar hangs from; the offset, the
    // tooltip room, and the strip's placement inside the surface are
    // the view's business, so every other geometry setting changes live
    let anchor = match config.position {
        BarPosition::Top => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
        BarPosition::Bottom => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
    };
    WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(0.), px(config.height + config.offset_top)),
        })),
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
