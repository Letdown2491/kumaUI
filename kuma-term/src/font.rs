//! The grid's monospace family. gpui matches family names literally against
//! the installed faces, so the generic "monospace" never resolves: the text
//! system silently falls back to a proportional face and the grid then
//! measures its cells off that face's "M", leaving every column ragged. The
//! alias belongs to fontconfig, so resolve it there first (which also honors
//! a user's alias override), then known mono families present on the system,
//! then the literal name as a last resort.

use std::collections::HashSet;
use std::process::Command;

use gpui::WindowTextSystem;

/// Mono families in preference order, for systems where fontconfig is
/// missing or has no alias. A face present under any of these beats the
/// proportional fallback the text system would pick on its own.
const FALLBACK_FAMILIES: &[&str] = &[
    "Adwaita Mono",
    "JetBrains Mono",
    "Noto Sans Mono",
    "Liberation Mono",
    "DejaVu Sans Mono",
    "Nimbus Mono PS",
];

/// The family to shape the grid with: the theme's explicit pick first,
/// then fontconfig's answer, then known mono families present, then the
/// generic name. Logged once at startup; the view's cell metrics and every
/// StyledText run must use the same name.
pub fn pick_family(explicit: Option<&str>, text_system: &WindowTextSystem) -> String {
    let installed: HashSet<String> = text_system
        .all_font_names()
        .into_iter()
        .map(|name| name.to_lowercase())
        .collect();
    let family = pick(explicit, fc_match_monospace().as_deref(), &installed);
    log::info!("terminal font: {family}");
    family
}

/// The theme's explicit family wins when a face by that name is actually
/// installed; a name no face carries would silently fall back to a
/// proportional face, so it is dropped instead. fc-match never fails or
/// returns empty: for an unknown query it echoes its best match, so its
/// answer is only trusted when a face with that name is installed too.
fn pick(explicit: Option<&str>, fc_family: Option<&str>, installed: &HashSet<String>) -> String {
    if let Some(matched) = explicit {
        let name = first_family(matched);
        if installed.contains(&name.to_lowercase()) {
            return name;
        }
    }
    if let Some(matched) = fc_family {
        let name = first_family(matched);
        if !name.is_empty() && installed.contains(&name.to_lowercase()) {
            return name;
        }
    }
    for name in FALLBACK_FAMILIES {
        if installed.contains(&name.to_lowercase()) {
            return (*name).to_string();
        }
    }
    "monospace".to_string()
}

/// "Family A,Family B" style matches use the first name.
fn first_family(names: &str) -> String {
    names.split(',').next().unwrap_or("").trim().to_string()
}

fn fc_match_monospace() -> Option<String> {
    let output = Command::new("fc-match")
        .args(["-f", "%{family}", "monospace"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok().map(|family| family.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_lowercase()).collect()
    }

    #[test]
    fn fontconfig_answer_wins_when_installed() {
        let installed = set(&["Noto Sans Mono", "Noto Sans"]);
        assert_eq!(pick(None, Some("Noto Sans Mono"), &installed), "Noto Sans Mono");
    }

    #[test]
    fn explicit_theme_family_wins_when_installed() {
        let installed = set(&["JetBrains Mono", "Noto Sans Mono"]);
        assert_eq!(pick(Some("JetBrains Mono"), Some("Noto Sans Mono"), &installed), "JetBrains Mono");
    }

    #[test]
    fn explicit_theme_family_is_dropped_when_not_installed() {
        let installed = set(&["Noto Sans Mono"]);
        assert_eq!(pick(Some("Fantasy Font"), Some("Noto Sans Mono"), &installed), "Noto Sans Mono");
    }

    #[test]
    fn fontconfig_answer_is_dropped_when_no_face_has_that_name() {
        // fc-match echoes garbage for a garbage query, so it must not be
        // trusted blindly
        let installed = set(&["DejaVu Sans Mono"]);
        assert_eq!(pick(None, Some("Does Not Exist"), &installed), "DejaVu Sans Mono");
    }

    #[test]
    fn multi_family_matches_use_the_first_name() {
        let installed = set(&["adwaita mono"]);
        assert_eq!(pick(None, Some("Adwaita Mono,Adwaita Mono"), &installed), "Adwaita Mono");
    }

    #[test]
    fn preference_order_walks_the_static_list() {
        let installed = set(&["Liberation Mono", "Nimbus Mono PS"]);
        assert_eq!(pick(None, None, &installed), "Liberation Mono");
    }

    #[test]
    fn nothing_matches_leaves_the_generic_name() {
        assert_eq!(pick(None, None, &HashSet::new()), "monospace");
    }
}
