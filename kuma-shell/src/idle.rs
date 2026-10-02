//! The idle watcher: one Wayland connection speaking ext-idle-notify-v1,
//! two clocks (the lock's and the monitors') and a bridge that hands
//! what fires to the main loop.
//!
//! This is the third clause of the swayidle line the shell replaced
//! (lock at 15 minutes, screens off a minute later, lock before
//! sleep; the last of those rides logind in `lock.rs`). The
//! compositor owns idleness: every app's own idle-inhibit (a video
//! playing, a download bar) counts as activity, which a screen-blanker
//! polling input cannot see and exactly why the protocol exists.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use std::os::fd::AsRawFd;

use gpui::{App, AppContext, Entity};
use smol::channel::{Sender, unbounded};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat::WlSeat},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{Event as NotificationEvent, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

use crate::lock;
use crate::settings::{IdleSettings, Settings};

/// How often the watcher's loop wakes to check its generation: a
/// settings change retires the watcher within this bound, because a
/// blocking Wayland dispatch cannot be interrupted from outside.
const LOOP_TICK_MS: i32 = 500;

/// Which clock fired.
#[derive(Clone, Copy, Debug)]
pub enum IdleEvent {
    /// The lock timeout was reached.
    Lock,
    /// The monitor timeout was reached: DPMS off, niri powers the
    /// monitors back on at the next input.
    ScreenOff,
}

/// The watcher's version tag. A settings change bumps the counter and
/// spawns a fresh watcher with the new timeouts; every watcher carries
/// the value it was minted with and exits when the counter moves.
type Generation = Arc<AtomicU64>;

/// Start the idle watcher against the current settings, and keep it on
/// the settings' timeouts across changes. The Wayland connection lives
/// on a plain thread (it touches no GUI state); what fires comes back
/// through a channel to the main loop.
pub fn run(settings: &Entity<Settings>, lock: &Entity<lock::LockState>, cx: &mut App) {
    let (tx, rx) = unbounded::<IdleEvent>();
    let generation: Generation = Arc::new(AtomicU64::new(0));

    spawn_watcher_with(0, settings.read(cx).idle, generation.clone(), tx.clone());

    // A settings change re-mints the watcher. Comparing against the
    // last applied value keeps an unrelated notification (the bar's
    // volume, the wallpaper) from spawning threads for nothing.
    cx.observe(settings, {
        let generation = generation.clone();
        let tx = tx.clone();
        let applied = std::cell::Cell::new(settings.read(cx).idle);
        move |settings: Entity<Settings>, cx: &mut App| {
            let idle = settings.read(cx).idle;
            if idle != applied.get() {
                applied.set(idle);
                let stamp = generation.fetch_add(1, Ordering::Relaxed) + 1;
                spawn_watcher_with(stamp, idle, generation.clone(), tx.clone());
            }
        }
    })
    .detach();

    let lock = lock.clone();
    cx.spawn(async move |cx| {
        while let Ok(event) = rx.recv().await {
            cx.update(|cx| match event {
                IdleEvent::Lock => lock::engage(&lock, cx),
                IdleEvent::ScreenOff => cx
                    .background_spawn(async {
                        let _ = std::process::Command::new("niri")
                            .args(["msg", "action", "power-off-monitors"])
                            .output();
                    })
                    .detach(),
            });
        }
    })
    .detach();
}

fn spawn_watcher_with(
    stamp: u64,
    idle: IdleSettings,
    generation: Generation,
    tx: Sender<IdleEvent>,
) {
    std::thread::Builder::new()
        .name("kuma-idle".into())
        .spawn(move || {
            if let Err(err) = watch(idle, generation, stamp, tx) {
                log::error!("idle watcher stopped: {err:#}");
            }
        })
        .expect("spawn kuma-idle thread");
}

fn watch(
    idle: IdleSettings,
    generation: Generation,
    stamp: u64,
    tx: Sender<IdleEvent>,
) -> anyhow::Result<()> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<IdleApp>(&connection)?;
    let qh = queue.handle();
    let manager: ExtIdleNotifierV1 = globals.bind(&qh, 1..=1, ()).map_err(|_| {
        anyhow::anyhow!("the compositor does not advertise ext-idle-notify-v1; idle locking is off")
    })?;
    // The notification is tied to a seat; idleness is input idleness,
    // and the seat is where input lands. Version 1 keeps the seat's
    // own events (capabilities) boring.
    let seat: WlSeat = globals.bind(&qh, 1..=1, ()).map_err(|_| {
        anyhow::anyhow!("the compositor does not advertise wl_seat; idle locking is off")
    })?;

    // One notification object per clause; the data slot tells the
    // dispatch which clock fired. A timeout of 0 means the clause is
    // off and gets no object at all.
    if idle.lock_timeout > 0 {
        manager.get_idle_notification(
            (idle.lock_timeout.min(u32::MAX as u64 / 1000) * 1000) as u32,
            &seat,
            &qh,
            IdleKind::Lock,
        );
    }
    if idle.screen_off_timeout > 0 {
        manager.get_idle_notification(
            (idle.screen_off_timeout.min(u32::MAX as u64 / 1000) * 1000) as u32,
            &seat,
            &qh,
            IdleKind::ScreenOff,
        );
    }

    let mut watch = IdleApp { tx };
    loop {
        queue.dispatch_pending(&mut watch)?;
        connection.flush()?;
        if generation.load(Ordering::Relaxed) != stamp {
            return Ok(());
        }
        // Wait for Wayland events, but no longer than a tick: the only
        // thing that can retire this thread is noticing its generation
        // moved, and a blocking dispatch would never look.
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
                // Events are already queued; the next dispatch_pending
                // takes them.
                std::thread::sleep(std::time::Duration::from_millis(LOOP_TICK_MS as u64));
            }
        }
    }
}

struct IdleApp {
    tx: Sender<IdleEvent>,
}

#[derive(Clone, Copy, Debug)]
enum IdleKind {
    Lock,
    ScreenOff,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for IdleApp {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Globals past the init roundtrip are of no interest here.
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for IdleApp {
    fn event(
        _state: &mut Self,
        _notifier: &ExtIdleNotifierV1,
        _event: <ExtIdleNotifierV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // The notifier has no events.
    }
}

impl Dispatch<WlSeat, ()> for IdleApp {
    fn event(
        _state: &mut Self,
        _seat: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // capabilities and name: nothing this watcher does with them.
    }
}

impl Dispatch<ExtIdleNotificationV1, IdleKind> for IdleApp {
    fn event(
        state: &mut Self,
        _notification: &ExtIdleNotificationV1,
        event: NotificationEvent,
        data: &IdleKind,
        _connection: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            NotificationEvent::Idled => {
                // The receiver outlives every watcher; a failure would
                // mean the main loop is gone, and then so is the shell.
                let _ = state.tx.try_send(match data {
                    IdleKind::Lock => IdleEvent::Lock,
                    IdleKind::ScreenOff => IdleEvent::ScreenOff,
                });
            }
            // Activity: the notification re-arms itself; the next idle
            // stretch fires again. Nothing to do.
            NotificationEvent::Resumed => {}
            // The generated enum is non-exhaustive (future protocol
            // versions may add events); unknown ones mean nothing here.
            _ => {}
        }
    }
}
