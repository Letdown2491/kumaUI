//! The night light: one Wayland connection speaking
//! wlr-gamma-control-unstable-v1, applying a color-temperature ramp
//! to every output on a schedule. The second use of the idle.rs
//! pattern: independent connection, no GUI state, and one honest
//! logged line when the compositor lacks the protocol.

use std::os::fd::{AsFd, AsRawFd, FromRawFd};

use gpui::{App, Entity};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_output, wl_registry},
};
use wayland_protocols_wlr::gamma_control::v1::client::{
    zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
    zwlr_gamma_control_v1::{Event as GammaEvent, ZwlrGammaControlV1},
};

use crate::settings::{NightLightConfig, Settings};

/// How often the loop wakes: fast enough that a settings change
/// applies before the user notices a delay (well under a second),
/// while the Wayland fd stays quiet between events.
const LOOP_TICK_MS: i32 = 500;

/// How often the schedule is re-evaluated and the ramp re-applied:
/// the window's grain is a minute.
const APPLY_TICK_SECS: u64 = 60;

/// A Failed control's output re-asks after this long, doubling per
/// failure to the cap: a compositor that rejects gamma outright (a
/// headless output, a driver without tables) settles at one rebind a
/// minute instead of a tight loop that out-races the compositor's
/// dispatch and logs tens of thousands of lines a second.
const REBIND_COOLDOWN_START_MS: u64 = 1_000;
const REBIND_COOLDOWN_MAX_MS: u64 = 60_000;

/// One Wayland connection for the shell's lifetime: the config
/// arrives through shared state and the loop reads it each tick, so
/// a settings change never re-mints the client (a new connection
/// would drop the output's exclusive gamma control for a moment,
/// and niri restores neutral gamma in that gap: a visible flicker).
pub fn run(settings: &Entity<Settings>, cx: &mut App) {
    use gpui::Entity;
    let shared = NightShared {
        config: std::sync::Mutex::new(settings.read(cx).night_light.clone()),
        version: std::sync::atomic::AtomicU64::new(0),
    };
    let shared = std::sync::Arc::new(shared);

    cx.observe(settings, {
        let shared = shared.clone();
        move |settings: Entity<Settings>, cx: &mut App| {
            *shared.config.lock().unwrap() = settings.read(cx).night_light.clone();
            shared
                .version
                .store(shared.version.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
    })
    .detach();

    std::thread::Builder::new()
        .name("kuma-night-light".into())
        .spawn(move || {
            // a dead connection (compositor restart) retries rather
            // than ending the feature
            loop {
                if let Err(err) = apply_loop(shared.clone()) {                    log::error!("night light stopped: {err:#}; retrying in 5s");
                    std::thread::sleep(std::time::Duration::from_secs(5));
                } else {
                    return;
                }
            }
        })
        .expect("spawn kuma-night-light thread");
}

type SharedConfig = std::sync::Arc<NightShared>;

struct NightShared {
    config: std::sync::Mutex<NightLightConfig>,
    version: std::sync::atomic::AtomicU64,
}

use std::sync::atomic::Ordering;

fn apply_loop(shared: SharedConfig) -> anyhow::Result<()> {
    let mut last_version = shared.version.load(Ordering::Relaxed);
    let _config = shared.config.lock().unwrap().clone();
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<NightApp>(&connection)?;
    let qh = queue.handle();
    let manager: ZwlrGammaControlManagerV1 = globals.bind(&qh, 1..=1, ()).map_err(|_| {
        anyhow::anyhow!(
            "the compositor does not advertise wlr-gamma-control-unstable-v1; the night light is off"
        )
    })?;

    let mut app = NightApp {
        manager,
        outputs: Vec::new(),
        controls: Vec::new(),
        rebind_at: Vec::new(),
    };

    // Every output that exists now, and every one that arrives later
    // (a monitor plugged in, a lid reopened), gets a gamma control.
    for global in globals.contents().clone_list() {
        if global.interface == wl_output::WlOutput::interface().name {
            // bind panics on a bad version, and version 1 is
            // guaranteed: every wl_output speaks it
            let output =
                globals
                    .registry()
                    .bind::<wl_output::WlOutput, _, _>(global.name, 1, &qh, ());
            app.outputs.push(output);
        }
    }

    let mut needs_apply = true;
    let mut last_tick = std::time::Instant::now();
    loop {
        queue.dispatch_pending(&mut app)?;
        queue.roundtrip(&mut app)?;
        // reclaim controls: a Failed (an output re-configuring, a
        // compositor recovering) is recovered from by asking again
        app.bind_missing(&qh);
        let sized = app.controls.iter().any(|(_, _, size)| size.is_some());

        let version = shared.version.load(Ordering::Relaxed);
        if version != last_version {
            last_version = version;
            needs_apply = true;
        }
        if last_tick.elapsed() >= std::time::Duration::from_secs(APPLY_TICK_SECS) {
            // the schedule clock moved: the window may have opened
            // or closed with no settings change to announce it
            needs_apply = true;
            last_tick = std::time::Instant::now();
        }
        if needs_apply && sized {
            let config = shared.config.lock().unwrap().clone();
            let now = local_hm();
            let tinted = config.active_now(now);
            let kelvin = if tinted { config.kelvin } else { 6500 };
            app.apply(kelvin)?;
            needs_apply = false;
        }

        connection.flush()?;
        match connection.prepare_read() {
            Some(guard) => {
                let mut fds = [libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                }];
                let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, LOOP_TICK_MS) };
                if ready > 0 {
                    guard.read()?;
                }
            }
            None => {
                std::thread::sleep(std::time::Duration::from_millis(
                    LOOP_TICK_MS as u64,
                ));
            }
        }
    }
}

/// The local wall clock as (hour, minute), the schedule's grain.
fn local_hm() -> (u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // eastern day + the local offset, then mod the day: good enough
    // for a minute-grained window, and zoneinfo-free
    let offset = local_offset_minutes(secs);
    let mins = ((secs / 60 + offset as u64) % 1440) as u32;
    (mins / 60, mins % 60)
}

/// The local UTC offset in minutes, east positive: `date +%z` once,
/// cached. Calling out per minute-grain tick is nothing.
fn local_offset_minutes(_secs: u64) -> i32 {
    static OFFSET: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| {
        std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|out| {
                let text = String::from_utf8_lossy(&out.stdout);
                let text = text.trim();
                let (sign, rest) = text.split_at(1);
                let hours: i32 = rest.get(..2)?.parse().ok()?;
                let minutes: i32 = rest.get(2..4)?.parse().ok()?;
                let magnitude = hours * 60 + minutes;
                Some(if sign == "-" { -magnitude } else { magnitude })
            })
            .unwrap_or(0)
    })
}

struct NightApp {
    manager: ZwlrGammaControlManagerV1,
    /// Every output the compositor advertises; a control is (re-)
    /// requested for any of them that lacks one.
    outputs: Vec<wl_output::WlOutput>,
    /// The live controls and the ramp size each has reported.
    controls: Vec<(wl_output::WlOutput, ZwlrGammaControlV1, Option<u16>)>,
    /// Outputs whose control Failed, with the time of the last failure
    /// and the wait before re-asking (doubles per failure, capped).
    rebind_at: Vec<(wl_output::WlOutput, std::time::Instant, u64)>,
}

impl NightApp {
    /// Ask for a control on every output that lacks one. A control can
    /// fail transiently (the old client's exclusive hold during a
    /// settings change, an output re-configuring), so this runs every
    /// loop and the failures are retried after a cooldown that doubles
    /// per failure: a permanent rejection costs a rebind a minute,
    /// not a loop that never lets the connection sleep.
    fn bind_missing(&mut self, qh: &QueueHandle<Self>) {
        for output in &self.outputs {
            if self.controls.iter().any(|(known, _, _)| known == output) {
                continue;
            }
            if let Some((_, at, wait)) = self
                .rebind_at
                .iter_mut()
                .find(|(known, _, _)| known == output)
            {
                if at.elapsed() < std::time::Duration::from_millis(*wait) {
                    continue;
                }
                *at = std::time::Instant::now();
                *wait = (*wait * 2).min(REBIND_COOLDOWN_MAX_MS);
            }
            let control = self.manager.get_gamma_control(output, qh, ());
            self.controls.push((output.clone(), control, None));
        }
    }

    /// Push the ramp for `kelvin` to every output. The first GammaSize
    /// event settles the ramp size; outputs report the same size, so
    /// one is enough.
    fn apply(&mut self, kelvin: u32) -> anyhow::Result<()> {
        let Some(size) = self.controls.iter().find_map(|(_, _, size)| *size) else {
            log::info!(
                "night light: nothing to apply yet ({} controls, none have a size)",
                self.controls.len()
            );
            return Ok(());
        };
        log::info!(
            "night light: applying {kelvin}K to {} output(s), ramp size {size}",
            self.controls.len()
        );

        // red ramp first, then green, then blue; 16-bit native order
        let ramp = crate::settings::kelvin_ramp(kelvin, size);
        let mut table = Vec::with_capacity(size as usize * 6);
        for channel in 0..3 {
            for entry in &ramp {
                table.extend_from_slice(&entry[channel].to_ne_bytes());
            }
        }
        let file = memfd(&table)?;
        for (_, control, _) in &self.controls {
            control.set_gamma(file.as_fd());
        }
        Ok(())
    }
}

/// An anonymous memory file holding the ramp, for the set_gamma fd.
fn memfd(bytes: &[u8]) -> anyhow::Result<std::fs::File> {
    let fd = unsafe { libc::memfd_create(c"kuma-gamma-ramp".as_ptr(), 0) };
    if fd < 0 {
        anyhow::bail!("memfd_create failed");
    }
    use std::io::{Seek, Write};
    let mut file = std::fs::File::from(unsafe {
        std::os::fd::OwnedFd::from_raw_fd(fd)
    });
    file.write_all(bytes)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    Ok(file)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for NightApp {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == wl_output::WlOutput::interface().name
        {
            // a new output: bind it (version 1 keeps its events boring)
            let _ = version;
            // bind panics on a bad version, and version 1 is
            // guaranteed: every wl_output speaks it
            let output = registry.bind::<wl_output::WlOutput, _, _>(name, 1, qh, ());
            state.outputs.push(output);
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for NightApp {
    fn event(
        _state: &mut Self,
        _output: &wl_output::WlOutput,
        _event: wl_output::Event,
        _data: &(),
        _connection: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // geometry and modes: nothing the ramp needs
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for NightApp {
    fn event(
        _state: &mut Self,
        _manager: &ZwlrGammaControlManagerV1,
        _event: <ZwlrGammaControlManagerV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // the manager has no events
    }
}

impl Dispatch<ZwlrGammaControlV1, ()> for NightApp {
    fn event(
        state: &mut Self,
        control: &ZwlrGammaControlV1,
        event: GammaEvent,
        _data: &(),
        _connection: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            GammaEvent::GammaSize { size } => {
                let size = u16::try_from(size).unwrap_or(256);
                for entry in &mut state.controls {
                    if entry.1 == *control {
                        entry.2 = Some(size);
                    }
                }
            }
            // the output or the ramp was rejected: drop the control so
            // the loop stops pushing to it, and arm the rebind
            // cooldown (a transient failure rebinds after a second; a
            // permanent one backs off to a minute)
            GammaEvent::Failed => {
                log::info!("night light: a gamma control failed; dropping it");
                let owner = state
                    .controls
                    .iter()
                    .find(|(_, c, _)| c == control)
                    .map(|(o, _, _)| o.clone());
                state.controls.retain(|(_, c, _)| c != control);
                let _ = control.destroy();
                if let Some(output) = owner {
                    match state
                        .rebind_at
                        .iter_mut()
                        .find(|(known, _, _)| known == &output)
                    {
                        Some((_, at, wait)) => {
                            *at = std::time::Instant::now();
                            *wait = (*wait * 2).min(REBIND_COOLDOWN_MAX_MS);
                        }
                        None => state.rebind_at.push((
                            output,
                            std::time::Instant::now(),
                            REBIND_COOLDOWN_START_MS,
                        )),
                    }
                }
            }
            _ => {}
        }
    }
}
