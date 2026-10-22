//! The NetworkManager watcher: NM's own D-Bus signals stand in for
//! the nmcli spawns behind the shell's network state (online, wifi,
//! ssid, radio). The manager and every device emit property changes;
//! the watcher listens once on the service-wide signal stream,
//! recomputes the state from properties on each event, and lands it
//! on the entity. A state that changes a few times a day stopped
//! paying three nmcli spawns every two seconds.
//!
//! The nmcli poll stays as the fallback seam: `watch_live` gates the
//! sysmon pass's idle state reads off while the bus is healthy, and
//! when the watcher dies the flag clears and the poll takes the
//! state back. The Wi-Fi panel's AP scan keeps its nmcli paths (the
//! watcher never touches the scan-owned lists, it preserves them).

use std::sync::atomic::{AtomicBool, Ordering};

use futures_lite::StreamExt;
use zbus::zvariant::OwnedObjectPath;

use crate::sysmon::{NetworkState, SysMon};

/// NetworkManager's bus coordinates: the manager object speaks its
/// own interface on its own path, and every signal the service emits
/// arrives on the manager proxy's stream.
const NM: &str = "org.freedesktop.NetworkManager";
const PATH: &str = "/org/freedesktop/NetworkManager";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";

/// Only an activated device carries a connection.
const STATE_ACTIVATED: u32 = 100;
/// The device types the bar's internet widget understands: the
/// first activated wifi or ethernet device speaks for the machine,
/// exactly as nmcli's device status did.
const TYPE_ETHERNET: u32 = 1;
const TYPE_WIFI: u32 = 2;
/// NM's connectivity: NONE is the only offline answer. Unknown and
/// the portal states count as online, matching the old nmcli read
/// where anything but "none" was online.
const CONNECTIVITY_NONE: u32 = 2;

/// Set once the watcher has landed its first read. The sysmon
/// network pass stands its idle state reads down while it holds.
/// Cleared when the watcher dies: the poll takes the state back.
static WATCH_LIVE: AtomicBool = AtomicBool::new(false);

/// Whether the D-Bus watcher owns the network state right now.
pub fn watch_live() -> bool {
    WATCH_LIVE.load(Ordering::Relaxed)
}

/// Start the watcher. It dies quiet: the error is logged, the flag
/// clears, and the nmcli poll takes the state back.
pub fn spawn(state: gpui::WeakEntity<SysMon>, cx: &mut gpui::App) {
    cx.spawn(async move |cx| {
        if let Err(err) = run(state, cx).await {
            log::error!("nm watcher: {err:#}");
        }
        WATCH_LIVE.store(false, Ordering::Relaxed);
    })
    .detach();
}

async fn run(state: gpui::WeakEntity<SysMon>, cx: &mut gpui::AsyncApp) -> anyhow::Result<()> {
    let connection = zbus::connection::Builder::system()?.build().await?;
    let manager = proxy(&connection, PATH, NM).await?;
    // subscribe before the first read: anything that changes under
    // the initial land queues on the stream and re-syncs on tick one
    let mut signals = manager.receive_all_signals().await?;

    land(&state, cx, read_state(&connection).await?).await;
    WATCH_LIVE.store(true, Ordering::Relaxed);

    while let Some(msg) = signals.next().await {
        // the service-wide stream carries every signal NM emits;
        // property changes are the ones that move the state
        let header = msg.header();
        if header.member().map(|m| m.as_str()) != Some("PropertiesChanged") {
            continue;
        }
        match read_state(&connection).await {
            Ok(network) => land(&state, cx, network).await,
            // a missed read (NM restarting under us) is not fatal:
            // the next signal re-syncs the state
            Err(err) => log::error!("nm recompute: {err:#}"),
        }
    }
    anyhow::bail!("signal stream ended");
}

/// An untyped proxy with caching off: the watcher reads on demand,
/// on the event, and a cached property could serve the stale value
/// the signal that woke us was about to replace.
async fn proxy(
    connection: &zbus::Connection,
    path: &str,
    interface: &str,
) -> zbus::Result<zbus::Proxy<'static>> {
    zbus::proxy::Builder::<zbus::Proxy>::new(connection)
        .destination(NM.to_string())?
        .path(path.to_string())?
        .interface(interface.to_string())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

/// The whole network state, read from properties: two manager gets,
/// two gets per device, one per active connection. No fork, no
/// exec, no thread.
async fn read_state(connection: &zbus::Connection) -> zbus::Result<NetworkState> {
    let manager = proxy(connection, PATH, NM).await?;
    let online = manager.get_property::<u32>("Connectivity").await? != CONNECTIVITY_NONE;
    let wifi_enabled: bool = manager.get_property("WirelessEnabled").await?;
    let devices: Vec<OwnedObjectPath> = manager.get_property("Devices").await?;

    let mut network = NetworkState {
        online,
        wifi: false,
        ssid: None,
        wifi_enabled,
        access_points: Vec::new(),
        saved: Vec::new(),
    };
    let mut probed = Vec::with_capacity(devices.len());
    for path in &devices {
        let device = proxy(connection, path.as_str(), DEVICE).await?;
        // a device can leave between the list and the read; it
        // simply does not count this round
        let state: u32 = match device.get_property("State").await {
            Ok(state) => state,
            Err(_) => continue,
        };
        let kind: u32 = device.get_property("DeviceType").await.unwrap_or(0);
        probed.push((kind, state));
    }
    if let Some(kind) = pick_device(&probed) {
        network.wifi = kind == TYPE_WIFI;
        network.ssid = find_connection_id(connection, &devices, kind).await;
    }
    Ok(network)
}

/// The speaker of the machine, in NM's own order: the first
/// activated wifi or ethernet device. Pure so the order rule has a
/// test.
fn pick_device(probed: &[(u32, u32)]) -> Option<u32> {
    probed
        .iter()
        .find(|(kind, state)| {
            *state == STATE_ACTIVATED && (*kind == TYPE_ETHERNET || *kind == TYPE_WIFI)
        })
        .map(|(kind, _)| *kind)
}

/// The active connection's name for the first activated device of
/// `kind`: the field nmcli's device status showed as CONNECTION
/// (for wireless, the network's name).
async fn find_connection_id(
    connection: &zbus::Connection,
    devices: &[OwnedObjectPath],
    kind: u32,
) -> Option<String> {
    for path in devices {
        let device = match proxy(connection, path.as_str(), DEVICE).await {
            Ok(device) => device,
            Err(_) => continue,
        };
        if device.get_property::<u32>("DeviceType").await.unwrap_or(0) != kind {
            continue;
        }
        if device.get_property::<u32>("State").await.unwrap_or(0) != STATE_ACTIVATED {
            continue;
        }
        let Ok(active) = device.get_property::<OwnedObjectPath>("ActiveConnection").await
        else {
            continue;
        };
        if active.as_str() == "/" {
            continue;
        }
        let Ok(active) = proxy(connection, active.as_str(), ACTIVE).await else {
            continue;
        };
        if let Ok(id) = active.get_property::<String>("Id").await {
            return Some(id);
        }
    }
    None
}

/// Land a fresh read. The AP list and the saved names are
/// scan-owned (the panel's nmcli scans mint them): a state landing
/// preserves both, and a radio flip drops the stale points, whose
/// radio can no longer see them.
async fn land(state: &gpui::WeakEntity<SysMon>, cx: &mut gpui::AsyncApp, mut network: NetworkState) {
    let _ = state.update(cx, |sysmon, cx| {
        if let Some(current) = &sysmon.network {
            network.saved = current.saved.clone();
            if network.wifi_enabled {
                network.access_points = current.access_points.clone();
            }
        }
        if sysmon.network.as_ref() != Some(&network) {
            sysmon.network = Some(network);
            cx.notify();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_none_is_offline() {
        // the old nmcli read: anything but "none" was online, even
        // the unknown answer of a network without connectivity
        // checking
        assert!(!pick_online(CONNECTIVITY_NONE));
        assert!(pick_online(1)); // unknown
        assert!(pick_online(3)); // portal
        assert!(pick_online(4)); // limited
        assert!(pick_online(5)); // full
    }

    fn pick_online(connectivity: u32) -> bool {
        connectivity != CONNECTIVITY_NONE
    }

    #[test]
    fn the_first_activated_wifi_or_ethernet_device_speaks() {
        // NM's own order decides: loopback (19) never counts, an
        // ethernet device ahead of a wifi one wins
        assert_eq!(pick_device(&[(19, 100), (2, 100), (1, 100)]), Some(TYPE_WIFI));
        assert_eq!(pick_device(&[(19, 100), (1, 100)]), Some(TYPE_ETHERNET));
        // connecting devices (state 30 and below) do not
        assert_eq!(pick_device(&[(2, 30), (1, 10)]), None);
        assert_eq!(pick_device(&[]), None);
    }
}
