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
    Ram,
    Temp,
    Disk,
    Volume,
    Mic,
    Brightness,
    PowerProfile,
    Media,
    Battery,
    Clock,
    Bluetooth,
    Internet,
    Notifications,
    Tray,
    Nostr,
    Weather,
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
        kind: WidgetKind::Ram,
        label: "Memory",
        icon: Some(WidgetIconSpec::Path("icons/memory.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Temp,
        label: "CPU temperature",
        icon: Some(WidgetIconSpec::Path("icons/temp.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Disk,
        label: "Disk",
        icon: Some(WidgetIconSpec::Path("icons/drive.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Volume,
        label: "Volume",
        icon: Some(WidgetIconSpec::Path("icons/volume.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Mic,
        label: "Microphone",
        icon: Some(WidgetIconSpec::Path("icons/mic.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::Brightness,
        label: "Brightness",
        icon: Some(WidgetIconSpec::Path("icons/brightness.svg")),
    },
    WidgetSpec {
        kind: WidgetKind::PowerProfile,
        label: "Power profile",
        icon: Some(WidgetIconSpec::Path("icons/power-profile.svg")),
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
    WidgetSpec {
        kind: WidgetKind::Weather,
        label: "Weather",
        icon: Some(WidgetIconSpec::Generated),
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

/// The screen edge the bar hangs from. Panels, toasts, and the OSD open
/// off the bar's inner face, so a bottom bar flips them all upward.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BarPosition {
    #[default]
    Top,
    Bottom,
}

impl BarPosition {
    pub fn label(self) -> &'static str {
        match self {
            BarPosition::Top => "top",
            BarPosition::Bottom => "bottom",
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
    pub position: BarPosition,
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
            position: BarPosition::default(),
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
    /// Rotate the folder's images every N minutes (0 = off). The
    /// current name rides the existing config round-trip, so the
    /// shown wallpaper survives a restart.
    pub rotate_minutes: u32,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        let folder = std::env::var("HOME")
            .map(|home| PathBuf::from(home).join("Pictures").join("Wallpapers"))
            .unwrap_or_else(|_| PathBuf::from("Pictures/Wallpapers"));
        Self {
            folder,
            current: "default".to_string(),
            rotate_minutes: 0,
        }
    }
}

/// Wallpaper-derived theming: off until proven.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub wallpaper_derived: bool,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            wallpaper_derived: false,
        }
    }
}

/// Night light: a manual toggle plus an optional fixed-time window.
/// No geolocation, no sunrise tables: the window is what the user
/// writes. `start > end` spans midnight; `start == end` means the
/// whole day.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NightLightConfig {
    pub enabled: bool,
    /// Color temperature while applied, 2500..=6500.
    pub kelvin: u32,
    /// "HH:MM" window start, or None for always on.
    pub window_start: Option<String>,
    /// "HH:MM" window end, or None for always on.
    pub window_end: Option<String>,
}

impl Default for NightLightConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            kelvin: 6500,
            window_start: None,
            window_end: None,
        }
    }
}

impl NightLightConfig {
    /// The parsed window: `None` means no window restriction. An
    /// unparsable endpoint is ignored (treated as no window) rather
    /// than blocking the feature.
    pub fn window(&self) -> Option<((u32, u32), (u32, u32))> {
        let (Some(start), Some(end)) = (&self.window_start, &self.window_end) else {
            return None;
        };
        match (parse_hhmm(start), parse_hhmm(end)) {
            (Some(start), Some(end)) => Some((start, end)),
            _ => None,
        }
    }

    /// Whether gamma should be tinted right now: the toggle is on
    /// AND (there is no window OR the local time is inside it).
    pub fn active_now(&self, now: (u32, u32)) -> bool {
        if !self.enabled {
            return false;
        }
        match self.window() {
            None => true,
            Some((start, end)) => window_contains(start, end, now),
        }
    }
}

/// "HH:MM" to (hour, minute).
pub fn parse_hhmm(text: &str) -> Option<(u32, u32)> {
    let (hour, minute) = text.trim().split_once(':')?;
    let (hour, minute) = (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?);
    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute))
}

/// The dropdown trio's hour choices (12-hour clock).
pub const HOUR_CHOICES: [&str; 12] = [
    "12", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11",
];
/// The dropdown trio's minute choices.
pub const MINUTE_CHOICES: [&str; 4] = ["00", "15", "30", "45"];
/// The dropdown trio's half-day choices.
pub const MERIDIEM_CHOICES: [&str; 2] = ["AM", "PM"];

/// "HH:MM" 24-hour to (hour choice index, minute choice index,
/// meridiem choice index) for the dropdown trio. An absent or
/// unparsable value lands on 12:00 AM by index.
pub fn to_12h(text: Option<&str>) -> (usize, usize, usize) {
    let Some((hour, minute)) = text.and_then(parse_hhmm) else {
        return (0, 0, 0);
    };
    let pm = hour >= 12;
    let hour12 = match hour % 12 {
        0 => 0,
        h => h,
    };
    (hour12 as usize, (minute / 15) as usize, usize::from(pm))
}

/// The dropdown trio's indices back to "HH:MM" 24-hour.
pub fn from_12h(hour: usize, minute: usize, meridiem: usize) -> String {
    let mut hour24 = match hour {
        0 => 0, // "12" AM
        h => h, // 1..=11 stay
    };
    if hour == 0 && meridiem == 1 {
        hour24 = 12; // "12" PM is noon
    } else if meridiem == 1 {
        hour24 += 12;
    }
    format!("{hour24:02}:{}", MINUTE_CHOICES[minute.min(3)])
}

/// The pinned schedule rule: `start == end` means the whole day,
/// `start > end` spans midnight.
pub fn window_contains(start: (u32, u32), end: (u32, u32), now: (u32, u32)) -> bool {
    if start == end {
        return true;
    }
    if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

/// The linear gamma ramp for a color temperature: white at 6500K
/// and above (6500 is the "off" temperature, and sRGB white is D65
/// anyway), red-heavy and blue-starved as the temperature drops.
/// The per-channel scale comes from the Tanner Helland approximation,
/// clamped so nothing saturates past full scale. The protocol's ramps
/// are 16-bit (the XML says "16-byte", a long-standing typo: the fd
/// length must be three times gamma size).
pub fn kelvin_ramp(kelvin: u32, size: u16) -> Vec<[u16; 3]> {
    if kelvin >= 6500 {
        return (0..size)
            .map(|step| {
                let level = (f64::from(step) / f64::from(size.saturating_sub(1)).max(1.)
                    * 65535.)
                    .round() as u16;
                [level, level, level]
            })
            .collect();
    }
    let kelvin = kelvin.clamp(1000, 40000) as f64 / 100.;
    let (r, g, b) = {
        let t = kelvin;
        let red = if t <= 66. {
            255.
        } else {
            329.7 * (t - 60.).powf(-0.1332047592)
        };
        let green = if t <= 66. {
            99.47 * t.ln() - 161.12
        } else {
            288.12 * (t - 60.).powf(-0.0755148492)
        };
        let blue = if t >= 66. {
            255.
        } else if t <= 19. {
            0.
        } else {
            138.52 * (t - 10.).ln() - 305.04
        };
        (
            red.clamp(0., 255.) / 255.,
            green.clamp(0., 255.) / 255.,
            blue.clamp(0., 255.) / 255.,
        )
    };
    (0..size)
        .map(|step| {
            let level = f64::from(step) / f64::from(size.saturating_sub(1)).max(1.);
            let scale = |c: f64| ((c * level) * 65535.).round() as u16;
            [scale(r), scale(g), scale(b)]
        })
        .collect()
}

pub fn default_wallpaper() -> PathBuf {
    PathBuf::from("/usr/share/backgrounds/kuma/kuma-wallpaper.jpg")
}

/// The image files in a wallpapers folder, sorted by name: the
/// backgrounds gallery and the rotation runner share one listing.
pub fn background_images(folder: &std::path::Path) -> Vec<(String, PathBuf)> {
    const IMAGE_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "avif"];
    let mut files: Vec<(String, PathBuf)> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file()
                && path.extension().is_some_and(|ext| {
                    IMAGE_EXTENSIONS.contains(&ext.to_string_lossy().to_lowercase().as_str())
                })
        })
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().to_string();
            Some((name, path))
        })
        .collect();
    files.sort();
    files
}

/// The next wallpaper in the folder: the entry after the current one,
/// wrapping; a current the folder doesn't know falls to the first
/// entry. A folder with nothing in it has no next.
pub fn next_background(entries: &[(String, PathBuf)], current: &str) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let position = entries.iter().position(|(name, _)| name == current);
    let next = match position {
        Some(index) => (index + 1) % entries.len(),
        None => 0,
    };
    Some(entries[next].0.clone())
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
    pub theme: ThemeConfig,
    #[serde(default)]
    pub night_light: NightLightConfig,
    #[serde(default)]
    pub notifications: NotificationSettings,
    #[serde(default)]
    pub dock: DockSettings,
    #[serde(default)]
    pub idle: IdleSettings,
    #[serde(default)]
    pub sysinfo: SysInfoSettings,
    #[serde(default)]
    pub weather: WeatherConfig,
}

/// The sysinfo widgets' knobs: which mount the disk widget watches.
/// A missing or unreadable mount is a hidden widget, not an error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SysInfoSettings {
    pub disk_mount: std::path::PathBuf,
}

impl Default for SysInfoSettings {
    fn default() -> Self {
        Self {
            disk_mount: std::path::PathBuf::from("/"),
        }
    }
}

/// A resolved location: what Nominatim answered, cached so the weather
/// poll never geocodes and the field can show the match it picked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedLocation {
    pub label: String,
    pub lat: String,
    pub lon: String,
}

/// The weather widget's knobs: the location query, its resolved
/// coordinates (empty until a resolve landed), and the unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WeatherConfig {
    /// The raw query text ("Hillsboro, OR", "97123").
    pub query: String,
    /// The resolved match, cached across restarts.
    pub resolved: Option<ResolvedLocation>,
    /// Fahrenheit (true, the default) or Celsius.
    pub fahrenheit: bool,
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            query: String::new(),
            resolved: None,
            fahrenheit: true,
        }
    }
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
/// Quiet hours add a scheduled window during which the state holds DND
/// on automatically; the hours are whole hours of the day (24h), and a
/// window that crosses midnight (22 to 7) is the common case.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    pub dnd: bool,
    /// The quiet window's start hour (0-23); None disables scheduling.
    pub quiet_from: Option<u8>,
    /// The quiet window's end hour (0-23); both ends must be set.
    pub quiet_to: Option<u8>,
    /// Critical-urgency notifications pass through during quiet hours.
    pub quiet_urgent: bool,
}

/// Whether `now` (minutes since midnight) sits inside the quiet window
/// between the two hours: a same-day window is a straight range, a
/// midnight-crossing window is the complement, and equal ends mean
/// always-on.
pub fn in_quiet_window(now: u32, from: u8, to: u8) -> bool {
    let (from, to) = (u32::from(from) * 60, u32::from(to) * 60);
    if from == to {
        return true;
    }
    if from < to {
        (from..to).contains(&now)
    } else {
        now >= from || now < to
    }
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
            theme: ThemeConfig::default(),
            night_light: NightLightConfig::default(),
            notifications: NotificationSettings::default(),
            dock: DockSettings::default(),
            idle: IdleSettings::default(),
            sysinfo: SysInfoSettings::default(),
            weather: WeatherConfig::default(),
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

    pub fn set_position(&mut self, position: BarPosition, cx: &mut Context<Self>) {
        self.bar.position = position;
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
        self.refresh_theme(cx);
    }

    pub fn set_theme_derived(&mut self, derived: bool, cx: &mut Context<Self>) {
        self.theme.wallpaper_derived = derived;
        self.commit(cx);
        self.refresh_theme(cx);
    }

    pub fn set_night_light_enabled(&mut self, on: bool, cx: &mut Context<Self>) {
        self.night_light.enabled = on;
        self.commit(cx);
    }

    pub fn set_night_light_kelvin(&mut self, kelvin: u32, cx: &mut Context<Self>) {
        self.night_light.kelvin = kelvin.clamp(2500, 6500);
        self.commit(cx);
    }

    /// Set the window ends as given. One end without the other is
    /// stored but stays inactive (window() needs both); an empty
    /// commit clears just the edited end.
    pub fn set_night_light_window(
        &mut self,
        start: Option<String>,
        end: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.night_light.window_start = start;
        self.night_light.window_end = end;
        self.commit(cx);
    }

    /// Re-derive the palette from the current wallpaper when
    /// wallpaper-derived theming is on: decode, seed, generate, swap
    /// the live theme. A failure at any step keeps the previous
    /// palette (the defaults ultimately backstop); turning the flag
    /// off restores the constants outright. Surfaces pick the swap up
    /// on their next render.
    pub fn refresh_theme(&self, cx: &mut Context<Self>) {
        if !self.theme.wallpaper_derived {
            crate::theme::set_current(crate::theme::Theme::default());
            self.publish_palette();
            return;
        }
        let path = self.background.current_path();
        cx.spawn(async move |this, cx| {
        let derived = cx
            .background_executor()
            .spawn(async move {
                crate::imaging::decode_sampled(&path).map(|sample| {
                    crate::palette::palette(&sample, crate::palette::Flavor::Faithful)
                })
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(theme) = derived {
                    crate::theme::set_current(theme);
                }
                this.publish_palette();
                cx.notify();
            });
        })
        .detach();
    }

    /// Publish the live palette for session neighbors (kuma-files and
    /// friends): a key=value file in the runtime dir, written
    /// atomically so a reader never sees a half file. Absent or
    /// unreadable on either side is fine; the built-ins backstop.
    pub fn publish_palette(&self) {
        let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
            return;
        };
        let dir = PathBuf::from(runtime).join("kuma-shell");
        if let Err(err) = std::fs::create_dir_all(&dir) {
            log::error!("palette dir: {err}");
            return;
        }
        let theme = crate::theme::current();
        let tmp = dir.join("palette.tmp");
        if let Err(err) = std::fs::write(&tmp, crate::theme::palette_text(&theme))
            .and_then(|()| std::fs::rename(&tmp, dir.join("palette")))
        {
            log::error!("palette write: {err}");
        }
    }

    pub fn set_background_rotate(&mut self, minutes: u32, cx: &mut Context<Self>) {
        self.background.rotate_minutes = minutes;
        self.commit(cx);
    }

    pub fn set_background_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        self.background.folder = folder;
        self.background.current = "default".to_string();
        self.commit(cx);
        self.refresh_theme(cx);
    }

    pub fn set_notifications_dnd(&mut self, dnd: bool, cx: &mut Context<Self>) {
        self.notifications.dnd = dnd;
        self.commit(cx);
    }

    pub fn set_quiet_hours(&mut self, from: Option<u8>, to: Option<u8>, cx: &mut Context<Self>) {
        self.notifications.quiet_from = from;
        self.notifications.quiet_to = to;
        self.commit(cx);
    }

    pub fn set_quiet_urgent(&mut self, urgent: bool, cx: &mut Context<Self>) {
        self.notifications.quiet_urgent = urgent;
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

    pub fn set_idle_lock_timeout(&mut self, seconds: u64, cx: &mut Context<Self>) {
        self.idle.lock_timeout = seconds;
        self.commit(cx);
    }

    pub fn set_idle_screen_off_timeout(&mut self, seconds: u64, cx: &mut Context<Self>) {
        self.idle.screen_off_timeout = seconds;
        self.commit(cx);
    }

    pub fn set_idle_lock_before_suspend(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.idle.lock_before_suspend = enabled;
        self.commit(cx);
    }

    /// The weather location: the query text and its resolved match
    /// land together, so a stale label never outlives its coordinates.
    /// A cleared query drops the resolve too.
    pub fn set_weather_location(
        &mut self,
        query: String,
        resolved: Option<ResolvedLocation>,
        cx: &mut Context<Self>,
    ) {
        self.weather.query = query;
        self.weather.resolved = resolved;
        self.commit(cx);
    }

    pub fn set_weather_fahrenheit(&mut self, fahrenheit: bool, cx: &mut Context<Self>) {
        self.weather.fahrenheit = fahrenheit;
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
    fn window_start_equal_end_is_the_whole_day() {
        // pinned: start == end means the whole day, not an empty window
        assert!(window_contains((22, 0), (22, 0), (3, 0)));
        assert!(window_contains((22, 0), (22, 0), (12, 0)));
        assert!(window_contains((22, 0), (22, 0), (22, 0)));
    }

    #[test]
    fn window_start_after_end_spans_midnight() {
        // pinned: 21:00 to 07:00 covers the night on both sides
        assert!(window_contains((21, 0), (7, 0), (23, 30)));
        assert!(window_contains((21, 0), (7, 0), (2, 15)));
        assert!(!window_contains((21, 0), (7, 0), (12, 0)));
        // the boundary belongs to the start, not the end
        assert!(window_contains((21, 0), (7, 0), (21, 0)));
        assert!(!window_contains((21, 0), (7, 0), (7, 0)));
    }

    #[test]
    fn window_start_before_end_is_the_ordinary_day_range() {
        assert!(window_contains((9, 0), (17, 30), (12, 0)));
        assert!(!window_contains((9, 0), (17, 30), (8, 59)));
        assert!(!window_contains((9, 0), (17, 30), (17, 30)));
    }

    #[test]
    fn active_now_needs_the_toggle_and_the_window() {
        let mut config = NightLightConfig::default();
        assert!(!config.active_now((23, 0)), "toggle off");
        config.enabled = true;
        assert!(config.active_now((23, 0)), "no window: always on");
        config.window_start = Some("21:00".into());
        config.window_end = Some("07:00".into());
        assert!(config.active_now((2, 0)));
        assert!(!config.active_now((12, 0)));
        // an unparsable endpoint degrades to no window, not to off
        config.window_end = Some("garbage".into());
        assert!(config.active_now((12, 0)));
    }

    #[test]
    fn kelvin_ramp_is_identity_at_6500_and_red_heated_below() {
        let ramp = kelvin_ramp(6500, 256);
        assert_eq!(ramp.len(), 256);
        assert_eq!(ramp[255], [65535, 65535, 65535], "6500K is white");
        assert_eq!(ramp[0], [0; 3], "black stays black");
        let warm = kelvin_ramp(2700, 256);
        assert_eq!(warm[255][0], 65535, "red full");
        assert!(
            warm[255][2] < 25000,
            "blue starved at 2700K: {}",
            warm[255][2]
        );
        // below the slider floor the clamp holds, nothing saturates
        let floor = kelvin_ramp(2500, 256);
        assert!(floor[255].iter().all(|c| *c <= 65535));
    }

    #[test]
    fn parse_hhmm_rules() {
        assert_eq!(parse_hhmm("21:30"), Some((21, 30)));
        assert_eq!(parse_hhmm(" 7:05 "), Some((7, 5)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("nope"), None);
    }

    #[test]
    fn twelve_hour_conversions_round_trip() {
        // every "HH:MM" on the quarter hours survives the 12-hour
        // dropdown trio's round trip
        for hour in 0..24 {
            for minute in [0, 15, 30, 45] {
                let text = format!("{hour:02}:{minute:02}");
                let (h, m, pm) = to_12h(Some(&text));
                assert_eq!(from_12h(h, m, pm), text, "round trip {text}");
            }
        }
        // edges: midnight is 12 AM, noon is 12 PM, 11 PM is 23:00
        assert_eq!(from_12h(0, 0, 0), "00:00");
        assert_eq!(from_12h(0, 0, 1), "12:00");
        assert_eq!(from_12h(11, 3, 1), "23:45");
        // an absent or unparsable value lands somewhere coherent
        assert_eq!(to_12h(None), (0, 0, 0));
        assert_eq!(to_12h(Some("garbage")), (0, 0, 0));
        // off-grid minutes snap to the nearest quarter below
        assert_eq!(to_12h(Some("21:37")), (9, 2, 1));
    }

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
        assert_eq!(WIDGETS.len(), 20);
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

    #[test]
    fn quiet_window_matches_wrap_and_bounds() {
        // a same-day window is a straight range
        assert!(in_quiet_window(13 * 60 + 30, 13, 14));
        assert!(!in_quiet_window(14 * 60, 13, 14));
        // the common case: 22 to 7 crosses midnight
        assert!(in_quiet_window(23 * 60, 22, 7));
        assert!(in_quiet_window(5 * 60, 22, 7));
        assert!(!in_quiet_window(12 * 60, 22, 7));
        assert!(!in_quiet_window(21 * 60 + 59, 22, 7));
        assert!(in_quiet_window(22 * 60, 22, 7));
        // equal ends mean always-on
        assert!(in_quiet_window(0, 9, 9));
        assert!(in_quiet_window(23 * 60 + 59, 9, 9));
    }

    #[test]
    fn next_background_walks_and_wraps() {
        let entries = [
            ("a.jpg".to_string(), PathBuf::from("a")),
            ("b.jpg".to_string(), PathBuf::from("b")),
            ("c.jpg".to_string(), PathBuf::from("c")),
        ];
        assert_eq!(next_background(&entries, "a.jpg").as_deref(), Some("b.jpg"));
        assert_eq!(next_background(&entries, "c.jpg").as_deref(), Some("a.jpg"));
        // a current the folder doesn't know falls to the first
        assert_eq!(next_background(&entries, "zzz").as_deref(), Some("a.jpg"));
        assert_eq!(next_background(&[], "a.jpg"), None);
    }

    #[test]
    fn idle_clocks_are_independent() {
        // the clocks are documented, not enforced: a config whose
        // screens would blank before the lock engages round-trips
        // unchanged, no clamping
        let inverted: Settings = toml::from_str(
            "[idle]\nlock_timeout = 900\nscreen_off_timeout = 300\nlock_before_suspend = true\n",
        )
        .unwrap();
        assert_eq!(inverted.idle.lock_timeout, 900);
        assert_eq!(inverted.idle.screen_off_timeout, 300);
    }
}
