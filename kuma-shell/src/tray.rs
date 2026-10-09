//! The system tray: the shell hosts `org.kde.StatusNotifierWatcher` itself
//! (no external tray process), polls item properties, and exposes
//! [`TrayState`] for the bar. Clicks ride a request channel back out to the
//! dbus task. DBusMenu is not spoken yet: clicks Activate the item.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{App, AppContext, Entity};
use smol::channel::{Receiver, Sender, unbounded};
use zbus::object_server::SignalEmitter;

use crate::imaging::{self, IconImage};

const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_PATH: &str = "/StatusNotifierItem";
/// Item property polls ride the same 2s cadence as the system monitors.
const POLL: Duration = Duration::from_secs(2);
/// Themed icons decode at twice the bar's painted 16px for hidpi screens.
const ICON_RESOLVE_SIZE: u32 = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrayStatus {
    #[default]
    Passive,
    Active,
    NeedsAttention,
}

/// One tray item, snapshot of its dbus properties plus the decoded icon.
#[derive(Clone)]
pub struct TrayItem {
    /// The item's bus destination (well-known name, or the unique name of
    /// the registering client for the ":PID" form).
    pub service: String,
    pub title: String,
    pub status: TrayStatus,
    pub tooltip: String,
    pub icon: Option<IconImage>,
    /// Content hash of the raw pixmap bytes, 0 for named icons. Pixmap
    /// rasters are re-converted fresh every poll; the state side pins the
    /// last render per item by this hash, so its atlas tile survives
    /// replacement (ADR-0016).
    pub pixmap_hash: u64,
    /// The item exports a DBusMenu we don't render yet; Activate still works.
    pub has_menu: bool,
}

impl TrayItem {
    fn parse_status(status: &str) -> TrayStatus {
        match status {
            "Active" => TrayStatus::Active,
            "NeedsAttention" => TrayStatus::NeedsAttention,
            _ => TrayStatus::Passive,
        }
    }
}

/// state → dbus task
enum TrayRequest {
    Activate { service: String, x: i32, y: i32 },
    SecondaryActivate { service: String, x: i32, y: i32 },
}

/// watcher object → dbus task (poll now), and state → dbus task, in one
/// channel so the task's loop selects on a single receiver.
enum TrayInput {
    Wake,
    Request(TrayRequest),
}

/// The whole tray state, on the main thread.
pub struct TrayState {
    items: Vec<TrayItem>,
    /// The last pinned render per pixmap-sourced item, by service, keyed
    /// with the pixmap's content hash. Fresh conversions arrive every
    /// poll; pinning keeps the painted Arc (and its atlas tile) stable
    /// across replacements, and a leaving item's tile is released
    /// (ADR-0016).
    pixmap_rasters: HashMap<String, (u64, Option<IconImage>)>,
    input: Sender<TrayInput>,
}

impl TrayState {
    pub fn items(&self) -> &[TrayItem] {
        &self.items
    }

    /// Left-click: the item's own Activate.
    pub fn activate(&self, service: &str, x: i32, y: i32) {
        let _ = self
            .input
            .try_send(TrayInput::Request(TrayRequest::Activate {
                service: service.to_string(),
                x,
                y,
            }));
    }

    /// Right-click: SecondaryActivate.
    pub fn secondary_activate(&self, service: &str, x: i32, y: i32) {
        let _ = self
            .input
            .try_send(TrayInput::Request(TrayRequest::SecondaryActivate {
                service: service.to_string(),
                x,
                y,
            }));
    }

    /// Swap in a fresh poll's items. Pixmap rasters dedupe by content
    /// hash: an unchanged pixmap keeps the pinned Arc (one atlas tile,
    /// however many polls repaint it). Returns the icons whose tiles must
    /// be released: a replaced pin, or an item that left. Pure list
    /// surgery, testable without a context; the caller does the dropping.
    fn absorb_items(&mut self, items: Vec<TrayItem>) -> Vec<Option<IconImage>> {
        self.items = items;
        let mut droplets = Vec::new();
        let alive: std::collections::HashSet<&str> = self
            .items
            .iter()
            .map(|item| item.service.as_str())
            .collect();
        self.pixmap_rasters.retain(|service, (_, icon)| {
            let keep = alive.contains(service.as_str());
            if !keep {
                droplets.push(icon.clone());
            }
            keep
        });
        for item in &mut self.items {
            if item.pixmap_hash == 0 {
                continue; // named icon: the registry's stable Arc serves it
            }
            match self.pixmap_rasters.get(&item.service) {
                Some((hash, pinned)) if *hash == item.pixmap_hash => {
                    item.icon = pinned.clone();
                }
                _ => {
                    if let Some((_, old)) = self.pixmap_rasters.insert(
                        item.service.clone(),
                        (item.pixmap_hash, item.icon.clone()),
                    ) {
                        droplets.push(old);
                    }
                }
            }
        }
        droplets
    }
}

/// The watcher dbus object. Registration updates the shared destination
/// registry and wakes the poll loop; everything visible happens there.
struct TrayWatcher {
    items: Arc<Mutex<Vec<String>>>,
    input: Sender<TrayInput>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl TrayWatcher {
    async fn register_status_notifier_item(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        service: &str,
    ) -> zbus::fdo::Result<()> {
        // ":PID" form: the item lives at the caller's unique bus name
        let destination = if service.starts_with(':') {
            header
                .sender()
                .map(ToString::to_string)
                .unwrap_or_else(|| service.to_string())
        } else {
            service.to_string()
        };
        let newly_registered = {
            let mut items = self.items.lock().expect("tray registry poisoned");
            if items.contains(&destination) {
                false
            } else {
                items.push(destination.clone());
                true
            }
        }; // guard dropped before the signal await below
        if newly_registered {
            let _ = self.input.try_send(TrayInput::Wake);
            TrayWatcher::status_notifier_item_registered(&emitter, &destination)
                .await
                .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        }
        Ok(())
    }

    async fn register_status_notifier_host(&self, _service: &str) -> zbus::fdo::Result<()> {
        // the shell itself is the host
        Ok(())
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().expect("tray registry poisoned").clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        name: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        name: &str,
    ) -> zbus::Result<()>;
}

/// The typed view of one item's properties.
#[zbus::proxy(
    interface = "org.kde.StatusNotifierItem",
    default_path = "/StatusNotifierItem"
)]
trait StatusNotifierItem {
    #[zbus(property)]
    fn title(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn icon_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn attention_icon_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn icon_pixmap(&self) -> zbus::Result<(i32, i32, Vec<u8>)>;
    #[zbus(property)]
    fn tool_tip(&self) -> zbus::Result<(String, (i32, i32, Vec<u8>), String, String)>;
    #[zbus(property)]
    fn menu(&self) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
}

/// Poll one item into a snapshot. A dbus failure means the item is gone:
/// `Ok(None)` drops it.
async fn poll_item(connection: &zbus::Connection, service: &str) -> Option<TrayItem> {
    let proxy = StatusNotifierItemProxy::builder(connection)
        .destination(service)
        .ok()?
        .path(ITEM_PATH)
        .ok()?
        .build()
        .await
        .ok()?;

    // per-property tolerances: a missing prop is an empty tray item, a
    // failing call is a dead one
    let title = proxy.title().await.unwrap_or_default();
    if title.is_empty() && proxy.status().await.is_err() {
        return None; // unresponsive and untitled: dead
    }
    let status = proxy
        .status()
        .await
        .map(|s| TrayItem::parse_status(&s))
        .unwrap_or_default();
    let icon_name = proxy.icon_name().await.unwrap_or_default();
    let attention_icon_name = proxy.attention_icon_name().await.unwrap_or_default();
    let icon_pixmap = proxy.icon_pixmap().await.ok();
    let tooltip = proxy
        .tool_tip()
        .await
        .map(|(_, _, tip_title, tip_description)| {
            if tip_description.is_empty() {
                tip_title
            } else {
                format!("{tip_title}: {tip_description}")
            }
        })
        .unwrap_or_default();
    let has_menu = proxy.menu().await.is_ok();

    let (icon, pixmap_hash) = resolve_icon(
        if status == TrayStatus::NeedsAttention && !attention_icon_name.is_empty() {
            &attention_icon_name
        } else {
            &icon_name
        },
        icon_pixmap,
    );
    let tooltip = if tooltip.is_empty() {
        title.clone()
    } else {
        tooltip
    };
    Some(TrayItem {
        service: service.to_string(),
        title,
        status,
        tooltip,
        icon,
        pixmap_hash,
        has_menu,
    })
}

/// Named icons resolve through the image registry (stable Arcs: one atlas
/// tile per icon, however often the poll repaints); pixmap items convert
/// fresh per poll, tagged with a content hash, and the state side pins the
/// last render per item so the tile survives replacement (ADR-0016).
fn resolve_icon(
    icon_name: &str,
    pixmap: Option<(i32, i32, Vec<u8>)>,
) -> (Option<IconImage>, u64) {
    if !icon_name.is_empty()
        && let Some(icon) = imaging::resolve(icon_name, ICON_RESOLVE_SIZE)
    {
        return (Some(icon.clone_shared()), 0);
    }
    pixmap
        .map(|(width, height, data)| {
            let mut hash = std::hash::DefaultHasher::new();
            data.hash(&mut hash);
            (
                imaging::argb_to_render_image(width, height, data)
                    .map(Arc::new)
                    .map(IconImage::Raster),
                hash.finish(),
            )
        })
        .unwrap_or((None, 0))
}

/// The dbus task: serve the watcher, poll items on the monitor cadence (and
/// immediately on registration), and run click requests out to items.
async fn run_tray(
    input_tx: Sender<TrayInput>,
    input_rx: Receiver<TrayInput>,
    updates: Sender<Vec<TrayItem>>,
) -> anyhow::Result<()> {
    let registry: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let watcher = TrayWatcher {
        items: registry.clone(),
        input: input_tx,
    };
    let connection = zbus::connection::Builder::session()?
        // both spellings: KDE is the spec, canonical is the legacy probe
        .name("org.kde.StatusNotifierWatcher")?
        .name("com.canonical.StatusNotifierWatcher")?
        .serve_at(WATCHER_PATH, watcher)?
        .build()
        .await?;
    log::info!("tray watcher online at {WATCHER_PATH}");

    loop {
        // input, or the poll tick; None is the tick's sentinel
        match smol::future::or(async { input_rx.recv().await.ok() }, async {
            smol::Timer::after(POLL).await;
            None
        })
        .await
        {
            Some(TrayInput::Wake) => poll_and_publish(&connection, &registry, &updates).await,
            Some(TrayInput::Request(request)) => {
                handle_request(&connection, request).await;
            }
            None => poll_and_publish(&connection, &registry, &updates).await,
        }
    }
}

/// Poll every registered item; dead items drop out of the registry.
async fn poll_and_publish(
    connection: &zbus::Connection,
    registry: &Arc<Mutex<Vec<String>>>,
    updates: &Sender<Vec<TrayItem>>,
) {
    let services = registry.lock().expect("tray registry poisoned").clone();
    let mut items = Vec::new();
    let mut alive = Vec::new();
    for service in services {
        if let Some(item) = poll_item(connection, &service).await {
            alive.push(service);
            items.push(item);
        }
    }
    *registry.lock().expect("tray registry poisoned") = alive;
    let _ = updates.try_send(items);
}

async fn handle_request(connection: &zbus::Connection, request: TrayRequest) {
    let (service, x, y, secondary) = match request {
        TrayRequest::Activate { service, x, y } => (service, x, y, false),
        TrayRequest::SecondaryActivate { service, x, y } => (service, x, y, true),
    };
    let builder = StatusNotifierItemProxy::builder(connection)
        .destination(service)
        .and_then(|builder| builder.path(ITEM_PATH));
    let proxy = match builder {
        Ok(builder) => builder.build().await,
        Err(err) => {
            log::debug!("tray item proxy setup failed: {err}");
            return;
        }
    };
    let proxy = match proxy {
        Ok(proxy) => proxy,
        Err(err) => {
            log::debug!("tray item proxy setup failed: {err}");
            return;
        }
    };
    // a failing activation usually means the item died; the next poll drops it
    let result = if secondary {
        proxy.secondary_activate(x, y).await
    } else {
        proxy.activate(x, y).await
    };
    if let Err(err) = result {
        log::debug!("tray item activation failed: {err}");
    }
}

/// Start the watcher daemon and hand back the state it feeds.
pub fn start(cx: &mut App) -> Entity<TrayState> {
    let (input_tx, input_rx) = unbounded::<TrayInput>();
    let (update_tx, update_rx) = unbounded::<Vec<TrayItem>>();

    let state = cx.new(|_| TrayState {
        items: Vec::new(),
        pixmap_rasters: HashMap::new(),
        input: input_tx.clone(),
    });

    cx.background_spawn(async move {
        if let Err(err) = run_tray(input_tx, input_rx, update_tx).await {
            log::error!("tray watcher terminated: {err:#}");
        }
    })
    .detach();

    let event_state = state.clone();
    cx.spawn(async move |cx| {
        while let Ok(items) = update_rx.recv().await {
            let _ = event_state.update(cx, |state, cx| {
                for icon in state.absorb_items(items) {
                    imaging::release_icon(&icon, cx);
                }
                cx.notify();
            });
        }
    })
    .detach();

    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_strings_map_to_tray_status() {
        assert_eq!(TrayItem::parse_status("Active"), TrayStatus::Active);
        assert_eq!(
            TrayItem::parse_status("NeedsAttention"),
            TrayStatus::NeedsAttention
        );
        assert_eq!(TrayItem::parse_status("Passive"), TrayStatus::Passive);
        assert_eq!(TrayItem::parse_status("bogus"), TrayStatus::Passive);
    }

    fn pixmap_item(service: &str, hash: u64, icon: Option<IconImage>) -> TrayItem {
        TrayItem {
            service: service.to_string(),
            title: service.to_string(),
            status: TrayStatus::Active,
            tooltip: String::new(),
            icon,
            pixmap_hash: hash,
            has_menu: false,
        }
    }

    fn render_image() -> Option<IconImage> {
        Some(IconImage::Raster(Arc::new(gpui::RenderImage::new(
            smallvec::smallvec![image::Frame::new(image::RgbaImage::new(1, 1))],
        ))))
    }

    fn pinned_raster(icon: &Option<IconImage>) -> Arc<gpui::RenderImage> {
        match icon {
            Some(IconImage::Raster(raster)) => raster.clone(),
            other => panic!("expected a raster, got {other:?}"),
        }
    }

    #[test]
    fn pixmap_rasters_pin_across_polls_and_report_droplets() {
        let mut state = TrayState {
            items: Vec::new(),
            pixmap_rasters: HashMap::new(),
            input: smol::channel::unbounded().0,
        };

        // poll 1: a pixmap item lands, its icon is pinned
        let first = render_image();
        let first_arc = pinned_raster(&first);
        let droplets = state.absorb_items(vec![pixmap_item(":1000", 7, first)]);
        assert!(droplets.is_empty(), "a first pin has nothing to release");
        assert_eq!(state.pixmap_rasters[":1000"].0, 7);

        // poll 2: the same pixmap hash keeps the pinned Arc serving the
        // item, even though the poll converted a fresh raster
        let fresh = render_image();
        let fresh_arc = pinned_raster(&fresh);
        assert!(!Arc::ptr_eq(&first_arc, &fresh_arc));
        let droplets = state.absorb_items(vec![pixmap_item(":1000", 7, fresh)]);
        assert!(droplets.is_empty(), "an unchanged pixmap releases nothing");
        let served = pinned_raster(&state.items[0].icon);
        assert!(
            Arc::ptr_eq(&served, &first_arc),
            "the item serves the pinned Arc, not the poll's fresh one"
        );

        // poll 3: the item died; its pin leaves with a droplet to release
        let droplets = state.absorb_items(Vec::new());
        assert_eq!(droplets.len(), 1);
        assert!(
            Arc::ptr_eq(&pinned_raster(&droplets[0]), &first_arc),
            "the droplet is the pinned Arc whose tile must drop"
        );
        assert!(state.pixmap_rasters.is_empty());
    }

    #[test]
    fn named_icons_bypass_the_pixmap_pin() {
        let mut state = TrayState {
            items: Vec::new(),
            pixmap_rasters: HashMap::new(),
            input: smol::channel::unbounded().0,
        };
        let mut item = pixmap_item(":1001", 0, render_image());
        item.icon = Some(IconImage::Svg(std::sync::Arc::from(
            &b"<svg/>"[..],
        )));
        let droplets = state.absorb_items(vec![item]);
        assert!(droplets.is_empty());
        assert!(state.pixmap_rasters.is_empty(), "hash 0 never pins");
        assert!(matches!(state.items[0].icon, Some(IconImage::Svg(_))));
    }
}
