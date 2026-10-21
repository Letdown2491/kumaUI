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

use std::path::{Path, PathBuf};
use std::time::Duration;

use log::warn;
use serde::Deserialize;

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
}
