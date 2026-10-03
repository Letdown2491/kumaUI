use std::{fs, path::PathBuf};

use gpui::Context;
use log::error;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Left,
    Center,
    Right,
}

pub const SECTIONS: [Section; 3] = [Section::Left, Section::Center, Section::Right];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    Workspaces,
    WindowTitle,
    Apps,
    Cpu,
    Volume,
    Brightness,
    Media,
    Battery,
    Clock,
    Bluetooth,
    Internet,
    Notifications,
    Tray,
    Nostr,
}

/// Where a Widget's icon comes from. The registry owns the decision; render
/// code consults it instead of restating paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetIconSpec {
    Path(&'static str),
    /// drawn at render time from the widget's data (battery: level, charge, AC)
    Generated,
}

/// The widget registry: the single declaration table. Adding a widget = a new
/// entry here + a render arm in bar.rs (compiler-checked) + data in sysmon/niri.
pub struct WidgetSpec {
    pub kind: WidgetKind,
    pub label: &'static str,
    pub icon: Option<WidgetIconSpec>,
}

pub const WIDGETS: &[WidgetSpec] = &[
    WidgetSpec {
        kind: WidgetKind::Workspaces,
        label: "Workspaces",
        icon: None,
    },
    WidgetSpec {
        kind: WidgetKind::WindowTitle,
        label: "Window title",
        icon: None,
    },
    WidgetSpec {
        kind: WidgetKind::Apps,
        label: "Apps",
        icon: Some(WidgetIconSpec::Path("icons/apps.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Cpu,
        label: "CPU",
        icon: Some(WidgetIconSpec::Path("icons/cpu.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Volume,
        label: "Volume",
        icon: Some(WidgetIconSpec::Path("icons/volume.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Brightness,
        label: "Brightness",
        icon: Some(WidgetIconSpec::Path("icons/brightness.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Media,
        label: "Media",
        icon: Some(WidgetIconSpec::Path("icons/media.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Battery,
        label: "Battery",
        icon: Some(WidgetIconSpec::Generated),
    },
    WidgetSpec {
        kind: WidgetKind::Clock,
        label: "Clock",
        icon: Some(WidgetIconSpec::Path("icons/clock.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Bluetooth,
        label: "Bluetooth",
        icon: Some(WidgetIconSpec::Path("icons/bluetooth.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Internet,
        label: "Internet",
        icon: Some(WidgetIconSpec::Path("icons/wifi.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Notifications,
        label: "Notifications",
        icon: Some(WidgetIconSpec::Path("icons/bell.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Tray,
        label: "System tray",
        icon: None,
    },
    WidgetSpec {
        kind: WidgetKind::Nostr,
        label: "Nostr Signer",
        icon: Some(WidgetIconSpec::Path("icons/shield-lock.svg")),
    },
];

impl WidgetKind {
    pub fn spec(self) -> &'static WidgetSpec {
        WIDGETS
            .iter()
            .find(|spec| spec.kind == self)
            .expect("widget registry is exhaustive")
    }

    pub fn label(self) -> &'static str {
        self.spec().label
    }

    pub fn icon_spec(self) -> Option<WidgetIconSpec> {
        self.spec().icon
    }

    pub fn supports_mode(self) -> bool {
        // the apps widget is an action button (opens the launcher panel):
        // always icon-only, it has no data to render as text
        match self {
            WidgetKind::Apps => false,
            kind => kind.icon_spec().is_some(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetMode {
    Icon,
    #[default]
    IconText,
    Text,
}

impl WidgetMode {
    pub fn label(self) -> &'static str {
        match self {
            WidgetMode::Icon => "icon",
            WidgetMode::IconText => "icon+text",
            WidgetMode::Text => "text",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetConfig {
    pub kind: WidgetKind,
    #[serde(default)]
    pub mode: WidgetMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BarWidth {
    #[default]
    Full,
    ThreeQuarter,
    TwoThirds,
    Half,
}

impl BarWidth {
    pub fn label(self) -> &'static str {
        match self {
            BarWidth::Full => "full",
            BarWidth::ThreeQuarter => "3/4",
            BarWidth::TwoThirds => "2/3",
            BarWidth::Half => "half",
        }
    }

    pub fn fraction(self) -> f32 {
        match self {
            BarWidth::Full => 1.0,
            BarWidth::ThreeQuarter => 0.75,
            BarWidth::TwoThirds => 2.0 / 3.0,
            BarWidth::Half => 0.5,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BarAlign {
    Left,
    #[default]
    Center,
    Right,
}

impl BarAlign {
    pub fn label(self) -> &'static str {
        match self {
            BarAlign::Left => "left",
            BarAlign::Center => "center",
            BarAlign::Right => "right",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BarRadius {
    #[default]
    None,
    Sm,
    Md,
    Lg,
    Xl,
}

impl BarRadius {
    pub fn label(self) -> &'static str {
        match self {
            BarRadius::None => "none",
            BarRadius::Sm => "sm",
            BarRadius::Md => "md",
            BarRadius::Lg => "lg",
            BarRadius::Xl => "xl",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CornerRounding {
    pub top_left: bool,
    pub top_right: bool,
    pub bottom_left: bool,
    pub bottom_right: bool,
}

impl Default for CornerRounding {
    fn default() -> Self {
        Self {
            top_left: true,
            top_right: true,
            bottom_left: true,
            bottom_right: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BarConfig {
    pub height: f32,
    pub offset_top: f32,
    pub width: BarWidth,
    pub align: BarAlign,
    pub radius: BarRadius,
    pub corners: CornerRounding,
    pub left: Vec<WidgetConfig>,
    pub center: Vec<WidgetConfig>,
    pub right: Vec<WidgetConfig>,
}

impl Default for BarConfig {
    fn default() -> Self {
        Self {
            height: 36.0,
            offset_top: 0.0,
            width: BarWidth::default(),
            align: BarAlign::default(),
            radius: BarRadius::default(),
            corners: CornerRounding::default(),
            left: Vec::new(),
            center: Vec::new(),
            right: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackgroundConfig {
    pub folder: PathBuf,
    pub current: String,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        let folder = std::env::var("HOME")
            .map(|home| PathBuf::from(home).join("Pictures").join("Wallpapers"))
            .unwrap_or_else(|_| PathBuf::from("Pictures/Wallpapers"));
        Self {
            folder,
            current: "default".to_string(),
        }
    }
}

pub fn default_wallpaper() -> PathBuf {
    PathBuf::from("/usr/share/backgrounds/kuma/kuma-wallpaper.jpg")
}

impl BackgroundConfig {
    pub fn current_path(&self) -> PathBuf {
        if self.current != "default" {
            let path = self.folder.join(&self.current);
            if path.exists() {
                return path;
            }
        }
        default_wallpaper()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub bar: BarConfig,
    #[serde(default)]
    pub background: BackgroundConfig,
    #[serde(default)]
    pub notifications: NotificationSettings,
    #[serde(default)]
    pub dock: DockSettings,
    #[serde(default)]
    pub idle: IdleSettings,
}

/// The idle contract: lock at 15 minutes, monitors off a minute later,
/// and lock before sleep, the three clauses of the swayidle line the
/// shell replaced, with the same numbers the image pinned on noctalia.
/// The defaults are the policy; a settings file may differ, and a
/// timeout of 0 disables its clause.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IdleSettings {
    /// Seconds of idle before the lock engages.
    pub lock_timeout: u64,
    /// Seconds of idle before the monitors power off (DPMS).
    pub screen_off_timeout: u64,
    /// Lock when the machine is about to sleep.
    pub lock_before_suspend: bool,
}

impl Default for IdleSettings {
    fn default() -> Self {
        Self {
            lock_timeout: 900,
            screen_off_timeout: 960,
            lock_before_suspend: true,
        }
    }
}

/// Do-not-disturb: persisted, mirrored into the notification state, and
/// consulted whenever a notification arrives (no toasts while on).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    pub dnd: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockPosition {
    #[default]
    Bottom,
    Top,
    Left,
    Right,
}

impl DockPosition {
    pub fn label(self) -> &'static str {
        match self {
            DockPosition::Bottom => "bottom",
            DockPosition::Top => "top",
            DockPosition::Left => "left",
            DockPosition::Right => "right",
        }
    }

    /// True when the dock runs along the screen's horizontal edges.
    pub fn is_horizontal(self) -> bool {
        matches!(self, DockPosition::Bottom | DockPosition::Top)
    }
}

/// The app dock: pinned favorites plus running windows, its own surface at
/// a screen edge. `pinned` keys are desktop-file paths, the same keys usage
/// counts use.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockSettings {
    pub enabled: bool,
    pub position: DockPosition,
    pub pinned: Vec<String>,
}

impl Default for DockSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            position: DockPosition::default(),
            pinned: default_pins(),
        }
    }
}

/// First-run pins: a terminal and a browser, so a freshly enabled dock
/// isn't an empty strip. Resolved from the standard applications dirs;
/// machines without them start empty. These apply only until the user
/// commits any dock setting, after which the persisted list is the truth.
fn default_pins() -> Vec<String> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(std::path::PathBuf::from(&home).join(".local/share/applications"));
        dirs.push(
            std::path::PathBuf::from(&home).join(".local/share/flatpak/exports/share/applications"),
        );
    }
    dirs.push(std::path::PathBuf::from("/usr/share/applications"));
    dirs.push(std::path::PathBuf::from(
        "/var/lib/flatpak/exports/share/applications",
    ));

    let find = |stems: &[&str]| {
        stems.iter().find_map(|stem| {
            let name = format!("{stem}.desktop");
            dirs.iter()
                .map(|dir| dir.join(&name))
                .find(|path| path.exists())
                .map(|path| path.to_string_lossy().to_string())
        })
    };

    let mut pins = Vec::new();
    if let Some(terminal) = find(&["kitty", "org.wezfurlong.wezterm"]) {
        pins.push(terminal);
    }
    if let Some(browser) = find(&["firefox", "org.chromium.Chromium", "chromium"]) {
        pins.push(browser);
    }
    pins
}

impl Default for Settings {
    fn default() -> Self {
        fn widget(kind: WidgetKind) -> WidgetConfig {
            WidgetConfig {
                kind,
                mode: WidgetMode::default(),
            }
        }
        Self {
            bar: BarConfig {
                left: vec![widget(WidgetKind::Workspaces)],
                center: vec![widget(WidgetKind::WindowTitle)],
                right: vec![
                    widget(WidgetKind::Cpu),
                    widget(WidgetKind::Volume),
                    widget(WidgetKind::Battery),
                    widget(WidgetKind::Clock),
                ],
                ..Default::default()
            },
            background: BackgroundConfig::default(),
            notifications: NotificationSettings::default(),
            dock: DockSettings::default(),
            idle: IdleSettings::default(),
        }
    }
}

fn config_path() -> PathBuf {
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
    base.join("kuma-shell").join("config.toml")
}

impl Settings {
    pub fn load() -> Settings {
        match fs::read_to_string(config_path())
            .map_err(anyhow::Error::msg)
            .and_then(|text| toml::from_str(&text).map_err(anyhow::Error::msg))
        {
            Ok(settings) => settings,
            Err(err) => {
                error!(
                    "loading config from {:?} failed, using defaults: {err:#}",
                    config_path()
                );
                Settings::default()
            }
        }
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            error!("creating config directory failed: {err:#}");
            return;
        }
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(err) = fs::write(&path, text) {
                    error!("writing config to {:?} failed: {err:#}", path);
                }
            }
            Err(err) => error!("serializing config failed: {err:#}"),
        }
    }

    pub fn widgets(&self, section: Section) -> &[WidgetConfig] {
        match section {
            Section::Left => &self.bar.left,
            Section::Center => &self.bar.center,
            Section::Right => &self.bar.right,
        }
    }

    fn widgets_mut(&mut self, section: Section) -> &mut Vec<WidgetConfig> {
        match section {
            Section::Left => &mut self.bar.left,
            Section::Center => &mut self.bar.center,
            Section::Right => &mut self.bar.right,
        }
    }

    /// The one commit ritual: every mutator persists and notifies here.
    /// Callers never call notify.
    fn commit(&mut self, cx: &mut Context<Self>) {
        self.save();
        cx.notify();
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        *self = Settings::load();
        self.commit(cx);
    }

    pub fn set_mode(
        &mut self,
        section: Section,
        index: usize,
        mode: WidgetMode,
        cx: &mut Context<Self>,
    ) {
        if let Some(widget) = self.widgets_mut(section).get_mut(index) {
            widget.mode = mode;
        }
        self.commit(cx);
    }

    /// Move a widget between (or within) sections: the drag-and-drop
    /// seam. `to_index` is insert-before semantics: dropping onto a
    /// chip lands the dragged widget at that chip's position.
    pub fn move_widget(
        &mut self,
        from: Section,
        index: usize,
        to: Section,
        to_index: usize,
        cx: &mut Context<Self>,
    ) {
        if self.move_widget_impl(from, index, to, to_index) {
            self.commit(cx);
        }
    }

    fn move_widget_impl(
        &mut self,
        from: Section,
        index: usize,
        to: Section,
        to_index: usize,
    ) -> bool {
        if index >= self.widgets(from).len() {
            return false;
        }
        // dropping on yourself, or just after yourself, is no move
        if from == to && (index == to_index || index + 1 == to_index) {
            return false;
        }
        let widget = self.widgets_mut(from).remove(index);
        // the removal slides same-section insertions left by one
        let to_index = if from == to && index < to_index {
            to_index - 1
        } else {
            to_index
        };
        let target = self.widgets_mut(to);
        target.insert(to_index.min(target.len()), widget);
        true
    }

    pub fn remove(&mut self, section: Section, index: usize, cx: &mut Context<Self>) {
        if self.remove_impl(section, index) {
            self.commit(cx);
        }
    }

    fn remove_impl(&mut self, section: Section, index: usize) -> bool {
        if index < self.widgets(section).len() {
            self.widgets_mut(section).remove(index);
            return true;
        }
        false
    }

    pub fn add(&mut self, kind: WidgetKind, cx: &mut Context<Self>) {
        if self.add_impl(kind) {
            self.commit(cx);
        }
    }

    fn add_impl(&mut self, kind: WidgetKind) -> bool {
        if self.is_present(kind) {
            return false;
        }
        self.widgets_mut(Section::Right).push(WidgetConfig {
            kind,
            mode: WidgetMode::default(),
        });
        true
    }

    pub fn is_present(&self, kind: WidgetKind) -> bool {
        SECTIONS.iter().any(|&section| {
            self.widgets(section)
                .iter()
                .any(|widget| widget.kind == kind)
        })
    }

    pub fn missing_kinds(&self) -> Vec<WidgetKind> {
        WIDGETS
            .iter()
            .map(|spec| spec.kind)
            .filter(|kind| !self.is_present(*kind))
            .collect()
    }

    /// Where a kind sits on the bar, if it does: the widgets page's
    /// enabled rows consult this for their mode control.
    pub fn position(&self, kind: WidgetKind) -> Option<(Section, usize)> {
        SECTIONS.iter().find_map(|&section| {
            self.widgets(section)
                .iter()
                .position(|widget| widget.kind == kind)
                .map(|index| (section, index))
        })
    }
    pub fn set_height(&mut self, height: f32, cx: &mut Context<Self>) {
        self.bar.height = height;
        self.commit(cx);
    }

    pub fn set_offset_top(&mut self, offset: f32, cx: &mut Context<Self>) {
        self.bar.offset_top = offset;
        self.commit(cx);
    }

    pub fn set_width(&mut self, width: BarWidth, cx: &mut Context<Self>) {
        self.bar.width = width;
        self.commit(cx);
    }

    pub fn set_align(&mut self, align: BarAlign, cx: &mut Context<Self>) {
        self.bar.align = align;
        self.commit(cx);
    }

    pub fn set_radius(&mut self, radius: BarRadius, cx: &mut Context<Self>) {
        self.bar.radius = radius;
        self.commit(cx);
    }

    pub fn toggle_corner(&mut self, corner: Corner, cx: &mut Context<Self>) {
        let corners = &mut self.bar.corners;
        match corner {
            Corner::TopLeft => corners.top_left = !corners.top_left,
            Corner::TopRight => corners.top_right = !corners.top_right,
            Corner::BottomLeft => corners.bottom_left = !corners.bottom_left,
            Corner::BottomRight => corners.bottom_right = !corners.bottom_right,
        }
        self.commit(cx);
    }

    pub fn set_background(&mut self, name: String, cx: &mut Context<Self>) {
        self.background.current = name;
        self.commit(cx);
    }

    pub fn set_background_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        self.background.folder = folder;
        self.background.current = "default".to_string();
        self.commit(cx);
    }

    pub fn set_notifications_dnd(&mut self, dnd: bool, cx: &mut Context<Self>) {
        self.notifications.dnd = dnd;
        self.commit(cx);
    }

    pub fn set_dock_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.dock.enabled = enabled;
        self.commit(cx);
    }

    pub fn set_dock_position(&mut self, position: DockPosition, cx: &mut Context<Self>) {
        self.dock.position = position;
        self.commit(cx);
    }

    /// Pin by desktop-file path (the usage-counts key); already-pinned is a
    /// no-op, order preserved.
    pub fn dock_pin(&mut self, desktop_path: &str, cx: &mut Context<Self>) {
        if self.dock_pin_impl(desktop_path) {
            self.commit(cx);
        }
    }

    pub fn dock_unpin(&mut self, desktop_path: &str, cx: &mut Context<Self>) {
        self.dock.pinned.retain(|pinned| pinned != desktop_path);
        self.commit(cx);
    }

    fn dock_pin_impl(&mut self, desktop_path: &str) -> bool {
        if self.dock.pinned.iter().any(|pinned| pinned == desktop_path) {
            return false;
        }
        self.dock.pinned.push(desktop_path.to_string());
        true
    }

    /// Drag-and-drop reorder: the source takes the target's place in the
    /// pin order. An unpinned source gets pinned by the act; an unpinned
    /// target (a running, unpinned app) means "append at the end".
    pub fn dock_reorder(&mut self, source: &str, target: &str, cx: &mut Context<Self>) {
        if self.dock_reorder_impl(source, target) {
            self.commit(cx);
        }
    }

    fn dock_reorder_impl(&mut self, source: &str, target: &str) -> bool {
        if source == target {
            return false;
        }
        let pinned = &mut self.dock.pinned;
        let source_index = pinned.iter().position(|key| key == source);
        if let Some(index) = source_index {
            pinned.remove(index);
        }
        // the target may have shifted by the removal; recompute
        let insert_at = pinned
            .iter()
            .position(|key| key == target)
            .unwrap_or(pinned.len());
        pinned.insert(insert_at, source.to_string());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_kinds_lists_absent_widgets() {
        let settings = Settings::default();
        let missing = settings.missing_kinds();
        assert!(missing.contains(&WidgetKind::Bluetooth));
        assert!(missing.contains(&WidgetKind::Internet));
        assert!(!missing.contains(&WidgetKind::Cpu));
    }

    #[test]
    fn move_widget_crosses_sections_and_reorders() {
        let mut settings = Settings::default();
        // left: [Workspaces, ...]; right: [Cpu, Volume, Battery, Clock]
        assert!(settings.move_widget_impl(Section::Left, 0, Section::Center, 0));
        assert!(settings.widgets(Section::Left).is_empty());
        assert_eq!(
            settings.widgets(Section::Center)[0].kind,
            WidgetKind::Workspaces
        );

        // within a section: dropping two chips down moves past one;
        // dropping on the immediately next chip would be a no-op
        assert!(!settings.move_widget_impl(Section::Right, 0, Section::Right, 1));
        assert!(settings.move_widget_impl(Section::Right, 0, Section::Right, 2));
        assert_eq!(settings.widgets(Section::Right)[0].kind, WidgetKind::Volume);
        assert_eq!(settings.widgets(Section::Right)[1].kind, WidgetKind::Cpu);

        // out-of-bounds source does nothing
        assert!(!settings.move_widget_impl(Section::Left, 5, Section::Right, 0));
    }

    #[test]
    fn move_widget_self_drops_are_no_ops() {
        let mut settings = Settings::default();
        // dropping on yourself, or just after yourself, changes nothing
        assert!(!settings.move_widget_impl(Section::Right, 1, Section::Right, 1));
        assert!(!settings.move_widget_impl(Section::Right, 1, Section::Right, 2));
        // a real same-section move still works
        assert!(settings.move_widget_impl(Section::Right, 0, Section::Right, 3));
        assert_eq!(settings.widgets(Section::Right)[2].kind, WidgetKind::Cpu);
    }

    #[test]
    fn remove_and_add_round_trip() {
        let mut settings = Settings::default();
        assert!(settings.remove_impl(Section::Right, 0)); // Cpu
        assert!(!settings.is_present(WidgetKind::Cpu));
        assert!(settings.missing_kinds().contains(&WidgetKind::Cpu));

        assert!(settings.add_impl(WidgetKind::Cpu));
        assert!(settings.is_present(WidgetKind::Cpu));
        // adding a present widget is a no-op
        assert!(!settings.add_impl(WidgetKind::Cpu));
        assert_eq!(
            settings
                .widgets(Section::Right)
                .iter()
                .filter(|w| w.kind == WidgetKind::Cpu)
                .count(),
            1
        );
    }

    #[test]
    fn dock_reorder_moves_pins_and_pins_the_dragged() {
        let mut settings = Settings::default();
        settings.dock.pinned = vec!["/x/a.desktop".to_string(), "/x/b.desktop".to_string()];

        // reorder onto the first pin: source takes the target's place
        assert!(settings.dock_reorder_impl("/x/b.desktop", "/x/a.desktop"));
        assert_eq!(settings.dock.pinned.first().unwrap(), "/x/b.desktop");
        assert_eq!(settings.dock.pinned.len(), 2);

        // dragging an unpinned app pins it at the target's place
        assert!(settings.dock_reorder_impl("/x/fresh.desktop", "/x/b.desktop"));
        assert_eq!(
            settings
                .dock
                .pinned
                .iter()
                .position(|k| k == "/x/fresh.desktop"),
            Some(0)
        );

        // dropping onto an unpinned target appends at the end
        assert!(settings.dock_reorder_impl("/x/b.desktop", "/x/running-unpinned.desktop"));
        assert_eq!(settings.dock.pinned.last().unwrap(), "/x/b.desktop");

        // self-drop is a no-op
        assert!(!settings.dock_reorder_impl("/x/a.desktop", "/x/a.desktop"));
    }

    #[test]
    fn default_pins_resolve_to_existing_files_only() {
        // environment-dependent (kitty/firefox may be absent); whatever
        // resolves must be a real desktop file
        for pin in default_pins() {
            assert!(std::path::Path::new(&pin).exists(), "{pin}");
        }
    }

    #[test]
    fn registry_owns_icon_decisions() {
        // no-icon widgets never support icon modes
        assert!(!WidgetKind::Workspaces.supports_mode());
        assert!(!WidgetKind::WindowTitle.supports_mode());
        // the apps widget has an icon but is an action button: icon-only
        assert!(!WidgetKind::Apps.supports_mode());
        // the battery icon is generated at render time, not a themed asset
        assert_eq!(
            WidgetKind::Battery.icon_spec(),
            Some(WidgetIconSpec::Generated)
        );
        assert_eq!(
            WidgetKind::Cpu.icon_spec(),
            Some(WidgetIconSpec::Path("icons/cpu.svg"))
        );
        assert_eq!(WIDGETS.len(), 14);
    }

    #[test]
    fn idle_defaults_carry_the_swayidle_contract() {
        // the three clauses of the line noctalia replaced: lock at 15
        // minutes, monitors off a minute later, lock before sleep
        let settings = Settings::default();
        assert_eq!(settings.idle.lock_timeout, 900);
        assert_eq!(settings.idle.screen_off_timeout, 960);
        assert!(settings.idle.lock_before_suspend);
    }

    #[test]
    fn idle_section_survives_a_partial_config() {
        // a machine's config.toml that predates the idle section (or
        // omits it) keeps the defaults; explicit values win
        let partial: Settings = toml::from_str("[background]\ncurrent = \"default\"\n").unwrap();
        assert_eq!(partial.idle.lock_timeout, 900);

        let explicit: Settings = toml::from_str(
            "[idle]\nlock_timeout = 600\nscreen_off_timeout = 0\nlock_before_suspend = false\n",
        )
        .unwrap();
        assert_eq!(explicit.idle.lock_timeout, 600);
        assert_eq!(explicit.idle.screen_off_timeout, 0);
        assert!(!explicit.idle.lock_before_suspend);
    }
}
