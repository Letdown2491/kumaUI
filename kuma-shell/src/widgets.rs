//! User-made widgets: the registry half. A widget is a directory holding a
//! `widget.toml` manifest plus whatever scripts and assets it names, so the
//! whole widget travels as one unit and activation is a file operation.
//! The runner, the bar arm, and the settings section consume what this
//! module discovers; nothing here touches gpui.
//!
//! The issue's one rule per concern lives here as the discovery half:
//! file presence in a widgets directory is all it takes to be an available
//! widget. Enabling stays a settings placement, decided elsewhere.
//!
//! The stdout contract the runner parses is the Waybar custom-module
//! convention: plain text is a label, and the JSON object
//! `{"text": ..., "tooltip": ..., "class": ...}` is the upgrade path.
//! Scripts written to feed a Waybar custom module render here unchanged;
//! CSS classes have no meaning in this bar and are ignored.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Context, Entity};
use notify::{Watcher as _, RecursiveMode};
use std::process::Command;
use log::{debug, error, warn};
use serde::Deserialize;

use crate::launcher::reap;
use crate::settings::WidgetKind;

/// The shortest tick a widget may ask for. A script that polls faster than
/// the bar can matter is a busy loop with a stderr; the clamp is the
/// issue's "interval clamped from below".
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// The tick when the manifest names none. Long enough that a stray
/// `while true; do curl; done` widget cannot matter, short enough that a
/// forgotten interval still feels live.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

/// A discovered, validated user-made widget. Identity is the directory
/// stem: a user widget overrides a system one by stem, and a placement
/// names this id in the config.
#[derive(Clone, Debug, PartialEq)]
pub struct CustomWidget {
    /// The directory stem, the string placements carry.
    pub id: String,
    /// The bar's tooltip and the settings rows' name. Defaults to the id.
    pub label: String,
    /// A themed icon name (resolved through the shell's asset registry at
    /// render) or a path relative to the widget directory (rendered from
    /// the file's bytes). The disambiguation is extension-shaped: a name
    /// ending in a known image extension is a file, anything else is themed.
    pub icon: Option<String>,
    /// How often the source runs. Clamped from below at MIN_INTERVAL.
    pub interval: Duration,
    /// A shell line spawned (fire and forget, bounded) when the widget
    /// is clicked. Empty stdout still renders; a missing source is the
    /// runner's problem at tick time.
    pub click: Option<String>,
    /// What runs on the interval.
    pub run: WidgetRun,
    /// The widget directory, source and icon paths resolve against it.
    pub dir: PathBuf,
}

/// What a widget executes on its interval. `source` is a file in the
/// widget directory, executed directly (its shebang applies; it is not
/// reinterpreted by a shell). `command` is a shell line run as-is, the
/// degenerate one-file widget: `command = "echo hello"` is complete.
#[derive(Clone, Debug, PartialEq)]
pub enum WidgetRun {
    Script(PathBuf),
    Command(String),
}

/// The manifest on disk. Field names are the issue's; `id` is checked
/// against the directory stem rather than trusted, so a renamed directory
/// cannot fork a widget's identity.
#[derive(Debug, Deserialize)]
struct WidgetManifest {
    id: Option<String>,
    label: Option<String>,
    icon: Option<String>,
    /// Seconds; serde's f64 visitor takes integers too.
    interval: Option<f64>,
    source: Option<String>,
    command: Option<String>,
    click: Option<String>,
}

/// Image extensions that make an `icon` a file path instead of a themed
/// name. Kept lowercase; the comparison is too.
const ICON_EXTENSIONS: [&str; 4] = ["svg", "png", "jpg", "jpeg"];

/// The user's widgets directory: installed widgets live here, and the
/// watcher on it is what makes installs live.
pub fn user_dir() -> PathBuf {
    // same base resolution as settings::config_path
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("kuma-shell").join("widgets")
}

/// The system pool: stock widgets shipped complete but not installed.
pub fn system_dir() -> PathBuf {
    PathBuf::from("/usr/share/kuma-shell/widgets")
}

/// Discover the available custom widgets across both pools. The user's
/// directory wins by stem over the system pool. A directory that fails to
/// parse is skipped with a logged reason, never taken down with the rest:
/// one broken widget must not blank the discovery of the others. The
/// result is sorted by id so listings and tests are stable.
pub fn discover(user: &Path, system: &Path) -> Vec<CustomWidget> {
    let mut widgets: Vec<CustomWidget> = Vec::new();
    for (dir, pool) in [(user, "user"), (system, "system")] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(stem) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // the user's copy wins: skip the system widget a user widget
            // already overrides (widgets may share a stem across pools)
            if pool == "system" && widgets.iter().any(|w| w.id == stem) {
                continue;
            }
            match load(&path, stem) {
                Ok(widget) => {
                    if let Some(existing) = widgets.iter_mut().find(|w| w.id == widget.id) {
                        *existing = widget;
                    } else {
                        widgets.push(widget);
                    }
                }
                Err(reason) => warn!("widget {:?} in {} pool rejected: {}", stem, pool, reason),
            }
        }
    }
    widgets.sort_by(|a, b| a.id.cmp(&b.id));
    widgets
}

/// Load one widget directory. `stem` is the identity the directory grants;
/// the manifest may not contradict it.
fn load(dir: &Path, stem: &str) -> Result<CustomWidget, String> {
    let text = std::fs::read_to_string(dir.join("widget.toml"))
        .map_err(|err| format!("reading widget.toml failed: {err}"))?;
    let manifest: WidgetManifest = toml::from_str(&text)
        .map_err(|err| format!("parsing widget.toml failed: {err}"))?;

    // identity: the stem wins. A manifest id that names something else is
    // a config drift bug waiting to happen, so it is a rejection, not a
    // rename.
    if let Some(id) = &manifest.id
        && id != stem
    {
        return Err(format!(
            "manifest id {id:?} does not match the directory stem {stem:?}"
        ));
    }
    if stem == "custom" {
        return Err("the stem 'custom' is reserved for config syntax".to_string());
    }
    if serde_json::from_value::<WidgetKind>(serde_json::Value::String(stem.to_string())).is_ok() {
        return Err(format!(
            "the stem {stem:?} names a built-in widget; user-made widgets render beside built-ins, not over them"
        ));
    }

    // exactly one runner: both is ambiguity, neither is a widget that
    // cannot run
    let run = match (manifest.source, manifest.command) {
        (Some(_), Some(_)) => {
            return Err("manifest names both source and command".to_string());
        }
        (Some(source), None) => WidgetRun::Script(resolve_relative(dir, &source, "source")?),
        (None, Some(command)) if !command.trim().is_empty() => WidgetRun::Command(command),
        (None, Some(_)) => return Err("command is blank".to_string()),
        (None, None) => return Err("manifest names neither source nor command".to_string()),
    };

    // the interval is the user's choice bounded by ours: a manifest
    // asking for sub-second ticks is clamped, not rejected
    let interval = manifest
        .interval
        .map(Duration::from_secs_f64)
        .unwrap_or(DEFAULT_INTERVAL);
    let interval = if interval < MIN_INTERVAL {
        MIN_INTERVAL
    } else {
        interval
    };

    if let Some(icon) = &manifest.icon
        && icon.starts_with('/')
    {
        return Err("icon must be a themed name or a relative path, not absolute".to_string());
    }

    Ok(CustomWidget {
        id: stem.to_string(),
        label: manifest.label.unwrap_or_else(|| stem.to_string()),
        icon: manifest.icon,
        interval,
        click: manifest.click,
        run,
        dir: dir.to_path_buf(),
    })
}

/// Resolve a manifest-named file against the widget directory. Widgets are
/// directories a user may have downloaded from anywhere: a manifest that
/// escapes itself (`..`) or names an absolute path is rejected, and the
/// file must exist, because a missing source would otherwise surface as a
/// runner error every tick instead of an honest install-time complaint.
fn resolve_relative(dir: &Path, relative: &str, what: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if path.is_absolute() {
        return Err(format!("{what} must be relative to the widget directory"));
    }
    if path.components().any(|c| c == std::path::Component::ParentDir) {
        return Err(format!("{what} must not escape the widget directory"));
    }
    let joined = dir.join(path);
    if !joined.is_file() {
        return Err(format!("{what} {relative:?} does not exist in the widget directory"));
    }
    Ok(joined)
}

/// True when an icon string names a file in the widget directory rather
/// than a themed name. The render arm resolves the two differently; the
/// manifest carries only the string.
pub fn icon_is_file(icon: &str) -> bool {
    Path::new(icon)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ICON_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

// ---------------------------------------------------------------------------
// the runner: one tick loop, one execution in flight per widget
// ---------------------------------------------------------------------------

/// What a widget's source last printed, as the bar renders it. `text` is
/// `None` when there is nothing to show (empty stdout hides the widget,
/// the way Weather hides with no read). `class` is stored but unused at
/// render: Waybar scripts set it for CSS hooks this bar does not have,
/// so accepting and ignoring is the graceful degradation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WidgetRead {
    pub text: Option<String>,
    pub tooltip: Option<String>,
    pub class: Option<String>,
    /// The last run failed (nonzero exit, timeout kill, spawn error):
    /// the previous text stays up, dimmed, with the render arm telling
    /// the honest story in the tooltip.
    pub stale: bool,
}

/// The JSON subset of the Waybar custom-module output contract. Unknown
/// fields are ignored on purpose: scripts written for Waybar carry CSS
/// hints (`percentage`, `interval`, whole class lists) that mean nothing
/// here.
#[derive(Debug, Deserialize)]
struct WidgetJson {
    text: Option<String>,
    tooltip: Option<String>,
    class: Option<String>,
}

/// Parse a source's stdout into a read. Plain text is a label; the JSON
/// object `{"text", "tooltip", "class"}` is the upgrade path. A stdout
/// that starts with `{` but does not parse falls back to plain text:
/// a script mid-rewrite must not blank the bar.
pub fn parse_output(stdout: &str) -> WidgetRead {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return WidgetRead::default();
    }
    if trimmed.starts_with('{')
        && let Ok(json) = serde_json::from_str::<WidgetJson>(trimmed)
    {
        let text = json
            .text
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        return WidgetRead {
            text,
            tooltip: json.tooltip.map(|t| t.trim().to_string()),
            class: json.class,
            stale: false,
        };
    }
    WidgetRead {
        text: Some(trimmed.to_string()),
        ..WidgetRead::default()
    }
}

/// The seconds a source may run before `timeout` kills it. An interval
/// longer than ten seconds earns a matching budget (a slow-but-honest
/// source is not a hung one), capped at a minute: past that the source
/// is hung and the stale path applies.
fn run_budget(interval: Duration) -> u32 {
    interval.as_secs().clamp(10, 60) as u32
}

/// Run one widget's source, blocking, bounded by `timeout`. This is
/// `timed_output`'s idiom: one hung script must cost seconds, not the
/// widget's updates for the rest of the session. Called on the
/// background executor only.
fn run_widget_bounded(widget: &CustomWidget, secs: u32) -> Result<WidgetRead, String> {
    let mut command = Command::new("timeout");
    command.arg(secs.to_string());
    match &widget.run {
        // a source file is executed directly: its shebang applies and it
        // is never reinterpreted by a shell, so a non-executable file is
        // an honest error, not a surprise interpreter
        WidgetRun::Script(path) => {
            command.arg(path);
        }
        WidgetRun::Command(line) => {
            command.arg("sh").arg("-c").arg(line);
        }
    }
    let output = command
        .output()
        .map_err(|err| format!("spawning failed: {err}"))?;
    if !output.status.success() {
        // timeout kills with 124; the exit status is the useful part
        return Err(format!("exited {}", output.status));
    }
    let read = parse_output(String::from_utf8_lossy(&output.stdout).as_ref());
    if read.text.is_none() {
        // an empty read is a valid "nothing to show", but a source that
        // exits cleanly and prints nothing on every tick is more likely
        // a bug; the log is the only trace (debug: it fires per tick)
        debug!("widget {:?} printed nothing", widget.id);
    }
    Ok(read)
}

/// The real-budget wrapper the runner uses.
fn run_widget(widget: &CustomWidget) -> Result<WidgetRead, String> {
    run_widget_bounded(widget, run_budget(widget.interval))
}

/// The entity the bar and the settings page read: the discovery snapshot
/// plus each widget's last read. Owned by the shell's surface deps; the
/// tick loop and the dir watcher are the only writers.
pub struct CustomWidgets {
    widgets: Vec<CustomWidget>,
    reads: HashMap<String, WidgetRead>,
    next_due: HashMap<String, Instant>,
    in_flight: HashSet<String>,
    /// The watcher bumps it when the widgets dirs move; the tick loop
    /// re-discovers when it has moved. The night_light shape, third use.
    generation: Arc<AtomicU64>,
    seen_generation: u64,
}

impl CustomWidgets {
    pub fn new(generation: Arc<AtomicU64>) -> Self {
        Self {
            widgets: Vec::new(),
            reads: HashMap::new(),
            next_due: HashMap::new(),
            in_flight: HashSet::new(),
            generation,
            seen_generation: 0,
        }
    }

    /// The discovery snapshot, stable-sorted by id.
    pub fn widgets(&self) -> &[CustomWidget] {
        &self.widgets
    }

    /// A widget's last read, for the bar arm and the settings rows.
    pub fn read(&self, id: &str) -> Option<&WidgetRead> {
        self.reads.get(id)
    }

    /// Run a widget's click line, fire and forget: bounded like every
    /// subprocess here, reaped like every spawn, and never awaited by
    /// the click handler.
    pub fn click(&self, id: &str) {
        let Some(widget) = self.widgets.iter().find(|w| w.id == id) else {
            return;
        };
        let Some(line) = &widget.click else {
            return;
        };
        match Command::new("timeout")
            .args(["15", "sh", "-c", line])
            .spawn()
        {
            Ok(child) => reap(child),
            Err(err) => error!("widget {:?} click failed to spawn: {err}", widget.id),
        }
    }

    /// Re-read both pools. Keeps the reads and due times of surviving
    /// widgets so a reinstall does not blank them; drops the state of
    /// vanished ones. Returns true when the snapshot moved (the callers
    /// notify only then).
    fn rediscover(&mut self, user: &Path, system: &Path) -> bool {
        let fresh = discover(user, system);
        if fresh == self.widgets {
            return false;
        }
        let alive: HashSet<String> = fresh.iter().map(|w| w.id.clone()).collect();
        self.reads.retain(|id, _| alive.contains(id));
        self.next_due.retain(|id, _| alive.contains(id));
        self.widgets = fresh;
        true
    }

    /// Commit a finished run. A success replaces the read and schedules
    /// the next tick; a failure keeps the previous read and marks it
    /// stale. Returns true when anything moved.
    fn commit(&mut self, id: &str, result: Result<WidgetRead, String>) -> bool {
        self.in_flight.remove(id);
        let interval = self
            .widgets
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.interval)
            .unwrap_or(DEFAULT_INTERVAL);
        self.next_due.insert(id.to_string(), Instant::now() + interval);
        match result {
            Ok(read) => {
                let changed = self.reads.get(id).map(|old| old != &read).unwrap_or(true);
                if changed {
                    self.reads.insert(id.to_string(), read);
                }
                changed
            }
            Err(err) => {
                warn!("widget {:?} run failed: {}", id, err);
                match self.reads.get_mut(id) {
                    Some(read) if !read.stale => {
                        read.stale = true;
                        true
                    }
                    Some(_) => false,
                    None => {
                        // never succeeded: nothing to keep, nothing to show
                        false
                    }
                }
            }
        }
    }

    /// One tick of the runner loop: re-discover if the watcher moved,
    /// then start every due widget's run that is not already in flight.
    fn tick(&mut self, cx: &mut Context<Self>) {
        let generation = self.generation.load(Ordering::Relaxed);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if self.rediscover(&user_dir(), &system_dir()) {
                cx.notify();
            }
        }
        let now = Instant::now();
        for widget in self.widgets.clone() {
            if self.in_flight.contains(&widget.id) {
                continue;
            }
            if !self.next_due.get(&widget.id).is_none_or(|due| now >= *due) {
                continue;
            }
            self.in_flight.insert(widget.id.clone());
            let id = widget.id.clone();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_spawn(async move { run_widget(&widget) })
                    .await;
                let _ = this.update(cx, |state, cx| {
                    if state.commit(&id, result) {
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }
}

/// Wire the runner up: the dir watcher, the entity, and the tick loop.
/// The shell calls this once at boot beside the other `run` fns.
pub fn start(cx: &mut App) -> Entity<CustomWidgets> {
    let generation = Arc::new(AtomicU64::new(0));
    watch_dirs(generation.clone());
    let entity = cx.new(|_| CustomWidgets::new(generation));
    // the first discovery happens here so the bar's first paint already
    // knows the widget set; the reads arrive as the first ticks land
    entity.update(cx, |state, _| {
        state.rediscover(&user_dir(), &system_dir());
    });
    let state = entity.downgrade();
    cx.spawn(async move |cx| {
        loop {
            if state.update(cx, |state, cx| state.tick(cx)).is_err() {
                break;
            }
            cx.background_executor()
                .timer(Duration::from_secs(1))
                .await;
        }
    })
    .detach();
    entity
}

/// Watch both pools for structural change: an install, a removal, a
/// manifest edit bumps the generation counter and the next tick
/// re-discovers. Recursive, because a widget's own script edits apply on
/// the next interval anyway (the runner re-reads the file each run), but
/// a manifest edit inside the widget directory must be seen live.
/// The Access drop is the kuma-files lesson: notify 8.x's watch mask
/// includes OPEN, so any readdir of a watched dir's children fires
/// events and the watcher would re-trigger itself forever.
fn watch_dirs(generation: Arc<AtomicU64>) {
    let _ = std::thread::Builder::new()
        .name("kuma-widgets-watch".to_string())
        .spawn(move || {
            // the user pool exists even when empty: an install target
            // that is already watched
            if let Err(err) = std::fs::create_dir_all(user_dir()) {
                debug!("creating the widgets dir failed: {err}");
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let mut watcher = match notify::recommended_watcher(tx) {
                Ok(watcher) => watcher,
                Err(err) => {
                    error!("widgets watcher unavailable: {err}");
                    return;
                }
            };
            for dir in [user_dir(), system_dir()] {
                match watcher.watch(&dir, RecursiveMode::Recursive) {
                    Ok(()) => {}
                    // a dev machine has no system pool; the user pool is the one that matters
                    Err(err) => debug!("watching {dir:?} failed: {err}"),
                }
            }
            let meaningful = |event: &notify::Result<notify::Event>| {
                matches!(event, Ok(event) if !matches!(event.kind, notify::EventKind::Access(_)))
            };
            loop {
                let Ok(event) = rx.recv() else { break };
                let mut bump = meaningful(&event);
                // coalesce the burst: one install is many events, one bump
                while let Ok(event) = rx.recv_timeout(Duration::from_millis(50)) {
                    bump = bump || meaningful(&event);
                }
                if bump {
                    generation.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A scratch widgets pool with a unique name; the caller's closure
    /// fills it. No tempfile dependency, so the tests build their own.
    fn pool(name: &str, fill: impl Fn(&Path)) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kuma-widgets-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fill(&dir);
        dir
    }

    fn write_widget(pool: &Path, stem: &str, manifest: &str) -> PathBuf {
        let dir = pool.join(stem);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("widget.toml"), manifest).unwrap();
        dir
    }

    fn find<'a>(widgets: &'a [CustomWidget], id: &str) -> &'a CustomWidget {
        widgets
            .iter()
            .find(|w| w.id == id)
            .unwrap_or_else(|| panic!("no widget {id} in {widgets:?}"))
    }

    #[test]
    fn command_only_is_a_complete_widget() {
        let dir = pool("command", |dir| {
            write_widget(dir, "hello", "command = \"echo hello\"\n");
        });
        let widgets = discover(&dir, Path::new("/nonexistent"));
        let widget = find(&widgets, "hello");
        assert_eq!(widget.run, WidgetRun::Command("echo hello".to_string()));
        assert_eq!(widget.label, "hello", "the label defaults to the id");
        assert_eq!(widget.interval, DEFAULT_INTERVAL);
    }

    #[test]
    fn source_must_exist_inside_the_directory() {
        let dir = pool("source", |dir| {
            let widget = write_widget(
                dir,
                "good",
                "source = \"run.sh\"\ninterval = 30\n",
            );
            fs::write(widget.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
            write_widget(dir, "missing", "source = \"absent.sh\"\n");
            write_widget(dir, "escape", "source = \"../good/run.sh\"\n");
            write_widget(dir, "absolute", "source = \"/bin/echo\"\n");
        });
        let widgets = discover(&dir, Path::new("/nonexistent"));
        assert_eq!(widgets.len(), 1, "only the good widget survives: {widgets:?}");
        assert_eq!(
            find(&widgets, "good").run,
            WidgetRun::Script(dir.join("good").join("run.sh"))
        );
        assert_eq!(find(&widgets, "good").interval, Duration::from_secs(30));
    }

    #[test]
    fn interval_is_clamped_from_below() {
        let dir = pool("interval", |dir| {
            write_widget(dir, "hasty", "command = \"true\"\ninterval = 0.1\n");
        });
        let widgets = discover(&dir, Path::new("/nonexistent"));
        let widget = find(&widgets, "hasty");
        assert_eq!(widget.interval, MIN_INTERVAL);
    }

    #[test]
    fn the_stem_is_the_identity() {
        let dir = pool("identity", |dir| {
            write_widget(dir, "weather", "command = \"true\"\nid = \"climate\"\n");
            write_widget(dir, "clock", "command = \"true\"\n");
            write_widget(dir, "honest", "command = \"true\"\n");
        });
        let widgets = discover(&dir, Path::new("/nonexistent"));
        assert_eq!(
            widgets.len(),
            1,
            "id/stem mismatch rejects, built-in stem rejects: {widgets:?}"
        );
        assert_eq!(widgets[0].id, "honest");
    }

    #[test]
    fn the_user_pool_overrides_the_system_pool_by_stem() {
        let user = pool("user", |dir| {
            write_widget(dir, "updates", "command = \"echo mine\"\nlabel = \"Mine\"\n");
        });
        let system = pool("system", |dir| {
            write_widget(dir, "updates", "command = \"echo stock\"\nlabel = \"Stock\"\n");
            write_widget(dir, "other", "command = \"echo other\"\n");
        });
        let widgets = discover(&user, &system);
        assert_eq!(widgets.len(), 2, "the shared stem collapses to one: {widgets:?}");
        assert_eq!(find(&widgets, "updates").label, "Mine");
        assert_eq!(find(&widgets, "other").label, "other");
    }

    #[test]
    fn one_broken_widget_does_not_sink_the_pool() {
        let dir = pool("broken", |dir| {
            write_widget(dir, "bad", "this is not = = toml");
            fs::write(dir.join("not-a-widget"), "just a file").unwrap();
            write_widget(dir, "good", "command = \"true\"\n");
        });
        let widgets = discover(&dir, Path::new("/nonexistent"));
        assert_eq!(widgets.len(), 1);
        assert_eq!(widgets[0].id, "good");
    }

    #[test]
    fn icon_file_vs_themed_name() {
        let dir = pool("icons", |dir| {
            write_widget(
                dir,
                "both",
                "command = \"true\"\nicon = \"face.svg\"\n",
            );
            write_widget(
                dir,
                "themed",
                "command = \"true\"\nicon = \"weather-clear\"\n",
            );
        });
        let widgets = discover(&dir, Path::new("/nonexistent"));
        assert!(icon_is_file(find(&widgets, "both").icon.as_deref().unwrap()));
        assert!(!icon_is_file(find(&widgets, "themed").icon.as_deref().unwrap()));
    }

    #[test]
    fn both_runners_is_a_rejection() {
        let dir = pool("both", |dir| {
            let greedy = write_widget(
                dir,
                "greedy",
                "command = \"true\"\nsource = \"run.sh\"\n",
            );
            fs::write(greedy.join("run.sh"), "#!/bin/sh\n").unwrap();
        });
        assert!(discover(&dir, Path::new("/nonexistent")).is_empty());
    }

    #[test]
    fn click_ride_along() {
        let dir = pool("click", |dir| {
            write_widget(
                dir,
                "tappy",
                "command = \"true\"\nclick = \"notify-send hi\"\n",
            );
        });
        let widgets = discover(&dir, Path::new("/nonexistent"));
        let widget = find(&widgets, "tappy");
        assert_eq!(widget.click.as_deref(), Some("notify-send hi"));
    }

    #[test]
    fn plain_text_is_a_label() {
        let read = parse_output("72 F and sunny\n");
        assert_eq!(read.text.as_deref(), Some("72 F and sunny"));
        assert_eq!(read.tooltip, None);
        assert!(!read.stale);
    }

    #[test]
    fn empty_stdout_hides() {
        assert_eq!(parse_output("\n  \n").text, None);
        assert_eq!(parse_output("").text, None);
    }

    #[test]
    fn the_json_subset_is_the_upgrade_path() {
        let read = parse_output(
            "{\"text\": \"1 update\", \"tooltip\": \"reboot to apply\", \"class\": \"urgent\", \"percentage\": 42}\n",
        );
        assert_eq!(read.text.as_deref(), Some("1 update"));
        assert_eq!(read.tooltip.as_deref(), Some("reboot to apply"));
        assert_eq!(read.class.as_deref(), Some("urgent"));
    }

    #[test]
    fn malformed_json_falls_back_to_plain_text() {
        let read = parse_output("{\"text\": \"unclosed");
        assert_eq!(
            read.text.as_deref(),
            Some("{\"text\": \"unclosed"),
            "a script mid-rewrite must not blank the bar"
        );
    }

    #[test]
    fn json_with_an_empty_text_hides() {
        assert_eq!(parse_output("{\"text\": \"\", \"tooltip\": \"gone\"}").text, None);
    }

    #[test]
    fn a_command_widget_runs_bounded() {
        let widget = CustomWidget {
            id: "hello".to_string(),
            label: "hello".to_string(),
            icon: None,
            interval: Duration::from_secs(10),
            click: None,
            run: WidgetRun::Command("echo from sh".to_string()),
            dir: PathBuf::from("/nonexistent"),
        };
        let read = run_widget(&widget).unwrap();
        assert_eq!(read.text.as_deref(), Some("from sh"));
    }

    #[test]
    fn a_failing_command_is_an_err() {
        let widget = CustomWidget {
            id: "bad".to_string(),
            label: "bad".to_string(),
            icon: None,
            interval: Duration::from_secs(10),
            click: None,
            run: WidgetRun::Command("echo boom >&2; exit 3".to_string()),
            dir: PathBuf::from("/nonexistent"),
        };
        assert!(run_widget(&widget).is_err());
    }

    #[test]
    fn a_hung_source_is_killed_by_the_budget() {
        let widget = CustomWidget {
            id: "hung".to_string(),
            label: "hung".to_string(),
            icon: None,
            interval: Duration::from_secs(10),
            click: None,
            run: WidgetRun::Command("sleep 999".to_string()),
            dir: PathBuf::from("/nonexistent"),
        };
        // one second of budget, so the test is fast; the real budget
        // floors at ten seconds (run_budget), which no test should wait out
        assert!(run_widget_bounded(&widget, 1).is_err());
    }

    #[test]
    fn the_budget_floors_at_ten_and_caps_at_a_minute() {
        assert_eq!(run_budget(Duration::from_secs(1)), 10);
        assert_eq!(run_budget(Duration::from_secs(30)), 30);
        assert_eq!(run_budget(Duration::from_secs(3600)), 60);
    }

    #[test]
    fn a_source_file_is_executed_directly() {
        let dir = pool("exec", |dir| {
            let widget = write_widget(dir, "scripty", "source = \"run.sh\"\n");
            let script = widget.join("run.sh");
            fs::write(&script, "#!/bin/sh\necho direct\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            // the twin without the exec bit: the error is the honesty
            let widget = write_widget(dir, "plain", "source = \"run.sh\"\n");
            let script = widget.join("run.sh");
            fs::write(&script, "#!/bin/sh\necho direct\n").unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        });
        let widgets = discover(&dir, Path::new("/nonexistent"));
        assert!(run_widget(find(&widgets, "scripty")).is_ok());
        assert!(
            run_widget(find(&widgets, "plain")).is_err(),
            "a non-executable source is an honest error, not a shell reinterpretation"
        );
    }

    #[test]
    fn rediscover_keeps_surviving_reads_and_drops_vanished_ones() {
        let generation = Arc::new(AtomicU64::new(0));
        let mut state = CustomWidgets::new(generation);
        let dir = pool("rediscover", |dir| {
            write_widget(dir, "stays", "command = \"echo a\"\n");
            write_widget(dir, "leaves", "command = \"echo b\"\n");
        });
        state.rediscover(&dir, Path::new("/nonexistent"));
        assert!(state.commit("stays", Ok(parse_output("first"))));
        assert!(state.read("stays").is_some());
        assert!(state.read("leaves").is_none(), "never succeeded: nothing to keep");

        // the leaves widget departs; its state goes with it, the stays
        // widget's read and due time survive
        fs::remove_dir_all(dir.join("leaves")).unwrap();
        assert!(state.rediscover(&dir, Path::new("/nonexistent")));
        assert_eq!(state.widgets.len(), 1);
        assert!(state.read("stays").is_some());
        assert!(state.read("leaves").is_none());
        assert!(state.next_due.contains_key("stays"));
        assert!(!state.next_due.contains_key("leaves"));

        // an unchanged pool is not a change
        assert!(!state.rediscover(&dir, Path::new("/nonexistent")));
    }

    #[test]
    fn a_failure_keeps_the_last_text_and_marks_stale() {
        let generation = Arc::new(AtomicU64::new(0));
        let mut state = CustomWidgets::new(generation);
        assert!(state.commit("w", Ok(parse_output("last good"))));
        assert!(state.commit("w", Err("exited 1".to_string())), "fresh to stale is a change");
        let read = state.read("w").unwrap();
        assert_eq!(read.text.as_deref(), Some("last good"));
        assert!(read.stale);
        // a second failure moves nothing: already stale
        assert!(!state.commit("w", Err("exited 2".to_string())));
    }
}
