use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    App, Context, Div, FocusHandle, Focusable, FontWeight, KeyDownEvent, ObjectFit, Render,
    SharedString, Window, div, img, point, prelude::*, px, rgb, rgba,
};

use crate::imaging::IconImage;
use crate::panel_kit as kit;

/// The launcher grid's 22px cells decode at twice that for hidpi screens.
const ICON_RESOLVE_SIZE: u32 = 44;

#[derive(Clone, Debug)]
pub struct AppEntry {
    pub name: String,
    /// GenericName from the desktop entry, e.g. "File Manager".
    pub generic: String,
    /// Keywords from the desktop entry, semicolon separated.
    pub keywords: String,
    pub exec: String,
    pub terminal: bool,
    pub icon: String,
    pub desktop_path: String,
    pub usage: u64,
}

pub fn load_apps() -> Vec<AppEntry> {
    // One read per scan: the desktop name never changes mid-session, and
    // threading it in as a plain value keeps the filter testable without
    // env mutation. Feeds the launcher, the dock's app menu, and the
    // volume panel's per-app rows, so all three honor the show-in pair.
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").ok()
        .filter(|name| !name.is_empty());
    let mut dirs: Vec<PathBuf> = Vec::new();
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for data_dir in data_dirs.split(':').filter(|dir| !dir.is_empty()) {
        dirs.push(PathBuf::from(data_dir).join("applications"));
    }
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/share/applications"));
        dirs.push(home.join(".local/share/flatpak/exports/share/applications"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));

    let usage = load_usage();
    let mut apps = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                let desktop_path = path.to_string_lossy().to_string();
                if let Some(mut app) = parse_desktop_entry(&text, desktop.as_deref()) {
                    app.desktop_path = desktop_path.clone();
                    app.usage = usage.get(&desktop_path).copied().unwrap_or(0);
                    apps.push(app);
                }
            }
        }
    }
    // same app can ship from several sources (system + flatpak): keep the
    // most-used entry per name
    let mut by_name: HashMap<String, AppEntry> = HashMap::new();
    for app in apps {
        let key = app.name.to_lowercase();
        match by_name.get(&key) {
            Some(existing) if existing.usage >= app.usage => {}
            _ => {
                by_name.insert(key, app);
            }
        }
    }
    let mut apps: Vec<AppEntry> = by_name.into_values().collect();
    apps.sort_by(|a, b| {
        b.usage
            .cmp(&a.usage)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    apps
}

fn usage_file() -> Option<PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("kuma-shell").join("usage.json"))
}

fn load_usage() -> HashMap<String, u64> {
    let Some(path) = usage_file() else {
        return HashMap::new();
    };
    if !path.exists() {
        // seed from noctalia's usage counts on first run
        let noctalia = std::env::var("HOME")
            .ok()
            .map(|home| PathBuf::from(home).join(".local/state/noctalia/usage_counts.json"));
        if let Some(noctalia) = noctalia
            && let Ok(text) = std::fs::read_to_string(&noctalia)
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(apps) = parsed.get("Applications").and_then(|v| v.as_object())
        {
            let map: HashMap<String, u64> = apps
                .iter()
                .filter_map(|(key, count)| count.as_u64().map(|count| (key.clone(), count)))
                .collect();
            save_usage(&map);
            return map;
        }
        return HashMap::new();
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|parsed| {
            let apps = parsed.get("Applications")?.as_object()?;
            Some(
                apps.iter()
                    .filter_map(|(key, count)| count.as_u64().map(|count| (key.clone(), count)))
                    .collect(),
            )
        })
        .unwrap_or_default()
}

fn save_usage(usage: &HashMap<String, u64>) {
    let Some(path) = usage_file() else { return };
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        log::error!("creating state directory failed: {err:#}");
        return;
    }
    let text = serde_json::to_string(&serde_json::json!({ "Applications": usage }));
    if let Ok(text) = text
        && let Err(err) = std::fs::write(&path, text)
    {
        log::error!("saving usage failed: {err:#}");
    }
}

fn record_usage(desktop_path: &str) {
    let mut usage = load_usage();
    *usage.entry(desktop_path.to_string()).or_insert(0) += 1;
    save_usage(&usage);
}

/// Semicolon-separated list value per the Desktop Entry spec: the trailing
/// semicolon is part of the syntax, so empty segments drop out.
fn show_in_list(value: &str) -> Vec<String> {
    value
        .trim()
        .split(';')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// glib's g_desktop_app_info_get_show_in, hand-rolled: walk the stacked
/// desktop values in order, OnlyShowIn consulted before NotShowIn per value,
/// and the tail hides whatever carries OnlyShowIn when nothing matched (an
/// unset or empty desktop hides every gated entry). Plain case-sensitive
/// equality, like the reference reader and the menu spec's registry
/// ("these are case-sensitive").
fn shows_in(only: Option<&[String]>, not: Option<&[String]>, desktop: Option<&str>) -> bool {
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

fn parse_desktop_entry(text: &str, desktop: Option<&str>) -> Option<AppEntry> {
    let mut name = None;
    let mut generic = None;
    let mut keywords = None;
    let mut exec = None;
    let mut icon = None;
    let mut terminal = false;
    let mut hidden = false;
    let mut only_show_in: Option<Vec<String>> = None;
    let mut not_show_in: Option<Vec<String>> = None;
    for line in text.lines() {
        if line.starts_with('[') && !line.starts_with("[Desktop Entry]") {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Name" => name = Some(value.trim().to_string()),
                "GenericName" => generic = Some(value.trim().to_string()),
                "Keywords" => keywords = Some(value.trim().to_string()),
                "Exec" => exec = Some(value.trim().to_string()),
                "Icon" => icon = Some(value.trim().to_string()),
                "Terminal" => terminal = value.trim() == "true",
                "NoDisplay" | "Hidden" => hidden = value.trim() == "true",
                "OnlyShowIn" => only_show_in = Some(show_in_list(value)),
                "NotShowIn" => not_show_in = Some(show_in_list(value)),
                _ => {}
            }
        }
    }
    if hidden {
        return None;
    }
    if !shows_in(only_show_in.as_deref(), not_show_in.as_deref(), desktop) {
        return None;
    }
    let exec = exec?;
    Some(AppEntry {
        icon: icon.unwrap_or_default().to_lowercase(),
        name: name.unwrap_or_else(|| "Unnamed".to_string()),
        generic: generic.unwrap_or_default(),
        keywords: keywords.unwrap_or_default(),
        desktop_path: String::new(),
        usage: 0,
        exec: exec
            .split_whitespace()
            .filter(|token| !token.starts_with('%'))
            .collect::<Vec<_>>()
            .join(" "),
        terminal,
    })
}

/// Subsequence match with bonuses for consecutive runs and word boundaries,
/// fzf-style: higher is better, None = no match.
fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let query: Vec<char> = query.chars().collect();
    let text: Vec<char> = text.chars().collect();
    if query.is_empty() || text.is_empty() || query.len() > text.len() {
        return if query.is_empty() { Some(0) } else { None };
    }

    let mut score = 0;
    let mut text_index = 0;
    for &needle in &query {
        let mut found = None;
        while text_index < text.len() {
            let candidate = text[text_index];
            text_index += 1;
            if candidate == needle {
                found = Some(candidate);
                break;
            }
            score -= 1;
        }
        let _matched = found?;
        // boundary = start of text or right after a separator in the text itself
        let boundary = text_index <= 1 || matches!(text[text_index - 2], ' ' | '-' | '_' | '.');
        score += if boundary { 8 } else { 2 };
        score += 1;
    }
    // prefer matches closer to the start
    score += (text.len() - text_index).min(10) as i32 * -1;
    Some(score)
}

/// The launcher's calculator: a query that is entirely one arithmetic
/// expression evaluates to a result row. Anything else (letters,
/// syntax errors, non-finite results) returns None and the query
/// stays a plain app search.
fn calc_result(query: &str) -> Option<String> {
    // a long input is never arithmetic worth evaluating; cap the work
    if query.chars().count() > 64 {
        return None;
    }
    let tokens = tokenize(query)?;
    // a bare number ("1234") is an app search, not a calculation:
    // an operator is what makes an expression
    if !tokens.iter().any(|token| matches!(token, Token::Op(_))) {
        return None;
    }
    let mut parser = Parser { tokens, position: 0 };
    let value = parser.expression(0)?;
    if parser.position != parser.tokens.len() {
        // the parse stopped short of the end ("2 3", "2+2("), so the
        // query was not one expression
        return None;
    }
    // division by zero, 0 % 0, and overflow all land here: the row
    // simply never appears, rather than showing a wrong answer
    if !value.is_finite() {
        return None;
    }
    Some(format_value(value))
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(f64),
    Op(char),
    LeftParen,
    RightParen,
}

fn tokenize(query: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = query.chars().peekable();
    while let Some(&ch) = chars.peek() {
        match ch {
            ' ' => {
                chars.next();
            }
            '+' | '-' | '*' | '/' | '%' | '^' => {
                tokens.push(Token::Op(ch));
                chars.next();
            }
            '(' => {
                tokens.push(Token::LeftParen);
                chars.next();
            }
            ')' => {
                tokens.push(Token::RightParen);
                chars.next();
            }
            digit if digit.is_ascii_digit() || digit == '.' => {
                let mut number = String::new();
                let mut dots = 0;
                while let Some(&ch) = chars.peek() {
                    if ch.is_ascii_digit() || (ch == '.' && dots == 0) {
                        if ch == '.' {
                            dots += 1;
                        }
                        number.push(ch);
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Number(number.parse().ok()?));
            }
            _ => return None,
        }
    }
    Some(tokens)
}

/// Precedence climbing over the tokens. `min_binding` is the operator
/// precedence an iteration must clear to consume: `^` binds tighter
/// than `* / %`, which bind tighter than `+ -`, and `^` is right
/// associative, so 2^3^2 is 2^(3^2).
struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn expression(&mut self, min_binding: u8) -> Option<f64> {
        let mut lhs = self.primary()?;
        while let Some(Token::Op(op)) = self.peek().cloned() {
            let (binding, right_binding) = match op {
                '+' | '-' => (2, 3),
                '*' | '/' | '%' => (3, 4),
                '^' => (4, 4),
                // the tokenizer mints no other operators
                _ => break,
            };
            if binding < min_binding {
                break;
            }
            self.position += 1;
            let rhs = self.expression(right_binding)?;
            lhs = match op {
                '+' => lhs + rhs,
                '-' => lhs - rhs,
                '*' => lhs * rhs,
                '/' => lhs / rhs,
                '%' => lhs % rhs,
                '^' => lhs.powf(rhs),
                // the tokenizer mints no other operators
                _ => break,
            };
        }
        Some(lhs)
    }

    /// A number, a parenthesized expression, or a negation. The
    /// negation recurses at `^`'s binding so -2^2 is -(2^2), the
    /// written-math convention.
    fn primary(&mut self) -> Option<f64> {
        match self.tokens.get(self.position)? {
            Token::Number(value) => {
                self.position += 1;
                Some(*value)
            }
            Token::LeftParen => {
                self.position += 1;
                let value = self.expression(0)?;
                match self.tokens.get(self.position)? {
                    Token::RightParen => {
                        self.position += 1;
                        Some(value)
                    }
                    _ => None,
                }
            }
            Token::Op('-') => {
                self.position += 1;
                Some(-self.expression(4)?)
            }
            _ => None,
        }
    }
}

/// Whole values print without the decimal tail (2.5 + 2.5 is "5", not
/// "5.0"); fractional values print as Rust's shortest round-trip form,
/// which keeps 10/4 honest at "2.5".
fn format_value(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

pub fn launch(exec: &str, terminal: bool) -> anyhow::Result<()> {
    use std::process::Command;

    let child = if terminal {
        // terminal-type desktop entries open in kuma-term; the exec
        // string rides KUMA_TERM_COMMAND (kuma-term runs it as
        // /bin/sh -c) instead of a terminal emulator flag
        Command::new("kuma-term")
            .env("KUMA_TERM_COMMAND", exec)
            .spawn()?
    } else {
        Command::new("sh").arg("-c").arg(exec).spawn()?
    };
    scope_and_reap(child, exec);
    Ok(())
}

/// Reap a spawned app on a detached thread: a dropped `Child` never
/// gets wait()ed, so its zombie would sit in the process table until
/// the shell exits (one per app launch).
pub fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Scope an app into its own transient unit, then reap it. Apps the
/// shell launches must not share the shell's cgroup: the service runs
/// with KillMode=control-group, so `systemctl --user restart
/// kuma-shell` would otherwise SIGTERM every open app along with the
/// shell. The scope (named like niri's, `app-kuma-shell-<prog>-<pid>`
/// in app.slice) survives shell restarts. The move races the child's
/// exit, which is fine: a dead pid fails the scope call and the empty
/// unit is collected, and a failed scoping never fails the launch.
fn scope_and_reap(mut child: std::process::Child, exec: &str) {
    let pid = child.id();
    let name = scope_name(exec, pid);
    std::thread::spawn(move || {
        // still running? (a fast exit skips the scope call entirely)
        if matches!(child.try_wait(), Ok(None)) {
            if let Err(err) = scope_app(pid, &name) {
                log::warn!("scoping {name} failed, app runs in the shell's cgroup: {err}");
            }
        }
        let _ = child.wait();
    });
}

/// `app-kuma-shell-<program>-<pid>.scope`, mirroring niri's
/// `app-niri-<program>-<pid>` scopes: the program is the exec line's
/// first token's basename, with anything outside [A-Za-z0-9-] folded
/// to a dash.
fn scope_name(exec: &str, pid: u32) -> String {
    let first = exec.split_whitespace().next().unwrap_or("app");
    let basename = first.rsplit('/').next().unwrap_or(first);
    let prog: String = basename
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    format!("app-kuma-shell-{prog}-{pid}.scope")
}

/// Move a running pid into a transient scope over systemd's D-Bus
/// API (the same StartTransientUnit call niri and GNOME shell make;
/// no systemd-run process needed).
fn scope_app(pid: u32, name: &str) -> anyhow::Result<()> {
    use zbus::zvariant::{Array, Value};

    let conn = zbus::blocking::Connection::session()?;
    let systemd = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )?;
    let properties: Vec<(&str, Value)> = vec![
        ("PIDs", Value::Array(Array::from(vec![pid]))),
        ("Slice", Value::Str("app.slice".into())),
    ];
    let aux: Vec<(&str, Vec<(&str, Value)>)> = Vec::new();
    let _job: zbus::zvariant::OwnedObjectPath =
        systemd.call("StartTransientUnit", &(name, "fail", properties, aux))?;
    Ok(())
}

pub struct LauncherView {
    focus_handle: FocusHandle,
    geometry: crate::panel::PanelGeometry,
    apps: Vec<AppEntry>,
    icons: HashMap<String, Option<IconImage>>,
    results_scroll: gpui::ScrollHandle,
    query: String,
    selected: usize,
}

/// The apps matching the query, best first: fuzzy score with the
/// generic-name and keyword fallbacks penalized, then name order.
fn filtered_indices(apps: &[AppEntry], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return (0..apps.len()).collect();
    }
    let mut scored: Vec<(i32, usize)> = apps
        .iter()
        .enumerate()
        .filter_map(|(index, app)| {
            // name hits rank first; generic name and keywords also
            // match, with a penalty so "Koguma" beats "File Manager"
            fuzzy_score(&query, &app.name.to_lowercase())
                .or_else(|| {
                    fuzzy_score(&query, &app.generic.to_lowercase()).map(|score| score - 20)
                })
                .or_else(|| {
                    fuzzy_score(&query, &app.keywords.to_lowercase()).map(|score| score - 30)
                })
                .map(|score| (score, index))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| apps[a.1].name.to_lowercase().cmp(&apps[b.1].name.to_lowercase()))
    });
    scored.into_iter().map(|(_, index)| index).collect()
}

/// How many most-used apps the idle list pins under Frequent.
const FREQUENT_LIMIT: usize = 4;

/// One entry of the visible list: a section marker, an app row, or a
/// calculator result.
#[derive(Clone, Debug, PartialEq)]
enum LauncherItem {
    Section(&'static str),
    /// The app's index in the loaded list.
    Row(usize),
    /// The evaluated query: copied to the clipboard on Enter.
    Calc(String),
}

/// The visible list: while idle, the most-used apps pinned under
/// Frequent ahead of the catalog; while a query is active, one ranked
/// list with the sections flattened away, led by the calculator's
/// result when the query evaluates (an expression is an answer, not a
/// search).
fn sectioned_items(apps: &[AppEntry], query: &str) -> Vec<LauncherItem> {
    if !query.is_empty() {
        let mut items = Vec::new();
        if let Some(result) = calc_result(query) {
            items.push(LauncherItem::Calc(result));
        }
        items.extend(
            filtered_indices(apps, query)
                .into_iter()
                .map(LauncherItem::Row),
        );
        return items;
    }
    let frequent: Vec<usize> = (0..apps.len())
        .filter(|&index| apps[index].usage > 0)
        .take(FREQUENT_LIMIT)
        .collect();
    let mut items = Vec::new();
    if !frequent.is_empty() {
        items.push(LauncherItem::Section("Frequent"));
        items.extend(frequent.iter().copied().map(LauncherItem::Row));
    }
    items.push(LauncherItem::Section("All applications"));
    items.extend(
        (0..apps.len())
            .filter(|index| !frequent.contains(index))
            .map(LauncherItem::Row),
    );
    items
}

impl LauncherView {
    pub fn new(
        apps: Vec<AppEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
        geometry: crate::panel::PanelGeometry,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let icon_keys: Vec<String> = apps.iter().map(|app| app.icon.clone()).collect();
        cx.spawn(async move |this, cx| {
            // the registry makes re-opens a map lookup and pins one atlas
            // tile per icon; these Arcs are shared with the dock's (and the
            // grid's icons decode at the grid's size, not the theme's)
            let decoded: Vec<(String, Option<IconImage>)> = cx
                .background_spawn(async move {
                    icon_keys
                        .into_iter()
                        .filter(|key| !key.is_empty())
                        .map(|key| {
                            let icon = crate::imaging::resolve(&key, ICON_RESOLVE_SIZE)
                                .map(|shared| shared.clone_shared());
                            (key, icon)
                        })
                        .collect()
                })
                .await;
            for (key, icon) in decoded {
                let _ = this.update(cx, |this, cx| {
                    this.icons.insert(key, icon);
                    cx.notify();
                });
            }
        })
        .detach();
        Self {
            focus_handle,
            geometry,
            apps,
            icons: HashMap::new(),
            results_scroll: gpui::ScrollHandle::new(),
            query: String::new(),
            selected: 0,
        }
    }

    fn launch_at(&mut self, app_index: usize, cx: &mut Context<Self>) {
        if let Some(app) = self.apps.get(app_index) {
            record_usage(&app.desktop_path);
            if let Err(err) = launch(&app.exec, app.terminal) {
                log::error!("launching {:?} failed: {err:#}", app.name);
                return;
            }
        }
        crate::panel::close_panels(cx);
    }

    /// Activate the selected row: an app launches (usage recorded, a
    /// failed launch leaves the panel open), a calculator result
    /// copies. The usage file stays desktop-path keyed, so a calc
    /// activation never counts as anything.
    fn activate(&mut self, item: LauncherItem, cx: &mut Context<Self>) {
        match item {
            LauncherItem::Calc(result) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(result));
            }
            LauncherItem::Row(app_index) => self.launch_at(app_index, cx),
            LauncherItem::Section(_) => {}
        }
        crate::panel::close_panels(cx);
    }

    /// The visible rows in selection order, each with its child index
    /// in the scrolled list: section markers sit between the rows, so
    /// scrolling targets the child, not the row number.
    fn rows(&self) -> Vec<(LauncherItem, usize)> {
        self.items()
            .into_iter()
            .enumerate()
            .filter_map(|(child, item)| match item {
                LauncherItem::Section(_) => None,
                _ => Some((item, child)),
            })
            .collect()
    }

    fn items(&self) -> Vec<LauncherItem> {
        sectioned_items(&self.apps, &self.query)
    }

    fn scroll_selected_into_view(&self, rows: &[(LauncherItem, usize)]) {
        if let Some((_, child)) = rows.get(self.selected) {
            self.results_scroll.scroll_to_item(*child);
        }
    }

    fn handle_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows();
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                // a live filter clears first (kuma-files' convention);
                // only an idle query lets Esc bubble to chrome, which
                // closes the panel
                if !self.query.is_empty() {
                    self.query.clear();
                    self.selected = 0;
                    self.results_scroll.set_offset(point(px(0.), px(0.)));
                    cx.notify();
                    cx.stop_propagation();
                }
            }
            "enter" => {
                if let Some((item, _)) = rows.get(self.selected) {
                    self.activate(item.clone(), cx);
                }
            }
            "up" => {
                if rows.is_empty() {
                    return;
                }
                self.selected = if self.selected == 0 {
                    rows.len() - 1
                } else {
                    self.selected - 1
                };
                self.scroll_selected_into_view(&rows);
                cx.notify();
            }
            "down" => {
                if rows.is_empty() {
                    return;
                }
                self.selected = (self.selected + 1) % rows.len();
                self.scroll_selected_into_view(&rows);
                cx.notify();
            }
            "backspace" => {
                if self.query.pop().is_some() {
                    self.selected = 0;
                    self.results_scroll.set_offset(point(px(0.), px(0.)));
                    cx.notify();
                }
            }
            "space" => {
                self.query.push(' ');
                self.selected = 0;
                self.results_scroll.set_offset(point(px(0.), px(0.)));
                cx.notify();
            }
            other if other.chars().count() == 1 && !event.keystroke.modifiers.modified() => {
                self.query.push_str(other);
                self.selected = 0;
                self.results_scroll.set_offset(point(px(0.), px(0.)));
                cx.notify();
            }
            _ => {}
        }
    }
}

impl Focusable for LauncherView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LauncherView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let items = self.items();
        let rows_count = items
            .iter()
            .filter(|item| !matches!(item, LauncherItem::Section(_)))
            .count();
        // the count's noun: with a calc row in the list the entries
        // are no longer all apps
        let noun = if items
            .iter()
            .any(|item| matches!(item, LauncherItem::Calc(_)))
        {
            if rows_count == 1 { "result" } else { "results" }
        } else {
            "apps"
        };
        let selected = self.selected;
        // the rows are built up front: the hover listeners mint here,
        // before chrome borrows cx
        let mut position = 0;
        let list: Vec<gpui::AnyElement> = items
            .into_iter()
            .map(|item| match item {
                LauncherItem::Section(label) => section_header(label).into_any_element(),
                LauncherItem::Calc(result) => {
                    let at = position;
                    position += 1;
                    // the pointer and the keyboard share one selection:
                    // hovering a row moves the selection to it
                    let hover = cx.listener(
                        move |this: &mut Self, _: &gpui::MouseMoveEvent, _, cx| {
                            if this.selected != at {
                                this.selected = at;
                                cx.notify();
                            }
                        },
                    );
                    calc_row(self.query.clone(), result, at == selected, hover)
                        .into_any_element()
                }
                LauncherItem::Row(app_index) => {
                    let at = position;
                    position += 1;
                    let app = self.apps[app_index].clone();
                    let icon = self.icons.get(&app.icon).cloned().flatten();
                    // the pointer and the keyboard share one selection:
                    // hovering a row moves the selection to it
                    let hover = cx.listener(
                        move |this: &mut Self, _: &gpui::MouseMoveEvent, _, cx| {
                            if this.selected != at {
                                this.selected = at;
                                cx.notify();
                            }
                        },
                    );
                    app_row(app, app_index, at == selected, icon, hover).into_any_element()
                }
            })
            .collect();
        let query = self.query.clone();
        let key_handler = cx.listener(Self::handle_key);
        let clear = cx.listener(move |this, _, _, cx| {
            this.query.clear();
            this.selected = 0;
            this.results_scroll.set_offset(point(px(0.), px(0.)));
            cx.notify();
        });
        crate::panel::chrome(
            self.geometry,
            window,
            cx,
            div()
                .id("launcher")
                .size_full()
                .flex()
                .flex_col()
                .track_focus(&self.focus_handle)
                .on_key_down(key_handler)
                .px(px(16.))
                .pb(px(12.))
                .pt(px(10.))
                .gap_2()
                .child(
                    kit::pane_header("Applications").when(rows_count > 0, |header| {
                        header.child(kit::count_badge(rows_count))
                    }),
                )
                // the kuma-files filter banner: no field at rest, typing
                // filters straight away (this panel holds the keyboard),
                // and the banner only explains a live filter
                .when(!query.is_empty(), |el| {
                    el.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(crate::theme::current().text_dim))
                                    .child("Search"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(crate::theme::current().text))
                                    .child(query.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(crate::theme::current().text_dim))
                                    .child(format!("· {rows_count} {noun}")),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(crate::theme::current().text_dim))
                                    .child("Esc clears"),
                            )
                            .child(kit::icon_button(
                                "launcher-clear",
                                "icons/x.svg",
                                kit::ButtonVariant::Ghost,
                                clear,
                            )),
                    )
                })
                .child(
                    div()
                        .id("results")
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .overflow_y_scroll()
                        .track_scroll(&self.results_scroll)
                        .when(rows_count == 0, |el| {
                            el.child(kit::empty_state(
                                "icons/search.svg",
                                "No apps match",
                                "Try a shorter or looser name",
                            ))
                        })
                        .children(list),
                ),
        )
    }
}

/// A section marker inside the list: the label and a hairline running
/// to the pane's right edge (the wifi panel's section row, quieter).
fn section_header(label: &'static str) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .pt_1()
        .child(
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(crate::theme::current().text_dim))
                .child(label.to_string()),
        )
        .child(
            div()
                .h(px(1.))
                .flex_1()
                .bg(rgba(crate::theme::current().divider_soft)),
        )
}

fn app_row(
    app: AppEntry,
    app_index: usize,
    is_selected: bool,
    icon: Option<IconImage>,
    on_hover: impl Fn(&gpui::MouseMoveEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id(SharedString::from(format!("app-{app_index}")))
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .rounded_lg()
        // the wifi row's fills: inset at rest, surface when selected,
        // hover brightens; no naked rows on the drawer
        .bg(rgb(if is_selected {
            crate::theme::current().surface
        } else {
            crate::theme::current().inset
        }))
        .on_mouse_move(on_hover)
        .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
            let _ = launch(&app.exec, app.terminal);
            crate::panel::close_panels(cx);
        })
        .hover(|style| style.bg(rgb(crate::theme::current().surface_hover)))
        .child(
            // the glyph sits directly on the row (no tile), like wifi's
            // signal glyph, tinting accent when the row is selected
            div()
                .w(px(24.))
                .flex()
                .justify_center()
                .overflow_hidden()
                .when_some(icon.clone(), |el, icon| match icon {
                    IconImage::Raster(raster) => el.child(
                        img(gpui::ImageSource::Render(raster))
                            .object_fit(ObjectFit::Cover)
                            .w(px(22.))
                            .h(px(22.)),
                    ),
                    IconImage::Svg(bytes) => el.child(
                        gpui::svg()
                            .data(&bytes)
                            .size(px(18.))
                            .text_color(rgb(if is_selected {
                                crate::theme::current().accent
                            } else {
                                crate::theme::current().text
                            })),
                    ),
                })
                .when_none(&icon, |el| {
                    el.text_size(px(12.5))
                        .text_color(rgb(if is_selected {
                            crate::theme::current().accent
                        } else {
                            crate::theme::current().text
                        }))
                        .child(app.name.chars().next().unwrap_or('?').to_string())
                }),
        )
        .child(
            // the kit's row shape: the name over its dim note (the
            // desktop entry's GenericName, when it has one)
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_weight(if is_selected {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::NORMAL
                        })
                        .text_color(rgb(crate::theme::current().text))
                        .truncate()
                        .child(app.name.clone()),
                )
                .when(!app.generic.is_empty(), |el| {
                    el.child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(crate::theme::current().text_dim))
                            .truncate()
                            .child(app.generic.clone()),
                    )
                }),
        )
        // trailing slot: the selected row shows the Enter affordance,
        // unselected terminal apps show where a label would crowd
        .when(is_selected, |el| {
            el.child(
                gpui::svg()
                    .path("icons/enter.svg")
                    .size(px(11.))
                    .text_color(rgb(crate::theme::current().accent)),
            )
        })
        .when(!is_selected && app.terminal, |el| {
            el.child(
                gpui::svg()
                    .path("icons/terminal.svg")
                    .size(px(11.))
                    .text_color(rgb(crate::theme::current().text_dim)),
            )
        })
}

/// The calculator's row, the app row's shape: result up front, the
/// query it came from beneath, and the Enter affordance on selection.
/// Click and Enter do the same thing: the result lands on the
/// clipboard and the panel closes, the same dismissal a launch gets.
fn calc_row(
    query: String,
    result: String,
    is_selected: bool,
    on_hover: impl Fn(&gpui::MouseMoveEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id("calc")
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .rounded_lg()
        // the app row's fills: inset at rest, surface when selected
        .bg(rgb(if is_selected {
            crate::theme::current().surface
        } else {
            crate::theme::current().inset
        }))
        .on_mouse_move(on_hover)
        .on_mouse_down(gpui::MouseButton::Left, {
            // the row hands its own copy to the click; the live one
            // stays for the "= result" line
            let clipboard = result.clone();
            move |_, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(clipboard.clone()));
                crate::panel::close_panels(cx);
            }
        })
        .hover(|style| style.bg(rgb(crate::theme::current().surface_hover)))
        .child(
            // the glyph sits directly on the row, tinting accent when
            // the row is selected, like the app rows' icons
            div()
                .w(px(24.))
                .flex()
                .justify_center()
                .child(
                    gpui::svg()
                        .path("icons/calc.svg")
                        .size(px(18.))
                        .text_color(rgb(if is_selected {
                            crate::theme::current().accent
                        } else {
                            crate::theme::current().text
                        })),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_weight(if is_selected {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::NORMAL
                        })
                        .text_color(rgb(crate::theme::current().text))
                        .truncate()
                        .child(format!("= {result}")),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(crate::theme::current().text_dim))
                        .truncate()
                        .child(format!("Enter copies · {query}")),
                ),
        )
        .when(is_selected, |el| {
            el.child(
                gpui::svg()
                    .path("icons/enter.svg")
                    .size(px(11.))
                    .text_color(rgb(crate::theme::current().accent)),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_scores_start_over_midword() {
        let umbrella = fuzzy_score("um", "umbrella");
        let volume = fuzzy_score("um", "volume");
        assert!(umbrella.unwrap() > volume.unwrap());
    }

    #[test]
    fn scope_names_follow_the_niri_shape() {
        // plain name, path binary, arguments, odd characters: the
        // program is the exec's first token's basename, sanitized
        assert_eq!(
            scope_name("kuma-term", 42),
            "app-kuma-shell-kuma-term-42.scope"
        );
        assert_eq!(
            scope_name("/usr/bin/kuma-files --hidden", 7),
            "app-kuma-shell-kuma-files-7.scope"
        );
        assert_eq!(
            scope_name("org.foo.Bar --flag=x", 9),
            "app-kuma-shell-org-foo-Bar-9.scope"
        );
        // no exec text at all still yields a valid unique name
        assert_eq!(scope_name("", 5), "app-kuma-shell-app-5.scope");
    }

    #[test]
    fn fuzzy_requires_subsequence() {
        assert!(fuzzy_score("xyz", "firefox").is_none());
        assert!(fuzzy_score("", "anything").is_some());
    }

    #[test]
    fn fuzzy_prefers_word_starts() {
        let start = fuzzy_score("s", "steam");
        let later = fuzzy_score("s", "obsidian");
        assert!(start.unwrap() > later.unwrap());
    }

    #[test]
    fn desktop_entry_parses() {
        let entry = parse_desktop_entry(
            "[Desktop Entry]\nName=GNU Image\nExec=gimp-3.0 %U\nIcon=gimp\nTerminal=false\nType=Application\n",
            None,
        )
        .unwrap();
        assert_eq!(entry.name, "GNU Image");
        assert_eq!(entry.exec, "gimp-3.0"); // field codes stripped
        assert!(!entry.terminal);
        assert_eq!(entry.icon, "gimp");
    }

    #[test]
    fn desktop_entry_hides_no_display() {
        let text = "[Desktop Entry]\nName=X\nExec=x\nNoDisplay=true\n";
        assert!(parse_desktop_entry(text, None).is_none());
    }

    #[test]
    fn desktop_entry_hides_other_desktops() {
        // the live find: blueman-adapters gates itself to XFCE;MATE
        let text = "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=XFCE;MATE;\n";
        assert!(parse_desktop_entry(text, Some("niri")).is_none());
        assert!(parse_desktop_entry(
            "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=niri\n",
            Some("niri"),
        )
        .is_some());
        // no gating keys at all: the common case, shown on any desktop
        assert!(parse_desktop_entry("[Desktop Entry]\nName=X\nExec=x\n", Some("niri")).is_some());
        // case-sensitive, like g_str_equal: no folding
        assert!(parse_desktop_entry(
            "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=NIRI\n",
            Some("niri"),
        )
        .is_none());
    }

    #[test]
    fn desktop_entry_not_show_in_hides() {
        assert!(parse_desktop_entry(
            "[Desktop Entry]\nName=X\nExec=x\nNotShowIn=niri\n",
            Some("niri"),
        )
        .is_none());
        assert!(parse_desktop_entry(
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
        assert!(parse_desktop_entry(
            "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=niri\nNotShowIn=niri\n",
            Some("niri"),
        )
        .is_some());
        // stacked values: any OnlyShowIn match shows, any NotShowIn match hides
        assert!(parse_desktop_entry(
            "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=niri\n",
            Some("xfce:niri"),
        )
        .is_some());
        assert!(parse_desktop_entry(
            "[Desktop Entry]\nName=X\nExec=x\nNotShowIn=xfce\n",
            Some("xfce:niri"),
        )
        .is_none());
    }

    #[test]
    fn desktop_entry_no_display_wins_over_show_in() {
        assert!(parse_desktop_entry(
            "[Desktop Entry]\nName=X\nExec=x\nNoDisplay=true\nOnlyShowIn=niri\n",
            Some("niri"),
        )
        .is_none());
    }

    #[test]
    fn desktop_entry_hides_hidden() {
        // Hidden=true means the user deleted the entry; same arm as NoDisplay
        assert!(parse_desktop_entry(
            "[Desktop Entry]\nName=X\nExec=x\nHidden=true\nOnlyShowIn=niri\n",
            Some("niri"),
        )
        .is_none());
    }

    #[test]
    fn desktop_entry_gated_entry_hides_without_a_desktop() {
        // glib's tail: only_show_in != NULL with no desktop means hidden
        assert!(parse_desktop_entry("[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=XFCE\n", None)
            .is_none());
        assert!(parse_desktop_entry("[Desktop Entry]\nName=X\nExec=x\n", None).is_some());
    }

    #[test]
    fn desktop_entry_captures_icon() {
        let entry = parse_desktop_entry(
            "[Desktop Entry]\nName=Files\nExec=nautilus %U\nIcon=org.gnome.Nautilus\n",
            None,
        )
        .unwrap();
        assert_eq!(entry.icon, "org.gnome.nautilus");
    }

    #[test]
    fn desktop_entry_captures_generic_and_keywords() {
        let entry = parse_desktop_entry(
            "[Desktop Entry]\nName=Koguma\nGenericName=File Manager\nKeywords=files;folders;\nExec=kuma-files\n",
            None,
        )
        .unwrap();
        assert_eq!(entry.generic, "File Manager");
        assert_eq!(entry.keywords, "files;folders;");
        // entries without them just score empty, never panic
        let bare = parse_desktop_entry("[Desktop Entry]\nName=X\nExec=x\n", None).unwrap();
        assert_eq!(bare.generic, "");
        assert_eq!(bare.keywords, "");
    }

    #[test]
    fn keyword_match_ranks_below_name_match() {
        let koguma = AppEntry {
            name: "Koguma".into(),
            generic: "File Manager".into(),
            keywords: "files;folders;".into(),
            ..parse_desktop_entry("[Desktop Entry]\nName=x\nExec=x\n", None).unwrap()
        };
        let mut renamed = koguma.clone();
        renamed.name = "Archive Files".into();
        // "file" hits all three fields: the name match outranks the
        // generic-name match, which outranks keywords
        let query = "file";
        let by_name = fuzzy_score(query, &renamed.name.to_lowercase()).unwrap();
        let by_generic = fuzzy_score(query, &koguma.generic.to_lowercase()).map(|s| s - 20);
        let by_keywords = fuzzy_score(query, &koguma.keywords.to_lowercase()).map(|s| s - 30);
        assert!(by_generic.unwrap() > by_keywords.unwrap());
        assert!(by_name > by_generic.unwrap());
        // "files" is not a subsequence of "file manager" (manager has
        // no s); the keywords field is what catches the plural
        assert!(fuzzy_score("files", &koguma.generic.to_lowercase()).is_none());
        assert!(fuzzy_score("files", &koguma.keywords.to_lowercase()).is_some());
    }

    fn entry(name: &str, usage: u64) -> AppEntry {
        AppEntry {
            usage,
            ..parse_desktop_entry(&format!("[Desktop Entry]\nName={name}\nExec={name}\n"), None)
                .unwrap()
        }
    }

    #[test]
    fn idle_list_pins_frequent_apps_ahead_of_the_catalog() {
        let apps = vec![
            entry("Zed", 9),
            entry("Amboss", 4),
            entry("Files", 0),
            entry("Terminal", 0),
        ];
        let items = sectioned_items(&apps, "");
        // Frequent first: the two used apps, then the catalog section
        assert_eq!(
            items[..4],
            [
                LauncherItem::Section("Frequent"),
                LauncherItem::Row(0),
                LauncherItem::Row(1),
                LauncherItem::Section("All applications"),
            ]
        );
        // the catalog skips the pinned apps, so nothing repeats
        assert_eq!(
            items[4..],
            [LauncherItem::Row(2), LauncherItem::Row(3)]
        );
    }

    #[test]
    fn idle_list_without_usage_is_one_catalog() {
        let apps = vec![entry("Files", 0), entry("Terminal", 0)];
        assert_eq!(
            sectioned_items(&apps, ""),
            [
                LauncherItem::Section("All applications"),
                LauncherItem::Row(0),
                LauncherItem::Row(1),
            ]
        );
    }

    #[test]
    fn a_query_flattens_the_sections_away() {
        let apps = vec![entry("Files", 5), entry("Terminal", 0)];
        assert_eq!(
            sectioned_items(&apps, "fil"),
            [LauncherItem::Row(0)]
        );
    }

    #[test]
    fn calc_evaluates_arithmetic_queries() {
        assert_eq!(calc_result("12*34").as_deref(), Some("408"));
        // precedence: multiplication before addition
        assert_eq!(calc_result("2+3*4").as_deref(), Some("14"));
        assert_eq!(calc_result("10/4").as_deref(), Some("2.5"));
        assert_eq!(calc_result("2^10").as_deref(), Some("1024"));
        assert_eq!(calc_result("(1+2)*3").as_deref(), Some("9"));
        assert_eq!(calc_result("7%3").as_deref(), Some("1"));
        assert_eq!(calc_result("1.5+1").as_deref(), Some("2.5"));
        // written-math convention: the negation binds looser than ^
        assert_eq!(calc_result("-2^2").as_deref(), Some("-4"));
        // ^ is right associative
        assert_eq!(calc_result("2^3^2").as_deref(), Some("512"));
        // spaces ride along
        assert_eq!(calc_result("1 + 2").as_deref(), Some("3"));
    }

    #[test]
    fn calc_rejects_non_expressions() {
        // a bare number is an app search, not a calculation
        assert_eq!(calc_result("1234"), None);
        assert_eq!(calc_result("1.5"), None);
        // syntax errors: the row just never appears
        assert_eq!(calc_result("2+"), None);
        assert_eq!(calc_result("*2"), None);
        assert_eq!(calc_result("2 3"), None);
        assert_eq!(calc_result("2+2("), None);
        assert_eq!(calc_result("1.2.3"), None);
        // letters make it an app search again
        assert_eq!(calc_result("obs"), None);
        assert_eq!(calc_result("2b"), None);
        // non-finite results (division by zero, overflow) hide the row
        assert_eq!(calc_result("1/0"), None);
        assert_eq!(calc_result("9^9^9"), None);
        // beyond the size cap
        let long = "1+".repeat(40) + "1";
        assert_eq!(calc_result(&long), None);
    }

    #[test]
    fn a_calc_query_leads_the_result_list() {
        // the app's name doubles as an expression, so both rows fire
        let apps = vec![entry("1+1", 0)];
        assert_eq!(
            sectioned_items(&apps, "1+1"),
            [LauncherItem::Calc("2".into()), LauncherItem::Row(0)]
        );
        // an expression that matches no app stands alone
        assert_eq!(
            sectioned_items(&apps, "2+2"),
            [LauncherItem::Calc("4".into())]
        );
    }

    #[test]
    fn calc_never_appears_without_a_query() {
        let apps = vec![entry("Files", 0)];
        assert!(!sectioned_items(&apps, "")
            .iter()
            .any(|item| matches!(item, LauncherItem::Calc(_))));
    }
}
