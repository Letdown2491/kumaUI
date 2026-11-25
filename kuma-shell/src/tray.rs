//! The system tray: the shell hosts `org.kde.StatusNotifierWatcher` itself
//! (no external tray process), polls item properties, speaks DBusMenu for
//! the items that export one, and exposes [`TrayState`] for the bar.
//! Clicks ride a request channel back out to the dbus task; a menu open
//! rides the same channel out and the update channel back.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU32;

use futures_lite::StreamExt;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{App, AppContext, Entity};
use smol::channel::{Receiver, Sender, unbounded};
use zbus::object_server::SignalEmitter;

use crate::imaging::{self, IconImage};

const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_PATH: &str = "/StatusNotifierItem";
/// The tray heartbeat: item signals own the fast path; this slow
/// sweep catches silent items and reaps dead ones (their polls fail).
const HEARTBEAT: Duration = Duration::from_secs(60);
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
    /// The object path of the item's DBusMenu, when it exports one.
    pub menu_path: Option<String>,
    /// The item's `ItemIsMenu`: its primary action IS the menu (KDE's
    /// convention for menu-first applets like VPN switchers).
    pub is_menu: bool,
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
    /// Fetch the item's menu tree and land it in the state: the menu
    /// panel's open request. The session id is the state's open counter:
    /// a stale panel close (the old menu's, arriving after the new
    /// open) retires nothing.
    OpenMenu { service: String, session: u64 },
    /// A menu entry was clicked: Event + AboutToShow (a true answer
    /// refetches the tree; toggles repaint without closing).
    MenuEvent { service: String, id: i32 },
    /// The menu panel closed: stop the menu's signal watcher, but only
    /// if this is still the newest menu (a stale close from the panel
    /// a newer open replaced does nothing).
    MenuClosed { session: u64 },
}

/// dbus task → state: the snapshots, and the open menu's tree.
enum TrayUpdate {
    Items(Vec<TrayItem>),
    Menu(MenuSnapshot),
}

/// One DBusMenu entry: everything the panel renders. The full tree
/// arrives at open time (GetLayout with unlimited recursion), so
/// submenu navigation is state-side, no round trip.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuItem {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub separator: bool,
    pub toggle: Option<(ToggleType, ToggleState)>,
    pub icon_name: String,
    pub children: Vec<MenuItem>,
}

impl MenuItem {
    /// Whether clicking it enters a submenu.
    pub fn is_submenu(&self) -> bool {
        !self.children.is_empty()
    }

    /// The click verb the dbus task should speak: an enabled entry
    /// clicks; nothing else does.
    pub fn clickable(&self) -> bool {
        self.enabled && !self.separator && !self.is_submenu()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToggleType {
    #[default]
    None,
    Checkmark,
    Radio,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToggleState {
    #[default]
    Off,
    On,
    Unknown,
}

/// One item's menu: the tree, tagged with the service that owns it
/// (the panel's open state) and the layout revision (the AboutToShow
/// refetch dedupes by it, cosmetic today).
#[derive(Clone, Debug, PartialEq)]
pub struct MenuSnapshot {
    pub service: String,
    pub revision: u32,
    pub root: Vec<MenuItem>,
}

/// watcher object → dbus task (poll now), and state → dbus task, in one
/// channel so the task's loop selects on a single receiver.
enum TrayInput {
    Wake,
    Request(TrayRequest),
    /// A registry service's bus name lost its owner: the item is gone.
    Expired(String),
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
    /// The open menu's tree: None when no menu is open, or while the
    /// open request is in flight (the panel shows its loading state).
    menu: Option<MenuSnapshot>,
    /// The submenu path entered from the root, outermost first: menu
    /// navigation without a dbus round trip per level.
    menu_stack: Vec<i32>,
    /// True between an open request and its snapshot landing.
    menu_pending: bool,
    /// The open counter: the request session id that arbitrates a new
    /// open against the previous panel's stale close.
    menu_session: u64,
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

    /// Open an item's menu: forget any previous one (the panel is the
    /// singleton) and ask the dbus task for the tree. The panel opens
    /// immediately and renders the loading state until the snapshot
    /// lands.
    pub fn open_menu(&mut self, service: &str) {
        self.menu_session += 1;
        self.menu = None;
        self.menu_stack.clear();
        self.menu_pending = true;
        let _ = self
            .input
            .try_send(TrayInput::Request(TrayRequest::OpenMenu {
                service: service.to_string(),
                session: self.menu_session,
            }));
    }

    /// The menu panel closed: stop the dbus task's signal watcher for
    /// the newest session (a stale close does nothing).
    pub fn close_menu(&mut self) {
        let _ = self
            .input
            .try_send(TrayInput::Request(TrayRequest::MenuClosed {
                session: self.menu_session,
            }));
    }

    /// The service whose menu is open, if any.
    pub fn menu_service(&self) -> Option<&str> {
        self.menu.as_ref().map(|menu| menu.service.as_str())
    }

    /// Whether an open request is in flight.
    pub fn menu_loading(&self) -> bool {
        self.menu_pending
    }

    /// The entries at the current navigation level: the root, or the
    /// deepest entered submenu's children. A submenu id that a refetch
    /// dropped walks back up as far as it can (a stale stack degrades
    /// to the nearest surviving level, never an empty pane).
    pub fn menu_entries(&self) -> &[MenuItem] {
        let Some(menu) = &self.menu else {
            return &[];
        };
        let mut level: &[MenuItem] = &menu.root;
        for id in &self.menu_stack {
            match level.iter().find(|entry| entry.id == *id) {
                Some(entry) => level = &entry.children,
                None => break,
            }
        }
        level
    }

    /// How deep the navigation is: a nonzero depth earns the panel's
    /// back row.
    pub fn menu_depth(&self) -> usize {
        self.menu_stack.len()
    }

    /// Enter a submenu.
    pub fn menu_enter(&mut self, id: i32) {
        self.menu_stack.push(id);
    }

    /// Leave the deepest submenu. False at the root (the caller closes
    /// instead).
    pub fn menu_back(&mut self) -> bool {
        self.menu_stack.pop().is_some()
    }

    /// Click a menu entry: the dbus task speaks Event (and AboutToShow,
    /// so a toggle repaints). The menu stays open: the view closes it
    /// for leaves.
    pub fn menu_click(&mut self, id: i32) {
        let Some(service) = self.menu_service().map(str::to_owned) else {
            return;
        };
        let _ = self
            .input
            .try_send(TrayInput::Request(TrayRequest::MenuEvent { service, id }));
    }

    /// Swap in a fresh poll's items. Pixmap rasters dedupe by content
    /// hash: an unchanged pixmap keeps the pinned Arc (one atlas tile,
    /// however many polls repaint it). Returns the icons whose tiles must
    /// be released: a replaced pin, or an item that left. Pure list
    /// surgery, testable without a context; the caller does the dropping.
    /// `None` means the poll found nothing changed: the caller skips the
    /// repaint instead of feeding the two-second dbus metronome straight
    /// into the render.
    fn absorb_items(&mut self, items: Vec<TrayItem>) -> Option<Vec<Option<IconImage>>> {
        // same services in the same order, same visible fields, same
        // icon identity: named icons are registry Arcs (pointer-equal),
        // rasters are their content hashes
        let unchanged = self.items.len() == items.len()
            && self
                .items
                .iter()
                .zip(&items)
                .all(|(a, b)| a.same_snapshot(b));
        if unchanged {
            return None;
        }
        self.items = items;
        // an open menu whose item died renders the gone state; the
        // navigation is meaningless without its service
        if let Some(menu) = &self.menu
            && !self.items.iter().any(|item| item.service == menu.service)
        {
            self.menu = None;
            self.menu_stack.clear();
            self.menu_pending = false;
        }
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
        Some(droplets)
    }

    /// Land a fetched menu tree: the open request answers (the pending
    /// flag clears), the panel re-renders. An empty root is the fetch's
    /// failure shape and reads as the panel's gone state.
    fn absorb_menu(&mut self, snapshot: MenuSnapshot) {
        self.menu_pending = false;
        self.menu = Some(snapshot);
    }
}

impl TrayItem {
    /// Whether the snapshot would render identically: everything the
    /// tray shows, with the icon compared by identity rather than
    /// pixels (rasters hash their bytes in `pixmap_hash`).
    fn same_snapshot(&self, other: &TrayItem) -> bool {
        let same_icon = match (&self.icon, &other.icon) {
            // a raster's identity is its content hash: the poll
            // re-converts the bytes every time, so the Arcs never match
            (Some(IconImage::Raster(_)), Some(IconImage::Raster(_))) => {
                self.pixmap_hash == other.pixmap_hash && self.pixmap_hash != 0
            }
            // svg and named icons ride the registry's stable Arcs
            (Some(IconImage::Svg(a)), Some(IconImage::Svg(b))) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        self.service == other.service
            && self.title == other.title
            && self.status == other.status
            && self.tooltip == other.tooltip
            && self.menu_path == other.menu_path
            && self.is_menu == other.is_menu
            && self.pixmap_hash == other.pixmap_hash
            && same_icon
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
    fn item_is_menu(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn menu(&self) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
}

/// Poll one item into a snapshot. A dbus failure means the item is gone:
/// `Ok(None)` drops it.
async fn poll_item(
    connection: &zbus::Connection,
    service: &str,
    previous: Option<&TrayItem>,
) -> Option<TrayItem> {
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
    // the liveness probe's answer doubles as the status: an untitled
    // item pays one round trip for both, not two
    let status_answer = proxy.status().await;
    if title.is_empty() && status_answer.is_err() {
        return None; // unresponsive and untitled: dead
    }
    let status = status_answer
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
    let menu_path = proxy
        .menu()
        .await
        .ok()
        .map(|path| path.to_string())
        .filter(|path| path != "/");
    let is_menu = if menu_path.is_some() {
        proxy.item_is_menu().await.unwrap_or(false)
    } else {
        false
    };

    let tooltip = if tooltip.is_empty() {
        title.clone()
    } else {
        tooltip
    };

    let wanted_name = if status == TrayStatus::NeedsAttention && !attention_icon_name.is_empty() {
        &attention_icon_name
    } else {
        &icon_name
    };

    // named icons resolve cheap (a registry Arc clone), so only the
    // raster conversion is worth skipping: when the pixmap bytes hash
    // to what the last snapshot already converted, reuse its tile
    let named = if wanted_name.is_empty() {
        None
    } else {
        imaging::resolve(wanted_name, ICON_RESOLVE_SIZE).map(|icon| icon.clone_shared())
    };
    let (icon, pixmap_hash) = match named {
        Some(icon) => (Some(icon), 0),
        None => match previous
            .filter(|previous| {
                previous.status == status
                    && previous.pixmap_hash != 0
                    && previous.pixmap_hash == pixmap_hash(&icon_pixmap)
                    && matches!(previous.icon, Some(IconImage::Raster(_)))
            })
            .map(|previous| (previous.icon.clone(), previous.pixmap_hash))
        {
            Some(reused) => reused,
            None => {
                let (icon, pixmap_hash) = resolve_icon(wanted_name, icon_pixmap);
                (icon, pixmap_hash)
            }
        },
    };
    Some(TrayItem {
        service: service.to_string(),
        title,
        status,
        tooltip,
        icon,
        pixmap_hash,
        menu_path,
        is_menu,
    })
}

/// The pixmap's content hash: identity for the conversion skip and
/// the state side's raster pin. Zero means no pixmap at all.
fn pixmap_hash(pixmap: &Option<(i32, i32, Vec<u8>)>) -> u64 {
    match pixmap {
        Some((width, height, data)) => {
            let mut hash = std::hash::DefaultHasher::new();
            (width, height, data).hash(&mut hash);
            hash.finish()
        }
        None => 0,
    }
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
    let hash = pixmap_hash(&pixmap);
    (
        pixmap.and_then(|(width, height, data)| {
            kuma_platform::imaging::argb32_to_render_image(width, height, data)
                .map(Arc::new)
                .map(IconImage::Raster)
        }),
        hash,
    )
}

/// The dbus task: serve the watcher, poll items on the monitor cadence (and
/// immediately on registration), speak DBusMenu for open-menu requests,
/// and run click requests out to items.
async fn run_tray(
    input_tx: Sender<TrayInput>,
    input_rx: Receiver<TrayInput>,
    updates: Sender<TrayUpdate>,
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

    // the last snapshot per service, and which services have a signal
    // watcher running
    let items: Arc<Mutex<HashMap<String, TrayItem>>> = Arc::new(Mutex::new(HashMap::new()));
    let watched: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    // the open menu's generation: every open (and close) retires the
    // previous menu's signal watcher
    let menu_gen: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // the newest menu session the task answered: a stale panel close
    // (older session) retires nothing
    let mut menu_session = 0u64;

    // bus departures reap items as they happen: a tray process exiting
    // shows up here as its name losing its owner, not at the heartbeat
    let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
    let mut owners = dbus.receive_name_owner_changed().await?;

    loop {
        // input (wake, request, or a bus departure), or the heartbeat;
        // None is the heartbeat's sentinel
        let input = smol::future::or(
            async { input_rx.recv().await.ok() },
            async {
                loop {
                    let event = owners.next().await?;
                    let args = event.args().ok()?;
                    // an empty new owner is a departure; anything else
                    // is a name changing hands, which the tray does
                    // not care about
                    let gone = {
                        let new_owner: Option<zbus::names::UniqueName> = args.new_owner.into();
                        new_owner
                            .map(|owner| owner.as_str().is_empty())
                            .unwrap_or(true)
                    };
                    if gone {
                        return Some(TrayInput::Expired(args.name.to_string()));
                    }
                }
            },
        );
        match smol::future::or(input, async {
            smol::Timer::after(HEARTBEAT).await;
            None
        })
        .await
        {
            Some(TrayInput::Wake) => {
                reconcile(&connection, &registry, &items, &watched, &updates);
            }
            Some(TrayInput::Request(request)) => {
                handle_request(&connection, &items, &menu_gen, &mut menu_session, request, &updates)
                    .await;
            }
            Some(TrayInput::Expired(service)) => {
                let registered = registry
                    .lock()
                    .expect("tray registry poisoned")
                    .iter()
                    .any(|row| row == &service);
                if registered {
                    forget_item(&service, &items, &registry, &watched, &updates);
                }
            }
            None => poll_all(&connection, &registry, &items, &watched, &updates).await,
        }
    }
}

/// Spawn a signal watcher for every registered service without one;
/// the watcher interface wakes this on registration.
fn reconcile(
    connection: &zbus::Connection,
    registry: &Arc<Mutex<Vec<String>>>,
    items: &Arc<Mutex<HashMap<String, TrayItem>>>,
    watched: &Arc<Mutex<HashSet<String>>>,
    updates: &Sender<TrayUpdate>,
) {
    let services = registry.lock().expect("tray registry poisoned").clone();
    for service in services {
        let fresh = watched
            .lock()
            .expect("tray watched poisoned")
            .insert(service.clone());
        if fresh {
                let task = watch_item(
                connection.clone(),
                service,
                items.clone(),
                registry.clone(),
                watched.clone(),
                updates.clone(),
            );
            // detach: a dropped zbus Task cancels its future, and the
            // dropped handle here would cancel the watcher before its
            // first poll
            connection.executor().spawn(task, "tray item").detach();
        }
    }
}

/// One item's watcher: a first read lands the snapshot the moment the
/// item registers, then the item's own signals re-poll it (the SNI
/// update signals and the generic PropertiesChanged both count). A
/// dead poll reaps the item and ends the watcher; a silent item waits
/// for the heartbeat.
async fn watch_item(
    connection: zbus::Connection,
    service: String,
    items: Arc<Mutex<HashMap<String, TrayItem>>>,
    registry: Arc<Mutex<Vec<String>>>,
    watched: Arc<Mutex<HashSet<String>>>,
    updates: Sender<TrayUpdate>,
) {
    if !repoll(&connection, &service, &items, &registry, &updates).await {
        forget_item(&service, &items, &registry, &watched, &updates);
        return;
    }
    let proxy = match item_proxy(&connection, &service).await {
        Ok(proxy) => proxy,
        // no one to listen to: the heartbeat owns this item now
        Err(_) => return,
    };
    let Ok(mut signals) = proxy.receive_all_signals().await else {
        return;
    };
    while let Some(msg) = signals.next().await {
        let header = msg.header();
        let Some(member) = header.member() else {
            continue;
        };
        if !matches!(
            member.as_str(),
            "PropertiesChanged" | "NewTitle" | "NewIcon" | "NewAttentionIcon" | "NewToolTip"
                | "NewStatus"
        ) {
            continue;
        }
        if !repoll(&connection, &service, &items, &registry, &updates).await {
            forget_item(&service, &items, &registry, &watched, &updates);
            return;
        }
    }
    // the signal stream ended without a verdict: retire the watcher
    // and let the heartbeat re-poll until the item proves itself
    watched.lock().expect("tray watched poisoned").remove(&service);
}

/// The untyped signal source for one item: caching off, the same seam
/// the nm watcher uses, since reads happen live on the event.
async fn item_proxy(connection: &zbus::Connection, service: &str) -> zbus::Result<zbus::Proxy<'static>> {
    zbus::proxy::Builder::<zbus::Proxy>::new(connection)
        .destination(service.to_string())?
        .path(ITEM_PATH.to_string())?
        .interface("org.kde.StatusNotifierItem".to_string())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

/// Re-read one item and land it if it moved. False means the item is
/// gone.
async fn repoll(
    connection: &zbus::Connection,
    service: &str,
    items: &Arc<Mutex<HashMap<String, TrayItem>>>,
    registry: &Arc<Mutex<Vec<String>>>,
    updates: &Sender<TrayUpdate>,
) -> bool {
    let previous = items
        .lock()
        .expect("tray items poisoned")
        .get(service)
        .cloned();
    let Some(item) = poll_item(connection, service, previous.as_ref()).await else {
        return false;
    };
    let moved = {
        let mut all = items.lock().expect("tray items poisoned");
        match all.get(service) {
            Some(last) if last.same_snapshot(&item) => false,
            _ => {
                all.insert(service.to_string(), item);
                true
            }
        }
    };
    if moved {
        publish(registry, items, updates);
    }
    true
}

/// A dead item: drop its snapshot and registry row, publish, retire
/// the watcher.
fn forget_item(
    service: &str,
    items: &Arc<Mutex<HashMap<String, TrayItem>>>,
    registry: &Arc<Mutex<Vec<String>>>,
    watched: &Arc<Mutex<HashSet<String>>>,
    updates: &Sender<TrayUpdate>,
) {
    items.lock().expect("tray items poisoned").remove(service);
    registry
        .lock()
        .expect("tray registry poisoned")
        .retain(|row| row != service);
    watched
        .lock()
        .expect("tray watched poisoned")
        .remove(service);
    publish(registry, items, updates);
}

/// The heartbeat sweep: re-poll every registered item (silent items
/// and missed signals land here), dropping the dead.
async fn poll_all(
    connection: &zbus::Connection,
    registry: &Arc<Mutex<Vec<String>>>,
    items: &Arc<Mutex<HashMap<String, TrayItem>>>,
    watched: &Arc<Mutex<HashSet<String>>>,
    updates: &Sender<TrayUpdate>,
) {
    let services = registry.lock().expect("tray registry poisoned").clone();
    for service in services {
        if !repoll(connection, &service, items, registry, updates).await {
            forget_item(&service, items, registry, watched, updates);
        }
    }
}

/// Publish the snapshot vec in registration order.
fn publish(
    registry: &Arc<Mutex<Vec<String>>>,
    items: &Arc<Mutex<HashMap<String, TrayItem>>>,
    updates: &Sender<TrayUpdate>,
) {
    let snapshots = items.lock().expect("tray items poisoned");
    let rows = registry.lock().expect("tray registry poisoned").clone();
    let published: Vec<TrayItem> = rows
        .iter()
        .filter_map(|service| snapshots.get(service).cloned())
        .collect();
    let _ = updates.try_send(TrayUpdate::Items(published));
}

async fn handle_request(
    connection: &zbus::Connection,
    items: &Arc<Mutex<HashMap<String, TrayItem>>>,
    menu_gen: &Arc<AtomicU32>,
    menu_session: &mut u64,
    request: TrayRequest,
    updates: &Sender<TrayUpdate>,
) {
    match request {
        TrayRequest::Activate { service, x, y } => {
            activate_item(connection, &service, x, y, false).await
        }
        TrayRequest::SecondaryActivate { service, x, y } => {
            activate_item(connection, &service, x, y, true).await
        }
        TrayRequest::OpenMenu { service, session } => {
            let menu_path = items
                .lock()
                .expect("tray items poisoned")
                .get(&service)
                .and_then(|item| item.menu_path.clone());
            // every open retires the previous watcher, including a
            // failed fetch's (the generation moves either way)
            *menu_session = session;            let generation = menu_gen.load(std::sync::atomic::Ordering::SeqCst) + 1;
            menu_gen.store(generation, std::sync::atomic::Ordering::SeqCst);
            let Some(menu_path) = menu_path else {
                // no menu behind the request: an empty tree lands the
                // panel's gone state (the pending flag clears either way)
                let _ = updates.try_send(TrayUpdate::Menu(MenuSnapshot {
                    service,
                    revision: 0,
                    root: Vec::new(),
                }));
                return;
            };
            let snapshot = fetch_layout(connection, &service, &menu_path, 0).await;
            let snapshot = match snapshot {
                Some(snapshot) => snapshot,
                None => MenuSnapshot {
                    service: service.clone(),
                    revision: 0,
                    root: Vec::new(),
                },
            };
            let _ = updates.try_send(TrayUpdate::Menu(snapshot));
            let task = watch_menu(
                connection.clone(),
                service,
                menu_path,
                generation,
                menu_gen.clone(),
                updates.clone(),
            );
            connection.executor().spawn(task, "tray menu").detach();
        }
        TrayRequest::MenuEvent { service, id } => {
            let Some(menu_path) = items
                .lock()
                .expect("tray items poisoned")
                .get(&service)
                .and_then(|item| item.menu_path.clone())
            else {
                return;
            };
            // the click itself, then the changed-under-us probe: a true
            // AboutToShow refetches the tree so toggles repaint
            let _ = connection
                .call_method(
                    Some(service.as_str()),
                    menu_path.as_str(),
                    Some("com.canonical.dbusmenu"),
                    "Event",
                    &(id, "clicked", zbus::zvariant::Value::from(""), 0u32),
                )
                .await;
            let changed: Option<bool> = connection
                .call_method(
                    Some(service.as_str()),
                    menu_path.as_str(),
                    Some("com.canonical.dbusmenu"),
                    "AboutToShow",
                    &id,
                )
                .await
                .ok()
                .and_then(|reply| reply.body().deserialize().ok());
            if changed == Some(true)
                && let Some(snapshot) = fetch_layout(connection, &service, &menu_path, 0).await
            {
                let _ = updates.try_send(TrayUpdate::Menu(snapshot));
            }
        }
        TrayRequest::MenuClosed { session } => {
            if session == *menu_session {
                menu_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

async fn activate_item(connection: &zbus::Connection, service: &str, x: i32, y: i32, secondary: bool) {
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

/// The menu's untyped proxy: GetLayout's reply is a recursive struct the
/// typed macros cannot name (children ride an `av` tail), so the calls
/// are raw and the parse is hand-rolled against zvariant values.
async fn menu_proxy(
    connection: &zbus::Connection,
    service: &str,
    path: &str,
) -> zbus::Result<zbus::Proxy<'static>> {
    zbus::proxy::Builder::<zbus::Proxy>::new(connection)
        .destination(service.to_string())?
        .path(path.to_string())?
        .interface("com.canonical.dbusmenu".to_string())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

/// Fetch one item's menu tree: GetLayout with unlimited recursion, the
/// whole tree in one round trip so submenu navigation is state-side.
/// A fetch failure answers None.
async fn fetch_layout(
    connection: &zbus::Connection,
    service: &str,
    path: &str,
    parent: i32,
) -> Option<MenuSnapshot> {
    let reply = connection
        .call_method(
            Some(service),
            path,
            Some("com.canonical.dbusmenu"),
            "GetLayout",
            &(parent, -1i32, Vec::<String>::new()),
        )
        .await
        .ok()?;
    let (revision, layout): (u32, zbus::zvariant::OwnedValue) =
        reply.body().deserialize().ok()?;
    let root = parse_menu_layout(&zbus::zvariant::Value::from(layout))?;
    Some(MenuSnapshot {
        service: service.to_string(),
        revision,
        root: root.children,
    })
}

/// The menu's signal watcher: while this generation of the menu is open,
/// the item's LayoutUpdated and ItemsPropertiesUpdated signals refetch
/// the tree (the same ride the icon updates have). The generation guard
/// retires the watcher when the menu closes or another opens.
async fn watch_menu(
    connection: zbus::Connection,
    service: String,
    path: String,
    generation: u32,
    menu_gen: Arc<AtomicU32>,
    updates: Sender<TrayUpdate>,
) {
    let proxy = match menu_proxy(&connection, &service, &path).await {
        Ok(proxy) => proxy,
        Err(_) => return,
    };
    let Ok(mut signals) = proxy.receive_all_signals().await else {
        return;
    };
    while menu_gen.load(std::sync::atomic::Ordering::SeqCst) == generation {
        let Some(msg) = signals.next().await else {
            break;
        };
        let member = msg.header().member().map(|m| m.to_string());
        if !matches!(
            member.as_deref(),
            Some("LayoutUpdated" | "ItemsPropertiesUpdated")
        ) {
            continue;
        }
        match fetch_layout(&connection, &service, &path, 0).await {
            Some(snapshot) => {
                if menu_gen.load(std::sync::atomic::Ordering::SeqCst) == generation {
                    let _ = updates.try_send(TrayUpdate::Menu(snapshot));
                }
            }
            None => break,
        }
    }
}

/// One layout node: `(id, properties, children)` where the children ride
/// an array-of-variants tail (the protocol's own introspection
/// signature, `u(ia{sv}av)`); the original spec form put each child
/// struct in the parent's tail directly. Both shapes parse.
fn parse_menu_layout(value: &zbus::zvariant::Value) -> Option<MenuItem> {
    use zbus::zvariant::Value;
    let Value::Structure(structure) = value else {
        return None;
    };
    let mut fields = structure.fields().iter();
    let id = match fields.next()? {
        Value::I32(id) => *id,
        _ => return None,
    };
    let props = fields.next()?;
    let mut item = MenuItem {
        id,
        // the spec's default: an entry is enabled and visible
        enabled: true,
        visible: true,
        ..Default::default()
    };
    if let Value::Dict(dict) = props {
        for (key, value) in dict.iter() {
            let Value::Str(key) = key else { continue };
            // a{sv}: the entry's value rides a variant
            let value = match value {
                Value::Value(inner) => inner,
                other => other,
            };
            match key.as_str() {
                "label" => {
                    if let Value::Str(label) = value {
                        item.label = strip_mnemonic(label.as_str());
                    }
                }
                "enabled" => {
                    if let Value::Bool(enabled) = value {
                        item.enabled = *enabled;
                    }
                }
                "visible" => {
                    if let Value::Bool(visible) = value {
                        item.visible = *visible;
                    }
                }
                "type" => {
                    if let Value::Str(kind) = value {
                        item.separator = kind.as_str() == "separator";
                    }
                }
                "toggle-type" => {
                    if let Value::Str(kind) = value {
                        // sorted-key order means either arm can land
                        // first; each preserves the other's half
                        let state = item.toggle.map(|(_, state)| state).unwrap_or_default();
                        item.toggle = Some(match kind.as_str() {
                            "checkmark" => (ToggleType::Checkmark, state),
                            "radio" => (ToggleType::Radio, state),
                            _ => (ToggleType::None, state),
                        });
                    }
                }
                "toggle-state" => {
                    if let Value::I32(state) = value {
                        let state = match state {
                            1 => ToggleState::On,
                            -1 => ToggleState::Unknown,
                            _ => ToggleState::Off,
                        };
                        let kind = item.toggle.map(|(kind, _)| kind).unwrap_or_default();
                        item.toggle = Some((kind, state));
                    }
                }
                "icon-name" => {
                    if let Value::Str(icon) = value {
                        item.icon_name = icon.as_str().to_string();
                    }
                }
                // "children-display" is a marker: the children themselves
                // arrive as the struct's tail
                _ => {}
            }
        }
    }
    // children: either one `av` field, or the spec's trailing child
    // structs; a bare variant wraps a child too
    for field in fields {
        match field {
            Value::Array(array) => {
                for child in array.iter() {
                    if let Value::Value(inner) = child
                        && let Some(entry) = parse_menu_layout(inner)
                    {
                        item.children.push(entry);
                    }
                }
            }
            Value::Value(inner) => {
                if let Some(entry) = parse_menu_layout(inner) {
                    item.children.push(entry);
                }
            }
            Value::Structure(_) => {
                if let Some(entry) = parse_menu_layout(field) {
                    item.children.push(entry);
                }
            }
            _ => {}
        }
    }
    Some(item)
}

/// The label's display text: DBusMenu labels mark mnemonics with an
/// underscore (a literal one doubled); the panel shows neither.
fn strip_mnemonic(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars();
    while let Some(c) = chars.next() {
        match c {
            '_' => match chars.next() {
                Some('_') => out.push('_'),
                Some(next) => out.push(next),
                None => {}
            },
            other => out.push(other),
        }
    }
    out
}

/// Start the watcher daemon and hand back the state it feeds.
pub fn start(cx: &mut App) -> Entity<TrayState> {
    let (input_tx, input_rx) = unbounded::<TrayInput>();
    let (update_tx, update_rx) = unbounded::<TrayUpdate>();

    let state = cx.new(|_| TrayState {
        items: Vec::new(),
        pixmap_rasters: HashMap::new(),
        menu: None,
        menu_stack: Vec::new(),
        menu_pending: false,
        menu_session: 0,
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
        while let Ok(update) = update_rx.recv().await {
            match update {
                TrayUpdate::Items(items) => {
                    let _ = event_state.update(cx, |state, cx| {
                        if let Some(droplets) = state.absorb_items(items) {
                            for icon in droplets {
                                imaging::release_icon(&icon, cx);
                            }
                            cx.notify();
                        }
                    });
                }
                TrayUpdate::Menu(snapshot) => {
                    let _ = event_state.update(cx, |state, cx| {
                        state.absorb_menu(snapshot);
                        cx.notify();
                    });
                }
            }
        }
    })
    .detach();

    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Dict, Signature, StructureBuilder, Value};

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
            menu_path: None,
            is_menu: false,
        }
    }

    // --- DBusMenu parse fixtures ---

    /// A layout node as the wire carries it: (id, props, children-av),
    /// each prop value and each child riding a variant.
    fn menu_node(id: i32, props: Vec<(&'static str, Value<'static>)>, children: Vec<Value<'static>>) -> Value<'static> {
        let mut dict = Dict::new(&Signature::Str, &Signature::Variant);
        for (key, value) in props {
            dict.append(Value::from(key), Value::Value(Box::new(value)))
                .unwrap();
        }
        let mut tail = Array::new(&Signature::Variant);
        for child in children {
            tail.append(Value::Value(Box::new(child))).unwrap();
        }
        Value::from(
            StructureBuilder::new()
                .append_field(Value::from(id))
                .append_field(Value::from(dict))
                .append_field(Value::from(tail))
                .build()
                .unwrap(),
        )
    }

    fn standard(id: i32, label: &str) -> Value<'static> {
        menu_node(id, vec![("label", Value::from(label.to_string()))], Vec::new())
    }

    /// The live noscat menu's shape, busctl-observed (2026-11-24): root
    /// 0 is a submenu carrying "Show noscat", a separator, and "Quit".
    fn noscat_root() -> Value<'static> {
        menu_node(
            0,
            vec![("children-display", Value::from("submenu"))],
            vec![
                standard(1, "Show noscat"),
                menu_node(2, vec![("type", Value::from("separator"))], Vec::new()),
                standard(3, "Quit"),
            ],
        )
    }

    #[test]
    fn the_live_menu_shape_parses() {
        let root = parse_menu_layout(&noscat_root()).expect("the busctl-observed shape parses");
        assert_eq!(root.id, 0);
        assert!(root.is_submenu());
        assert_eq!(root.children.len(), 3);
        let (show, separator, quit) =
            (root.children.first().unwrap(), &root.children[1], root.children.last().unwrap());
        assert_eq!(show.label, "Show noscat");
        assert!(show.enabled && show.visible && show.clickable());
        assert!(separator.separator);
        assert!(!separator.clickable());
        assert_eq!(quit.label, "Quit");
        assert!(quit.clickable());
    }

    #[test]
    fn toggles_and_submenus_parse_with_their_marks() {
        let root = menu_node(
            0,
            vec![("children-display", Value::from("submenu"))],
            vec![
                menu_node(
                    1,
                    vec![
                        ("label", Value::from("Wi-Fi")),
                        ("toggle-type", Value::from("checkmark")),
                        ("toggle-state", Value::from(1i32)),
                    ],
                    Vec::new(),
                ),
                menu_node(
                    2,
                    vec![
                        ("label", Value::from("Networks")),
                        ("children-display", Value::from("submenu")),
                    ],
                    vec![standard(3, "kuma-net")],
                ),
                menu_node(
                    4,
                    vec![("label", Value::from("Ghost")),
                        ("enabled", Value::from(false))],
                    Vec::new(),
                ),
            ],
        );
        let parsed = parse_menu_layout(&root).unwrap();
        let (wifi, networks, ghost) = (&parsed.children[0], &parsed.children[1], &parsed.children[2]);
        assert_eq!(wifi.toggle, Some((ToggleType::Checkmark, ToggleState::On)));
        assert!(networks.is_submenu() && !networks.clickable());
        assert_eq!(networks.children[0].label, "kuma-net");
        assert!(!ghost.enabled && !ghost.clickable(), "a disabled entry cannot click");
    }

    #[test]
    fn the_spec_trailing_struct_children_parse_too() {
        // the original spec's shape: each child struct rides the parent's
        // tail directly, no av wrapper
        let mut dict = Dict::new(&Signature::Str, &Signature::Variant);
        dict.append(Value::from("label"), Value::Value(Box::new(Value::from("Quit"))))
            .unwrap();
        let child = Value::from(
            StructureBuilder::new()
                .append_field(Value::from(1i32))
                .append_field(Value::from(dict))
                .build()
                .unwrap(),
        );
        let root = Value::from(
            StructureBuilder::new()
                .append_field(Value::from(0i32))
                .append_field(Value::from(Dict::new(&Signature::Str, &Signature::Variant)))
                .append_field(child)
                .build()
                .unwrap(),
        );
        let parsed = parse_menu_layout(&root).unwrap();
        assert_eq!(parsed.children[0].label, "Quit");
    }

    #[test]
    fn labels_strip_mnemonics_and_keep_literal_underscores() {
        assert_eq!(strip_mnemonic("_Quit"), "Quit");
        assert_eq!(strip_mnemonic("Show _noscat"), "Show noscat");
        assert_eq!(strip_mnemonic("file__name"), "file_name", "a doubled underscore is literal");
        assert_eq!(strip_mnemonic("trailing_"), "trailing");
    }

    #[test]
    fn invisible_entries_parse_but_mark_themselves() {
        let root = menu_node(
            0,
            vec![("children-display", Value::from("submenu"))],
            vec![menu_node(
                1,
                vec![("label", Value::from("hidden")), ("visible", Value::from(false))],
                Vec::new(),
            )],
        );
        let parsed = parse_menu_layout(&root).unwrap();
        assert!(!parsed.children[0].visible);
    }

    // --- state-side navigation and open lifecycle ---

    fn menu_state() -> TrayState {
        TrayState {
            items: Vec::new(),
            pixmap_rasters: HashMap::new(),
            menu: None,
            menu_stack: Vec::new(),
            menu_pending: false,
            menu_session: 0,
            input: smol::channel::unbounded().0,
        }
    }

    fn noscat_snapshot() -> MenuSnapshot {
        let root = parse_menu_layout(&noscat_root()).unwrap();
        MenuSnapshot {
            service: "noscat".into(),
            revision: 7,
            root: root.children,
        }
    }

    #[test]
    fn open_menu_is_pending_until_the_snapshot_lands() {
        let mut state = menu_state();
        state.open_menu("noscat");
        assert!(state.menu_loading() && state.menu.is_none(), "the panel renders loading");
        state.absorb_menu(noscat_snapshot());
        assert!(!state.menu_loading());
        assert_eq!(state.menu_service(), Some("noscat"));
        assert_eq!(state.menu_entries().len(), 3);
    }

    #[test]
    fn submenu_navigation_pushes_and_pops() {
        let mut state = menu_state();
        state.absorb_menu(noscat_snapshot());
        // the live tree's root carries no submenus; build one to enter
        state.menu_enter(1);
        assert_eq!(state.menu_depth(), 1);
        assert_eq!(state.menu_entries().len(), 0, "Show noscat is a leaf");
        assert!(state.menu_back());
        assert_eq!(state.menu_depth(), 0);
        assert_eq!(state.menu_entries().len(), 3);
        assert!(!state.menu_back(), "the root's back answer is false: the caller closes");
    }

    #[test]
    fn a_stale_stack_degrades_to_the_nearest_level() {
        let mut state = menu_state();
        state.absorb_menu(noscat_snapshot());
        // a refetch replaced the tree and the entered id vanished: the
        // entries walk back up to the nearest surviving level instead
        // of panicking on the gap
        state.menu_enter(99);
        assert_eq!(state.menu_depth(), 1);
        assert_eq!(state.menu_entries().len(), 3, "id 99 is gone: the root serves");
        // the whole tree empty (a failed refetch): empty at any depth
        state.absorb_menu(MenuSnapshot {
            service: "noscat".into(),
            revision: 8,
            root: vec![],
        });
        assert_eq!(state.menu_entries().len(), 0);
    }

    #[test]
    fn a_dead_menu_item_drops_the_open_menu() {
        let mut state = menu_state();
        state.absorb_menu(noscat_snapshot());
        state.items.push(TrayItem {
            service: "noscat".into(),
            title: "noscat".into(),
            status: TrayStatus::Active,
            tooltip: String::new(),
            icon: None,
            pixmap_hash: 0,
            menu_path: Some("/MenuBar".into()),
            is_menu: false,
        });
        // the item leaves: the menu's navigation is meaningless
        state.absorb_items(Vec::new()).unwrap();
        assert!(state.menu.is_none() && !state.menu_loading());
        assert!(state.menu_entries().is_empty());
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
        let mut state = menu_state();

        // poll 1: a pixmap item lands, its icon is pinned
        let first = render_image();
        let first_arc = pinned_raster(&first);
        let droplets = state
            .absorb_items(vec![pixmap_item(":1000", 7, first)])
            .expect("a first poll is always a change");
        assert!(droplets.is_empty(), "a first pin has nothing to release");
        assert_eq!(state.pixmap_rasters[":1000"].0, 7);

        // poll 2: the same pixmap hash keeps the pinned Arc serving the
        // item, even though the poll converted a fresh raster. The
        // title changes so the snapshot is a change (an unchanged poll
        // short-circuits before the pin logic); the pin must still hold
        let fresh = render_image();
        let fresh_arc = pinned_raster(&fresh);
        assert!(!Arc::ptr_eq(&first_arc, &fresh_arc));
        let mut polled = pixmap_item(":1000", 7, fresh);
        polled.title = "renamed".into();
        let droplets = state
            .absorb_items(vec![polled])
            .expect("a title change is a change");
        assert!(droplets.is_empty(), "an unchanged pixmap releases nothing");
        let served = pinned_raster(&state.items[0].icon);
        assert!(
            Arc::ptr_eq(&served, &first_arc),
            "the item serves the pinned Arc, not the poll's fresh one"
        );
        assert_eq!(state.items[0].title, "renamed");

        // poll 3: the item died; its pin leaves with a droplet to release
        let droplets = state
            .absorb_items(Vec::new())
            .expect("an item leaving is a change");
        assert_eq!(droplets.len(), 1);
        assert!(
            Arc::ptr_eq(&pinned_raster(&droplets[0]), &first_arc),
            "the droplet is the pinned Arc whose tile must drop"
        );
        assert!(state.pixmap_rasters.is_empty());
    }

    #[test]
    fn an_unchanged_poll_reports_no_change() {
        let mut state = menu_state();
        let first = render_image();
        let item = pixmap_item(":1000", 7, first);
        state.absorb_items(vec![item.clone()]).unwrap();
        // the poll rebuilt the item from dbus: identical fields, fresh
        // raster — the state must answer None, not repaint the tray
        let again = pixmap_item(":1000", 7, render_image());
        assert!(state.absorb_items(vec![again]).is_none());
    }

    #[test]
    fn named_icons_bypass_the_pixmap_pin() {
        let mut state = menu_state();
        let mut item = pixmap_item(":1001", 0, render_image());
        item.icon = Some(IconImage::Svg(std::sync::Arc::from(
            &b"<svg/>"[..],
        )));
        let droplets = state
            .absorb_items(vec![item])
            .expect("a first poll is always a change");
        assert!(droplets.is_empty());
        assert!(state.pixmap_rasters.is_empty(), "hash 0 never pins");
        assert!(matches!(state.items[0].icon, Some(IconImage::Svg(_))));
    }
}
