//! Desktop-entry scanning: the [Desktop Entry] parser, the Exec
//! tokenizer, the applications directory walk, and the wayland-sessions
//! variant. The copies this folds: the shell's launcher, kuma-files'
//! open-with scan, and the greeter's session list, which between them
//! had three INI walkers and two Exec tokenizers with different quote
//! handling.
//!
//! The parse is the spec's: only the `[Desktop Entry]` section speaks,
//! a section header ends it, NoDisplay and Hidden hide, and OnlyShowIn
//! versus NotShowIn follows glib's get_show_in (per stacked value,
//! OnlyShowIn consulted first, the tail hides whatever carries
//! OnlyShowIn when nothing matched).
//!
//! What stays at the callers: the dedupe and sort policies (the shell
//! keeps its most-used entry per name, kuma-files its first per label),
//! the usage bookkeeping, and the launch paths.

use std::path::{Path, PathBuf};

/// One parsed desktop entry. `name` may be empty (kuma-files falls
/// back to the exec, the shell to "Unnamed"); `exec` is the raw Exec
/// line, field codes still in place (the tokenizer is `exec_argv`, the
/// shell's sh -c path strips codes its own way).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub name: String,
    pub generic: String,
    /// Keywords, semicolon separated as the file carries them.
    pub keywords: String,
    pub exec: String,
    pub icon: String,
    pub terminal: bool,
    pub comment: Option<String>,
    /// MimeType, semicolon separated, empties dropped.
    pub mimes: Vec<String>,
    /// The Type key said Application. The scan filters on this; a
    /// Link or Directory entry parses but does not scan.
    pub application: bool,
}

/// One scanned file: where it came from (the launcher records usage
/// against it, kuma-files derives the app id from the file name) and
/// the parsed entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ScannedEntry {
    pub path: PathBuf,
    pub entry: Entry,
}

/// Semicolon-separated list value per the Desktop Entry spec: the
/// trailing semicolon is part of the syntax, so empty segments drop out.
fn show_in_list(value: &str) -> Vec<String> {
    value
        .trim()
        .split(';')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// glib's g_desktop_app_info_get_show_in, hand-rolled: walk the stacked
/// desktop values in order, OnlyShowIn consulted before NotShowIn per
/// value, and the tail hides whatever carries OnlyShowIn when nothing
/// matched (an unset or empty desktop hides every gated entry). Plain
/// case-sensitive equality, like the reference reader and the menu
/// spec's registry ("these are case-sensitive").
pub fn shows_in(only: Option<&[String]>, not: Option<&[String]>, desktop: Option<&str>) -> bool {
    let Some(desktop) = desktop else {
        return only.is_none();
    };
    for env in desktop.split(':').filter(|env| !env.is_empty()) {
        if let Some(only) = only
            && only.iter().any(|name| name.as_str() == env)
        {
            return true;
        }
        if let Some(not) = not
            && not.iter().any(|name| name.as_str() == env)
        {
            return false;
        }
    }
    only.is_none()
}

/// Parse one desktop file's `[Desktop Entry]` section. Hidden,
/// NoDisplay, and not-showing entries read as None (the caller asked
/// for a visible entry); a missing Exec is None. The `desktop` name is
/// the caller's XDG_CURRENT_DESKTOP (None when unset), so the parse is
/// testable without env mutation.
pub fn parse_entry(text: &str, desktop: Option<&str>) -> Option<Entry> {
    let mut entry = Entry::default();
    let mut hidden = false;
    let mut only_show_in: Option<Vec<String>> = None;
    let mut not_show_in: Option<Vec<String>> = None;
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            // a section header starts or ends the entry: later
            // sections ([Desktop Action a], vendor extensions) must
            // not overwrite the main keys (an action's Name=New
            // Window is not the app's name)
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            // a stray line without '=' is skipped, not fatal: the
            // entry should not vanish over one bad line
            continue;
        };
        match key.trim() {
            "Name" => entry.name = value.trim().to_string(),
            "GenericName" => entry.generic = value.trim().to_string(),
            "Keywords" => entry.keywords = value.trim().to_string(),
            "Exec" => entry.exec = value.trim().to_string(),
            "Icon" => entry.icon = value.trim().to_string(),
            "Comment" => entry.comment = Some(value.trim().to_string()),
            "Terminal" => entry.terminal = value.trim() == "true",
            "Type" => entry.application = value.trim() == "Application",
            "MimeType" => {
                entry.mimes = value.split(';').filter(|m| !m.is_empty()).map(str::to_string).collect()
            }
            "NoDisplay" | "Hidden" => hidden = value.trim() == "true",
            "OnlyShowIn" => only_show_in = Some(show_in_list(value)),
            "NotShowIn" => not_show_in = Some(show_in_list(value)),
            _ => {}
        }
    }
    if hidden || !shows_in(only_show_in.as_deref(), not_show_in.as_deref(), desktop) {
        return None;
    }
    if entry.exec.is_empty() {
        return None;
    }
    Some(entry)
}

/// The applications directories, most specific first: the data home,
/// the data dirs, the flatpak exports. The union of the copies (the
/// shell's list missed XDG_DATA_HOME, kuma-files' missed flatpak).
pub fn application_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let data_home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|home| PathBuf::from(home).join(".local/share")));
    if let Some(dir) = data_home {
        dirs.push(dir.join("applications"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    for dir in data_dirs.split(':').filter(|dir| !dir.is_empty()) {
        dirs.push(PathBuf::from(dir).join("applications"));
    }
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/flatpak/exports/share/applications"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    dirs
}

/// Walk the applications directories and parse every .desktop file
/// visible on `desktop` (the caller's XDG_CURRENT_DESKTOP, None when
/// unset). Every visible Type=Application entry surfaces; dedupe and
/// sort are the callers' policies. A few hundred small INI files read
/// in milliseconds; callers scan once and cache.
pub fn scan_applications(desktop: Option<&str>) -> Vec<ScannedEntry> {
    let mut scanned = Vec::new();
    for dir in application_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(entry) = parse_entry(&text, desktop)
                && entry.application
            {
                scanned.push(ScannedEntry { path, entry });
            }
        }
    }
    scanned
}

/// Turn a .desktop Exec line into an argv for one path: tokenize with
/// quote awareness, substitute the field codes, and append the path
/// when the line names none.
pub fn exec_argv(exec: &str, path: &Path, app_name: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = exec.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(&next) = chars.peek() {
                    cur.push(next);
                    chars.next();
                }
            }
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            ending if Some(ending) == quote => quote = None,
            split if quote.is_none() && split.is_whitespace() => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            other => cur.push(other),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }

    let mut argv: Vec<String> = Vec::new();
    let mut has_file = false;
    for token in tokens {
        match token.as_str() {
            "%f" | "%u" | "%F" | "%U" | "%d" | "%D" | "%n" | "%N" => {
                argv.push(path.display().to_string());
                has_file = true;
            }
            "%c" => argv.push(app_name.to_string()),
            "%i" | "%k" | "%v" | "%m" => {}
            field if field.starts_with('%') => {}
            plain => argv.push(plain.to_string()),
        }
    }
    if !has_file {
        argv.push(path.display().to_string());
    }
    argv
}

// --------------------------------------------------------- sessions

/// One wayland session the greeter can start: its display name and
/// the command greetd runs for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    pub exec: String,
}

/// Read the wayland sessions from `dir` (normally
/// /usr/share/wayland-sessions): every desktop entry's Name and Exec,
/// hidden entries skipped. Sorted by name so the list is stable.
pub fn wayland_sessions(dir: &Path) -> Vec<Session> {
    let mut sessions = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return sessions;
    };
    for entry in entries.flatten() {
        let Some(session) = parse_session(&entry.path()) else {
            continue;
        };
        sessions.push(session);
    }
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    sessions
}

/// Parse one session desktop file: the `[Desktop Entry]` section's
/// Name and Exec, NoDisplay or Hidden rejected. Unlike a regular
/// application entry a session's Exec rides verbatim to greetd (no
/// field-code business), and Icon, Type and friends are irrelevant.
fn parse_session(path: &Path) -> Option<Session> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut name = None;
    let mut exec = None;
    let mut hidden = false;
    let mut in_entry = false;
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Name" => name = Some(value.trim().to_string()),
            "Exec" => exec = Some(value.trim().to_string()),
            "NoDisplay" | "Hidden" if value.trim() == "true" => hidden = true,
            _ => {}
        }
    }
    if hidden {
        return None;
    }
    Some(Session {
        name: name?,
        exec: exec?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_parses() {
        let entry = parse_entry(
            "[Desktop Entry]\nName=GNU Image\nExec=gimp-3.0 %U\nIcon=gimp\nTerminal=false\nType=Application\n",
            None,
        )
        .unwrap();
        assert_eq!(entry.name, "GNU Image");
        assert_eq!(entry.exec, "gimp-3.0 %U"); // the raw line, codes and all
        assert!(!entry.terminal);
        assert_eq!(entry.icon, "gimp");
        assert!(entry.application);
    }

    #[test]
    fn desktop_entry_hides_no_display() {
        let text = "[Desktop Entry]\nName=X\nExec=x\nNoDisplay=true\n";
        assert!(parse_entry(text, None).is_none());
    }

    #[test]
    fn desktop_entry_hides_hidden() {
        // Hidden=true means the user deleted the entry; same arm as NoDisplay
        assert!(parse_entry(
            "[Desktop Entry]\nName=X\nExec=x\nHidden=true\nOnlyShowIn=niri\n",
            Some("niri"),
        )
        .is_none());
    }

    #[test]
    fn desktop_entry_hides_other_desktops() {
        // the live find: blueman-adapters gates itself to XFCE;MATE
        let text = "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=XFCE;MATE;\n";
        assert!(parse_entry(text, Some("niri")).is_none());
        assert!(parse_entry(
            "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=niri\n",
            Some("niri"),
        )
        .is_some());
        // no gating keys at all: the common case, shown on any desktop
        assert!(parse_entry("[Desktop Entry]\nName=X\nExec=x\n", Some("niri")).is_some());
        // case-sensitive, like g_str_equal: no folding
        assert!(parse_entry(
            "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=NIRI\n",
            Some("niri"),
        )
        .is_none());
    }

    #[test]
    fn desktop_entry_not_show_in_hides() {
        assert!(parse_entry(
            "[Desktop Entry]\nName=X\nExec=x\nNotShowIn=niri\n",
            Some("niri"),
        )
        .is_none());
        assert!(parse_entry(
            "[Desktop Entry]\nName=X\nExec=x\nNotShowIn=GNOME\n",
            Some("niri"),
        )
        .is_some());
    }

    #[test]
    fn desktop_entry_show_in_precedence_matches_glib() {
        // glib consults OnlyShowIn before NotShowIn per stacked value, so a
        // value both keys share shows the entry; pinned because we claim
        // exact glib behavior
        assert!(parse_entry(
            "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=niri\nNotShowIn=niri\n",
            Some("niri"),
        )
        .is_some());
        // stacked values: any OnlyShowIn match shows, any NotShowIn match hides
        assert!(parse_entry(
            "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=niri\n",
            Some("xfce:niri"),
        )
        .is_some());
        assert!(parse_entry(
            "[Desktop Entry]\nName=X\nExec=x\nNotShowIn=xfce\n",
            Some("xfce:niri"),
        )
        .is_none());
    }

    #[test]
    fn desktop_entry_no_display_wins_over_show_in() {
        assert!(parse_entry(
            "[Desktop Entry]\nName=X\nExec=x\nNoDisplay=true\nOnlyShowIn=niri\n",
            Some("niri"),
        )
        .is_none());
    }

    #[test]
    fn desktop_entry_gated_entry_hides_without_a_desktop() {
        // glib's tail: only_show_in != NULL with no desktop means hidden
        assert!(parse_entry("[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=XFCE\n", None).is_none());
        assert!(parse_entry("[Desktop Entry]\nName=X\nExec=x\n", None).is_some());
    }

    #[test]
    fn desktop_entry_action_sections_do_not_overwrite() {
        // [Desktop Action newWindow] carries its own Name; the main
        // entry's keys must survive it
        let text = "[Desktop Entry]\nName=Files\nExec=files %u\n\
                    [Desktop Action newWindow]\nName=New Window\nExec=files -n\n";
        let entry = parse_entry(text, None).unwrap();
        assert_eq!(entry.name, "Files");
        assert_eq!(entry.exec, "files %u");
    }

    #[test]
    fn desktop_entry_captures_generic_keywords_comment_mimes() {
        let entry = parse_entry(
            "[Desktop Entry]\nName=Koguma\nGenericName=File Manager\nKeywords=files;folders;\n\
             Comment=Browse files\nMimeType=text/plain;inode/directory;\nExec=kuma-files\n",
            None,
        )
        .unwrap();
        assert_eq!(entry.generic, "File Manager");
        assert_eq!(entry.keywords, "files;folders;");
        assert_eq!(entry.comment.as_deref(), Some("Browse files"));
        assert_eq!(entry.mimes, vec!["text/plain", "inode/directory"]);
        // entries without them just read empty, never panic
        let bare = parse_entry("[Desktop Entry]\nName=X\nExec=x\n", None).unwrap();
        assert_eq!(bare.generic, "");
        assert_eq!(bare.keywords, "");
        assert_eq!(bare.comment, None);
        assert!(bare.mimes.is_empty());
    }

    #[test]
    fn exec_argv_tokenizes_with_quotes_and_substitutes() {
        let path = Path::new("/tmp/a b.txt");
        // quotes keep a space inside one token; the escaped quote rides
        assert_eq!(
            exec_argv("gvim --arg \"a b\" 'c\\'d' %f", path, "Editor"),
            vec!["gvim", "--arg", "a b", "c'd", "/tmp/a b.txt"]
        );
        // the path substitutes into the field code's position
        assert_eq!(
            exec_argv("xdg-open %u", Path::new("/tmp/x"), "X"),
            vec!["xdg-open", "/tmp/x"]
        );
        // a line naming no file code gets the path appended
        assert_eq!(exec_argv("eog", path, "X"), vec!["eog", "/tmp/a b.txt"]);
        // unsupported codes drop (%i icon, %k source path), unknown too
        assert_eq!(
            exec_argv("app %i %k %z tail", path, "App"),
            vec!["app", "tail", "/tmp/a b.txt"]
        );
        // %c becomes the app's name; the path appends (no file code)
        assert_eq!(exec_argv("app %c", path, "Editor"), vec!["app", "Editor", "/tmp/a b.txt"]);
    }

    #[test]
    fn wayland_sessions_parse_and_sort() {
        let dir = std::env::temp_dir().join(format!("kuma-sessions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("zshell.desktop"),
            "[Desktop Entry]\nName=Z Shell\nExec=zshell --session\nType=Application\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("niri.desktop"),
            "[Desktop Entry]\nName=Niri\nExec=niri-session\nType=Application\nDesktopNames=niri\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("hidden.desktop"),
            "[Desktop Entry]\nName=Hidden\nExec=hidden\nNoDisplay=true\n",
        )
        .unwrap();
        std::fs::write(dir.join("broken.desktop"), "garbage\n").unwrap();

        let sessions = wayland_sessions(&dir);
        assert_eq!(
            sessions,
            vec![
                Session {
                    name: "Niri".into(),
                    exec: "niri-session".into()
                },
                Session {
                    name: "Z Shell".into(),
                    exec: "zshell --session".into()
                },
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_session_dir_reads_as_empty() {
        assert!(wayland_sessions(Path::new("/nonexistent-kuma-sessions")).is_empty());
    }

    #[test]
    fn application_dirs_are_deduped_and_user_first() {
        // no env mutation in tests (Rust 2024 makes it unsafe); the
        // shape check is all we can pin down portably: every dir ends
        // in applications, and none repeats
        let dirs = application_dirs();
        assert!(!dirs.is_empty());
        let mut names: Vec<_> = dirs.iter().map(|d| d.display().to_string()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), dirs.len(), "dirs repeat: {dirs:?}");
        for dir in &dirs {
            assert!(dir.ends_with("applications"), "{} is not an applications dir", dir.display());
        }
    }
}
