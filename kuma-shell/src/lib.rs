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

#[cfg(test)]
mod changelog_tests {

    /// The version the changelog's newest release section names for
    /// the given app, or None when the section or the app's line is
    /// missing. The three version lines sit at column 0 under the
    /// heading; bullets start with a dash or a bracket, so a plain
    /// prefix match reads only the header.
    fn changelog_version(changelog: &str, app: &str) -> Option<String> {
        let rest = changelog.split_once("\n## v")?.1;
        let newest = rest.split("\n## ").next()?;
        let prefix = format!("{app} ");
        newest
            .lines()
            .filter_map(|line| line.trim().strip_prefix(prefix.as_str()))
            .map(str::trim)
            .find(|v| !v.is_empty())
            .map(str::to_string)
    }

    #[test]
    fn the_changelog_names_this_version() {
        // bumping Cargo.toml without writing the changelog fails here
        let md = include_str!("../../CHANGELOG.md");
        assert_eq!(
            changelog_version(md, "kuma-shell").as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn changelog_version_reads_the_newest_section() {
        let md = "# Changelog\n\n## Unreleased\n\n## v0.9.0 (2026-01-01)\n\n\
                  kuma-shell 0.9.0\nkuma-files 0.1.0\n\n\
                  ## v0.8.0 (2025-12-01)\n\nkuma-shell 0.8.0\n";
        assert_eq!(changelog_version(md, "kuma-shell").as_deref(), Some("0.9.0"));
        assert_eq!(changelog_version(md, "kuma-files").as_deref(), Some("0.1.0"));
        // no line in the newest section, no release section at all
        assert_eq!(changelog_version(md, "kuma-term"), None);
        assert_eq!(changelog_version("no sections", "kuma-shell"), None);
    }
}
