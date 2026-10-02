use kuma_shell::{bar::ShellBar, icons::KumaAssets, niri::NiriState, sysmon::SysMon};

use gpui::{
    App, AppContext, Bounds, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    point, px, size,
};

use kuma_shell::settings::{BarConfig, Settings};

use gpui_platform::application;

fn main() {
    // millis in the log: the difference between a key's bounce (two
    // requests tens of ms apart) and a human's second press
    use std::io::Write as _;
    env_logger::Builder::from_env(env_logger::Env::default())
        .format(|buf, record| {
            writeln!(
                buf,
                "[{} {:<5} {}] {}",
                buf.timestamp_millis(),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(run_cli(&args));
    }

    // GUI mode needs a compositor session
    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        eprintln!("kuma-shell: no Wayland/X11 session detected");
        std::process::exit(1);
    }

    application().with_assets(KumaAssets).run(|cx: &mut App| {
        let niri = cx.new(|_| NiriState::default());
        kuma_shell::niri::connect(&niri, cx);

        let sysmon = cx.new(|_| SysMon::default());
        kuma_shell::sysmon::run(&sysmon, cx);

        let settings = cx.new(|_| Settings::load());

        let osd = cx.new(|_| kuma_shell::osd::Osd::new(sysmon.clone(), settings.clone()));
        kuma_shell::osd::run(&osd, &sysmon, &settings, cx);

        let notifications = kuma_shell::notifications::start(settings.clone(), cx);
        let tray = kuma_shell::tray::start(cx);
        let nostr = cx.new(|_| kuma_shell::nostr::NostrState::new(notifications.clone()));
        kuma_shell::nostr::run(&nostr, cx);
        let lock = cx.new(|_| kuma_shell::lock::LockState::new(settings.clone()));
        kuma_shell::lock::connect(&lock, cx);
        kuma_shell::idle::run(&settings, &lock, cx);
        cx.set_global(kuma_shell::panel::PanelHost::new(
            settings.clone(),
            sysmon.clone(),
            notifications.clone(),
            nostr.clone(),
        ));

        // the dock: its own layer-shell surface, synced to the dock settings
        kuma_shell::dock::run(niri.clone(), settings.clone(), cx);

        let bar_options = bar_window_options(&settings.read(cx).bar);

        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        cx.open_window(wallpaper_options(), |_, cx| {
            cx.new(|cx| kuma_shell::wallpaper::WallpaperView::new(settings.clone(), cx))
        })
        .expect("failed to open wallpaper window");

        cx.open_window(bar_options, |_, cx| {
            cx.new(|cx| {
                ShellBar::new(
                    niri.clone(),
                    sysmon.clone(),
                    settings.clone(),
                    notifications.clone(),
                    tray.clone(),
                    nostr.clone(),
                    cx,
                )
            })
        })
        .expect("failed to open layer-shell window: compositor must support wlr-layer-shell");

        // TEMP DEBUG
        if std::env::var("KUMA_DEBUG_OPEN_SETTINGS").is_ok() {
            log::info!("debug hook registered");
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(400))
                    .await;
                let _ = cx.update(|cx| {
                    kuma_shell::panel::toggle_panel(kuma_shell::panel::PanelKind::Settings, cx);
                });
            })
            .detach();
        }

        let (bar_toggle_tx, bar_toggle_rx) = smol::channel::unbounded::<kuma_shell::msg::Request>();
        kuma_shell::msg::spawn_listener(bar_toggle_tx);

        let msg_settings = settings.clone();
        let msg_nostr = nostr.clone();
        let msg_sysmon = sysmon.clone();
        cx.spawn(async move |cx| {
            while let Ok(request) = bar_toggle_rx.recv().await {
                log::info!("msg: {request:?}");
                let _ = cx.update(|cx| match request {
                    kuma_shell::msg::Request::Launcher => {
                        kuma_shell::panel::toggle_panel(kuma_shell::panel::PanelKind::Launcher, cx);
                    }
                    kuma_shell::msg::Request::Notifications => {
                        let settings = msg_settings.clone();
                        settings.update(cx, |settings, cx| {
                            let dnd = !settings.notifications.dnd;
                            settings.set_notifications_dnd(dnd, cx);
                        });
                    }
                    kuma_shell::msg::Request::Volume(cmd) => {
                        msg_sysmon.update(cx, |sysmon, cx| match cmd {
                            kuma_shell::msg::VolumeCmd::Up => sysmon.request_volume(5, cx),
                            kuma_shell::msg::VolumeCmd::Down => sysmon.request_volume(-5, cx),
                            kuma_shell::msg::VolumeCmd::Mute => sysmon.request_mute_toggle(cx),
                        });
                    }
                    kuma_shell::msg::Request::Brightness(cmd) => {
                        msg_sysmon.update(cx, |sysmon, cx| match cmd {
                            kuma_shell::msg::BrightnessCmd::Up => sysmon.request_brightness(5, cx),
                            kuma_shell::msg::BrightnessCmd::Down => {
                                sysmon.request_brightness(-5, cx)
                            }
                        });
                    }
                    kuma_shell::msg::Request::MicMute => {
                        msg_sysmon.update(cx, |sysmon, cx| sysmon.request_mic_toggle(cx));
                    }
                    kuma_shell::msg::Request::Nostr(uri) => {
                        // The scheme handler's landing: the offer
                        // arrives even with the panel open; the view
                        // watches the entity and lands on Pair. The
                        // panel is opened, not toggled: a clicked link
                        // must never close the panel over its own offer.
                        msg_nostr.update(cx, |state, cx| state.offer(uri, cx));
                        let open = cx
                            .global::<kuma_shell::panel::PanelHost>()
                            .is_open(&kuma_shell::panel::PanelKind::Nostr);
                        if !open {
                            kuma_shell::panel::toggle_panel(
                                kuma_shell::panel::PanelKind::Nostr,
                                cx,
                            );
                        }
                    }
                    kuma_shell::msg::Request::NostrPanel => {
                        // The keybind's landing: the same open, no
                        // offer. A second press toggles closed; the
                        // panel belongs to whoever opened it last.
                        let open = cx
                            .global::<kuma_shell::panel::PanelHost>()
                            .is_open(&kuma_shell::panel::PanelKind::Nostr);
                        if !open {
                            kuma_shell::panel::toggle_panel(
                                kuma_shell::panel::PanelKind::Nostr,
                                cx,
                            );
                        }
                    }
                });
            }
        })
        .detach();
    });
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

fn run_cli(args: &[String]) -> i32 {
    // accepts both `kuma-shell volume-up` and noctalia-compatible `kuma-shell msg volume-up`
    let mut iter = args.iter().map(String::as_str);
    let mut action = iter.next().unwrap_or_default();
    if action == "msg" {
        action = iter.next().unwrap_or_default();
    }
    let result = match action {
        "volume-up" => cli_av(
            &kuma_shell::msg::Request::Volume(kuma_shell::msg::VolumeCmd::Up),
            || kuma_shell::sysmon::change_volume(5),
        ),
        "volume-down" => cli_av(
            &kuma_shell::msg::Request::Volume(kuma_shell::msg::VolumeCmd::Down),
            || kuma_shell::sysmon::change_volume(-5),
        ),
        "volume-mute" | "mute" => cli_av(
            &kuma_shell::msg::Request::Volume(kuma_shell::msg::VolumeCmd::Mute),
            kuma_shell::sysmon::toggle_mute,
        ),
        "mic-mute" => cli_av(
            &kuma_shell::msg::Request::MicMute,
            kuma_shell::sysmon::toggle_mic,
        ),
        "launcher-toggle" => kuma_shell::msg::send(&kuma_shell::msg::Request::Launcher),
        "notifications-dnd" => kuma_shell::msg::send(&kuma_shell::msg::Request::Notifications),
        // the scheme handler's road: `kuma-shell msg nostr <nostrconnect://…>`,
        // the URI is argv end to end, never a shell string. Bare
        // `nostr` (no URI) is the keybind's road: open the panel.
        "nostr" => {
            let uri = iter.collect::<Vec<_>>().join(" ");
            if uri.is_empty() {
                kuma_shell::msg::send(&kuma_shell::msg::Request::NostrPanel)
            } else {
                kuma_shell::msg::send(&kuma_shell::msg::Request::Nostr(uri))
            }
        }
        "media" => match iter.next() {
            Some("play-pause" | "toggle") => kuma_shell::sysmon::play_pause(),
            Some("stop") => kuma_shell::sysmon::stop_track(),
            Some("next") => kuma_shell::sysmon::next_track(),
            Some("previous" | "prev") => kuma_shell::sysmon::previous_track(),
            other => {
                eprintln!("kuma-shell: unknown media command {other:?}");
                eprintln!("usage: kuma-shell msg media <play-pause|stop|next|previous>");
                return 1;
            }
        },
        // optional step: `msg brightness-up 10` (default 5)
        "brightness-up" => {
            let step = iter.next().and_then(|step| step.parse().ok()).unwrap_or(5);
            // the wire's Up steps by the shell's own five; a custom
            // step stays standalone, which steps exactly
            if step == 5 {
                cli_av(
                    &kuma_shell::msg::Request::Brightness(kuma_shell::msg::BrightnessCmd::Up),
                    move || kuma_shell::sysmon::change_brightness(step),
                )
            } else {
                kuma_shell::sysmon::change_brightness(step)
            }
        }
        "brightness-down" => {
            let step = iter.next().and_then(|step| step.parse().ok()).unwrap_or(5);
            if step == 5 {
                cli_av(
                    &kuma_shell::msg::Request::Brightness(kuma_shell::msg::BrightnessCmd::Down),
                    move || kuma_shell::sysmon::change_brightness(-step),
                )
            } else {
                kuma_shell::sysmon::change_brightness(-step)
            }
        }
        "workspace" => {
            let Some(number) = iter.next().and_then(|n| n.parse().ok()) else {
                eprintln!("usage: kuma-shell msg workspace <1-based workspace number>");
                return 1;
            };
            kuma_shell::niri::focus_workspace_index(number)
        }
        other => {
            eprintln!("kuma-shell: unknown command {other:?}");
            eprintln!(
                "usage: kuma-shell [msg] volume-up | volume-down | volume-mute | mute | mic-mute | launcher-toggle | notifications-dnd | nostr [uri] | media <play-pause|stop|next|previous> | brightness-up [step] | brightness-down [step] | workspace <n>"
            );
            return 1;
        }
    };
    if let Err(err) = result {
        eprintln!("kuma-shell: {err:#}");
        return 1;
    }
    0
}

/// The audio and brightness verbs prefer the running shell: applied
/// in-process, the widgets and the OSD react at once, with no second
/// wpctl round trip racing the shell's. With no shell listening, the
/// request errors and the standalone path runs instead.
fn cli_av(
    request: &kuma_shell::msg::Request,
    standalone: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if kuma_shell::msg::send_quiet(request).is_ok() {
        return Ok(());
    }
    standalone()
}

fn bar_window_options(config: &BarConfig) -> WindowOptions {
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
            anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            exclusive_zone: Some(px(config.height + config.offset_top)),
            keyboard_interactivity: KeyboardInteractivity::None,
            ..Default::default()
        }),
        ..Default::default()
    }
}
