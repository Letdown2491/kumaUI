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
pub mod widgets;
pub mod theme;
pub mod tray;
pub mod wallpaper;
pub mod weather;
pub mod weather_panel;
pub mod wifi_panel;

/// The version line every surface carries: the crate version plus the
/// commit build.sh stamped in (unstamped builds read unknown), so a
/// running instance pins to an exact build. The format lives in the
/// platform crate; the version stays a call-site argument because it
/// is this crate's.
pub fn version_line() -> String {
    kuma_platform::version::line("kuma-shell", env!("CARGO_PKG_VERSION"))
}

/// The short tag for in-app surfaces: the version alone, the name is
/// already on the page.
pub fn version_tag() -> String {
    kuma_platform::version::tag(env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod changelog_tests {

    #[test]
    fn the_changelog_names_this_version() {
        // bumping Cargo.toml without writing the changelog fails here;
        // the parser lives in the platform crate
        let md = include_str!("../../CHANGELOG.md");
        assert_eq!(
            kuma_platform::version::changelog_version(md, "kuma-shell").as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }
}
