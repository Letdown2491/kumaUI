pub mod bar;
pub mod bluetooth_panel;
pub mod calendar;
pub mod controls;
pub mod displays;
pub mod dock;
pub mod icons;
pub mod idle;
pub mod imaging;
pub mod launcher;
pub mod lock;
pub mod msg;
pub mod niri;
pub mod nostr;
pub mod nostr_panel;
pub mod notifications;
pub mod notifications_view;
pub mod osd;
pub mod panel;
pub mod panel_kit;
pub mod polkit;
pub mod power_panel;
pub mod session;
pub mod settings;
pub mod settings_view;
pub mod slider_panel;
pub mod surfaces;
pub mod sway;
pub mod sysinfo_panel;
pub mod sysmon;
pub mod night_light;
pub mod palette;
pub mod greeter;
pub mod theme;
pub mod tray;
pub mod wallpaper;
pub mod weather;
pub mod weather_panel;
pub mod wifi_panel;

/// The version line every surface carries: the crate version plus the
/// commit build.sh stamped in (option_env! falls back when cargo runs
/// without the stamp), so a running instance pins to an exact build.
pub fn version_line() -> String {
    format!(
        "kuma-shell {} (g{})",
        env!("CARGO_PKG_VERSION"),
        option_env!("KUMA_GIT_SHA").unwrap_or("unknown"),
    )
}

/// The short tag for in-app surfaces: the version alone, the name is
/// already on the page.
pub fn version_tag() -> String {
    format!(
        "v{} (g{})",
        env!("CARGO_PKG_VERSION"),
        option_env!("KUMA_GIT_SHA").unwrap_or("unknown"),
    )
}
