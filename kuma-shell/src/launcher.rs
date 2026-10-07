use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    App, Context, Div, FocusHandle, Focusable, FontWeight, KeyDownEvent, ObjectFit, Render,
    SharedString, Window, div, img, point, prelude::*, px, rgb, rgba,
};

use crate::imaging::{IconImage, decode_icon_file, icon_roots};
use crate::panel_kit as kit;

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
                if let Some(mut app) = parse_desktop_entry(&text) {
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

fn parse_desktop_entry(text: &str) -> Option<AppEntry> {
    let mut name = None;
    let mut generic = None;
    let mut keywords = None;
    let mut exec = None;
    let mut icon = None;
    let mut terminal = false;
    let mut no_display = false;
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
                "NoDisplay" => no_display = value.trim() == "true",
                _ => {}
            }
        }
    }
    if no_display {
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

pub fn launch(exec: &str, terminal: bool) -> anyhow::Result<()> {
    use std::process::Command;

    let child = if terminal {
        Command::new("kitty")
            .arg("-e")
            .arg("sh")
            .arg("-c")
            .arg(exec)
            .spawn()?
    } else {
        Command::new("sh").arg("-c").arg(exec).spawn()?
    };
    reap(child);
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

/// One entry of the visible list: a section marker or an app row.
#[derive(Clone, Debug, PartialEq)]
enum LauncherItem {
    Section(&'static str),
    /// The app's index in the loaded list.
    Row(usize),
}

/// The visible list: while idle, the most-used apps pinned under
/// Frequent ahead of the catalog; while a query is active, one ranked
/// list with the sections flattened away.
fn sectioned_items(apps: &[AppEntry], query: &str) -> Vec<LauncherItem> {
    if !query.is_empty() {
        return filtered_indices(apps, query)
            .into_iter()
            .map(LauncherItem::Row)
            .collect();
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
            let index = cx
                .background_spawn(async move { crate::imaging::build_icon_index(&icon_roots()) })
                .await;
            for key in icon_keys {
                if key.is_empty() {
                    continue;
                }
                let decoded = index.get(&key).and_then(|path| decode_icon_file(path));
                let _ = this.update(cx, |this, cx| {
                    this.icons.insert(key, decoded);
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

    /// The visible rows in selection order, each with its child index
    /// in the scrolled list: section markers sit between the rows, so
    /// scrolling targets the child, not the row number.
    fn rows(&self) -> Vec<(usize, usize)> {
        self.items()
            .into_iter()
            .enumerate()
            .filter_map(|(child, item)| match item {
                LauncherItem::Row(app_index) => Some((app_index, child)),
                LauncherItem::Section(_) => None,
            })
            .collect()
    }

    fn items(&self) -> Vec<LauncherItem> {
        sectioned_items(&self.apps, &self.query)
    }

    fn scroll_selected_into_view(&self, rows: &[(usize, usize)]) {
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
                if let Some(&(app_index, _)) = rows.get(self.selected) {
                    self.launch_at(app_index, cx);
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
            .filter(|item| matches!(item, LauncherItem::Row(_)))
            .count();
        let selected = self.selected;
        // the rows are built up front: the hover listeners mint here,
        // before chrome borrows cx
        let mut position = 0;
        let list: Vec<gpui::AnyElement> = items
            .into_iter()
            .map(|item| match item {
                LauncherItem::Section(label) => section_header(label).into_any_element(),
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
                                    .child(format!("· {rows_count} apps")),
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
        assert!(parse_desktop_entry(text).is_none());
    }

    #[test]
    fn desktop_entry_captures_icon() {
        let entry = parse_desktop_entry(
            "[Desktop Entry]\nName=Files\nExec=nautilus %U\nIcon=org.gnome.Nautilus\n",
        )
        .unwrap();
        assert_eq!(entry.icon, "org.gnome.nautilus");
    }

    #[test]
    fn desktop_entry_captures_generic_and_keywords() {
        let entry = parse_desktop_entry(
            "[Desktop Entry]\nName=Koguma\nGenericName=File Manager\nKeywords=files;folders;\nExec=kuma-files\n",
        )
        .unwrap();
        assert_eq!(entry.generic, "File Manager");
        assert_eq!(entry.keywords, "files;folders;");
        // entries without them just score empty, never panic
        let bare = parse_desktop_entry("[Desktop Entry]\nName=X\nExec=x\n").unwrap();
        assert_eq!(bare.generic, "");
        assert_eq!(bare.keywords, "");
    }

    #[test]
    fn keyword_match_ranks_below_name_match() {
        let koguma = AppEntry {
            name: "Koguma".into(),
            generic: "File Manager".into(),
            keywords: "files;folders;".into(),
            ..parse_desktop_entry("[Desktop Entry]\nName=x\nExec=x\n").unwrap()
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
            ..parse_desktop_entry(&format!("[Desktop Entry]\nName={name}\nExec={name}\n"))
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
}
