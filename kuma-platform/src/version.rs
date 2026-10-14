//! Version reporting: the `--version` line, the in-app tag, and the
//! changelog guard that keeps a version bump honest.

/// The version line `--version` prints: the app's name and version plus
/// the commit build.sh stamped in, so a running instance pins to an
/// exact build. The version is the caller's (`env!("CARGO_PKG_VERSION")`
/// at the call site), not this crate's.
pub fn line(app: &str, version: &str) -> String {
    format!("{app} {version} (g{})", env!("KUMA_GIT_SHA"))
}

/// The short tag for in-app surfaces: the version alone, the name is
/// already on the page.
pub fn tag(version: &str) -> String {
    format!("v{version} (g{})", env!("KUMA_GIT_SHA"))
}

/// The version the changelog's newest release section names for the
/// given app, or None when the section or the app's line is missing.
/// The version lines sit at column 0 under the release heading; bullets
/// start with a dash or a bracket, so a plain prefix match reads only
/// the header. Each app's guard test asserts this against its own
/// `CARGO_PKG_VERSION`.
pub fn changelog_version(changelog: &str, app: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_names_the_app_and_the_stamp() {
        let line = line("kuma-term", "0.2.0");
        assert!(line.starts_with("kuma-term 0.2.0 "));
        assert!(line.contains("(g"));
    }

    #[test]
    fn tag_is_short() {
        let tag = tag("0.2.0");
        assert!(tag.starts_with('v'));
        assert!(!tag.contains("kuma"));
        assert!(tag.contains("(g"));
    }

    #[test]
    fn changelog_version_reads_the_newest_section() {
        let md = "# Changelog\n\n## Unreleased\n\n## v0.9.0 (2026-01-01)\n\n\
                  kuma-shell 0.9.0\nkuma-files 0.1.0\n\n\
                  ## v0.8.0 (2025-12-01)\n\nkuma-shell 0.8.0\n";
        assert_eq!(
            changelog_version(md, "kuma-shell").as_deref(),
            Some("0.9.0")
        );
        assert_eq!(
            changelog_version(md, "kuma-files").as_deref(),
            Some("0.1.0")
        );
        // no line in the newest section, no release section at all
        assert_eq!(changelog_version(md, "kuma-term"), None);
        assert_eq!(changelog_version("no sections", "kuma-shell"), None);
    }
}
