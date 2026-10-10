use kuma_shell::{icons::KumaAssets, session::SessionState, sysmon::SysMon};

use gpui::{App, AppContext, QuitMode};

use kuma_shell::settings::Settings;

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

    // Startup instrumentation: every marker logs elapsed-since-main, so
    // one journalctl pass over a login prices the phases (config read,
    // gpui init, first surfaces) against the greetd handoff outside the
    // process. Cheap enough to leave in: an Instant and a few logs.
    let started = std::time::Instant::now();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(run_cli(&args));
    }

    // GUI mode needs a compositor session
    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        eprintln!("kuma-shell: no Wayland/X11 session detected");
        std::process::exit(1);
    }

    log::info!("boot: entering gpui, {}ms", started.elapsed().as_millis());

    application()
        .with_quit_mode(QuitMode::Explicit)
        .with_assets(KumaAssets)
        .run(move |cx: &mut App| {
            log::info!("boot: app closure, {}ms", started.elapsed().as_millis());
            let niri = cx.new(|_| SessionState::default());
            kuma_shell::session::connect(&niri, cx);
            log::info!("boot: session connected, {}ms", started.elapsed().as_millis());

            let settings = cx.new(|_| Settings::load());
            settings.update(cx, |settings, cx| settings.refresh_theme(cx));
            log::info!("boot: settings loaded, {}ms", started.elapsed().as_millis());

            kuma_shell::wallpaper::run(&settings, cx);
            kuma_shell::night_light::run(&settings, cx);

            let sysmon = cx.new(|_| SysMon::default());
            kuma_shell::sysmon::run(&sysmon, &settings, cx);

            let osd = cx.new(|_| kuma_shell::osd::Osd::new(sysmon.clone(), settings.clone()));
            kuma_shell::osd::run(&osd, &sysmon, &settings, cx);

            let notifications = kuma_shell::notifications::start(settings.clone(), cx);
            let tray = kuma_shell::tray::start(cx);
            let nostr = cx.new(|_| kuma_shell::nostr::NostrState::new(notifications.clone()));
            kuma_shell::nostr::run(&nostr, cx);
            let weather = cx.new(|_| kuma_shell::weather::WeatherState::default());
            kuma_shell::weather::run(&weather, &settings, cx);
            let lock = cx.new(|_| kuma_shell::lock::LockState::new(settings.clone()));
            kuma_shell::lock::connect(&lock, cx);
            kuma_shell::idle::run(&settings, &lock, cx);
            kuma_shell::polkit::run(&lock, cx);
            cx.set_global(kuma_shell::panel::PanelHost::new(
                settings.clone(),
                sysmon.clone(),
                notifications.clone(),
                nostr.clone(),
                weather.clone(),
            ));

            // the dock: its own layer-shell surface, synced to the dock settings
            kuma_shell::dock::run(niri.clone(), settings.clone(), cx);

            let (bar_toggle_tx, bar_toggle_rx) =
                smol::channel::unbounded::<kuma_shell::msg::Request>();
            kuma_shell::msg::spawn_listener(bar_toggle_tx);

            let msg_settings = settings.clone();
            let msg_nostr = nostr.clone();
            let msg_sysmon = sysmon.clone();

            // The persistent surfaces (wallpaper, bar) and the watch that
            // keeps them alive: opened by the same `ensure` pass that
            // recreates them after their outputs go away, so startup and
            // recovery are one code path. The shell does not quit when
            // windows close: gpui's default quit mode exits on the last
            // window close (QuitMode::Default on non-macOS, set above), so
            // the builder pins QuitMode::Explicit and the shell idles
            // displayless (bus names held, and no logind sleep inhibitor:
            // the shell deliberately takes no part in the suspend path)
            // while the watch brings the surfaces back when outputs do.
            // Exit is the compositor's death, which is session end.
            kuma_shell::surfaces::init(
                kuma_shell::surfaces::SurfaceDeps {
                    niri,
                    sysmon,
                    settings,
                    notifications,
                    tray,
                    nostr,
                    weather,
                    lock,
                },
                cx,
            );
            kuma_shell::surfaces::ensure(cx);
            kuma_shell::surfaces::watch(cx);
            log::info!("boot: surfaces up, {}ms", started.elapsed().as_millis());

            // Hand freed heap back to the OS on a slow cadence. glibc's
            // per-thread arenas never shrink on their own: weeks of panel
            // and toast churn leave every page a session ever touched
            // mapped, and each suspend cycle swaps the lot out for good.
            // malloc_trim(0) walks all arenas and madvises the free ones
            // away; it is cheap when there is nothing to release.
            let background = cx.background_executor().clone();
            cx.background_spawn(async move {
                loop {
                    background.timer(std::time::Duration::from_secs(30)).await;
                    // SAFETY: malloc_trim takes the arena locks itself
                    // and only madvises pages nothing can reach.
                    unsafe { libc::malloc_trim(0) };
                }
            })
            .detach();

            cx.spawn(async move |cx| {
                while let Ok(request) = bar_toggle_rx.recv().await {
                    log::info!("msg: {request:?}");
                    let _ = cx.update(|cx| match request {
                        kuma_shell::msg::Request::Launcher => {
                            kuma_shell::panel::toggle_panel(
                                kuma_shell::panel::PanelKind::Launcher,
                                cx,
                            );
                        }
                        kuma_shell::msg::Request::Settings => {
                            kuma_shell::panel::toggle_panel(
                                kuma_shell::panel::PanelKind::Settings,
                                cx,
                            );
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
                                kuma_shell::msg::BrightnessCmd::Up => {
                                    sysmon.request_brightness(5, cx)
                                }
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
                            let open = kuma_shell::panel::is_open(
                                &kuma_shell::panel::PanelKind::Nostr,
                                cx,
                            );
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
                            let open = kuma_shell::panel::is_open(
                                &kuma_shell::panel::PanelKind::Nostr,
                                cx,
                            );
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
        "settings" => kuma_shell::msg::send(&kuma_shell::msg::Request::Settings),
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
            kuma_shell::session::focus_workspace_index(number)
        }
        other => {
            eprintln!("kuma-shell: unknown command {other:?}");
            eprintln!(
                "usage: kuma-shell [msg] volume-up | volume-down | volume-mute | mute | mic-mute | launcher-toggle | settings | notifications-dnd | nostr [uri] | media <play-pause|stop|next|previous> | brightness-up [step] | brightness-down [step] | workspace <n>"
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
