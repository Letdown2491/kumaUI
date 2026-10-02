//! The signer's panel: the bunker's face, built like the shell's own
//! panels: a rail of tabs on the left, the pane's content on the
//! right, cards where a decision happens, glyphs wherever a word would
//! shout.
//!
//! The port of the noctalia plugin's panel, one surface over: the
//! rail (Requests, Paired apps, Activity, Pair), the ask cards with
//! their three answers (approve, approve for an hour, deny), the app
//! list whose cards are reads and whose detail view is where the acts
//! live, the activity log, and the Pair pane that mints and copies a
//! fresh URI. The daemon stays the policy, the CLI stays the
//! transport, and this file is only the shape the answers wear.

use std::collections::{HashMap, HashSet};

use chrono::Local;
use gpui::{
    AppContext, ClickEvent, Context, Div, Entity, FontWeight, IntoElement, ObjectFit, Render,
    SharedString, Window, div, img, prelude::*, px, rgb,
};

use crate::imaging::IconImage;
use crate::nostr::{self, NostrState, PairedApp, Prompt, VaultFact};
use crate::panel::PanelGeometry;
use crate::panel_kit::{self as kit, ButtonVariant};
use crate::theme::*;

/// The pane's poll cadence while the panel is open: an ask arriving
/// mid-read belongs in the list the person is looking at, not behind a
/// close-and-reopen. The loop dies with the view; a closed panel
/// polls nothing, which is exactly the frame-tick contract it ports.
const POLL: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Asks,
    Apps,
    Log,
    Pair,
}

impl Tab {
    fn key(self) -> &'static str {
        match self {
            Tab::Asks => "asks",
            Tab::Apps => "apps",
            Tab::Log => "log",
            Tab::Pair => "pair",
        }
    }

    fn from_key(key: &str) -> Option<Tab> {
        match key {
            "asks" => Some(Tab::Asks),
            "apps" => Some(Tab::Apps),
            "log" => Some(Tab::Log),
            "pair" => Some(Tab::Pair),
            _ => None,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Tab::Asks => "Requests",
            Tab::Apps => "Paired apps",
            Tab::Log => "Activity",
            Tab::Pair => "Pair",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Tab::Asks => "icons/bell.svg",
            Tab::Apps => "icons/apps.svg",
            Tab::Log => "icons/history.svg",
            Tab::Pair => "icons/link.svg",
        }
    }
}

/// The signer's panel. Ephemeral by design: every open builds a fresh
/// view; the state that outlives it (the snapshot, the offer, the last
/// tab a person chose) lives on the entity.
pub struct NostrSignerView {
    geometry: PanelGeometry,
    nostr: Entity<NostrState>,
    tab: Tab,
    /// The pubkey whose detail view is open, if any.
    selected_app: Option<String>,
    /// The clipboard is read once per open, and only the Pair tab's
    /// landing consumes the read.
    clipboard_checked: bool,
    /// The offer the view last saw: an arriving nostrconnect:// link
    /// (the scheme handler) lands on Pair, opened or not.
    seen_offer: Option<String>,
    /// Decoded avatars, keyed by pubkey: the launcher's icon pattern,
    /// decoded off-thread, folded back in, None while pending or
    /// failed (the fallback tile shows either way).
    avatars: HashMap<String, Option<IconImage>>,
    /// Avatar fetches already started this view's life: a missing
    /// avatar is downloaded once, not once per render.
    downloads: HashSet<String>,
}

impl NostrSignerView {
    pub fn new(
        nostr: Entity<NostrState>,
        _window: &mut Window,
        cx: &mut Context<Self>,
        geometry: PanelGeometry,
    ) -> Self {
        cx.observe(&nostr, |_, _, cx| cx.notify()).detach();

        // Open or act, the panel says a person is here.
        nostr.update(cx, |state, _| state.touch());
        // Onboarding, and the last tab the person chose on a previous
        // open: the state remembers, the fresh view obeys. A chosen
        // tab (the person's click wins) also retires onboarding;
        // once a key is on the entity, an empty app list no longer
        // lands the open on Pair.
        let tab = {
            let state = nostr.read(cx);
            if state.offered_uri.is_some() {
                Tab::Pair
            } else if let Some(key) = state.panel_tab {
                Tab::from_key(key).unwrap_or(Tab::Asks)
            } else if state.apps.is_empty() {
                Tab::Pair
            } else {
                Tab::Asks
            }
        };

        // While the panel is open it polls; the loop dies with the view.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                if this
                    .update(cx, |this, cx| {
                        this.nostr.update(cx, |state, cx| state.refresh_all(cx));
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        nostr.update(cx, |state, cx| state.refresh_all(cx));

        Self {
            geometry,
            nostr,
            tab,
            selected_app: None,
            clipboard_checked: false,
            seen_offer: None,
            avatars: HashMap::new(),
            downloads: HashSet::new(),
        }
    }

    /// The state remembers the tab for the next open; the Luau's
    /// module globals were the memory; here the entity is.
    fn remember_tab(&mut self, cx: &mut Context<Self>) {
        let key = self.tab.key();
        self.nostr.update(cx, |state, cx| {
            if state.panel_tab != Some(key) {
                state.panel_tab = Some(key);
                cx.notify();
            }
        });
    }

    /// The person tapped Pair: the clipboard is read once per open,
    /// most web apps ship a copy button, not a clickable link, so the
    /// copied URI is the common case and the paste step is a toll.
    /// Only the prefix is matched, nothing is stored beyond the offer,
    /// and the offer card it fills is still the person's question to
    /// answer; the tap on Pair remains the approval.
    fn check_clipboard(&mut self, cx: &mut Context<Self>) {
        if self.clipboard_checked || self.nostr.read(cx).offered_uri.is_some() {
            return;
        }
        self.clipboard_checked = true;
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
            && text.trim().starts_with("nostrconnect://")
        {
            let uri = text.trim().to_string();
            self.nostr.update(cx, |state, cx| state.offer(uri, cx));
        }
    }

    /// An arriving offer (the scheme handler, or the clipboard read)
    /// lands on Pair; a click is not an approval, and the person's tap
    /// on Pair is. Watched at render so an offer reaching an open panel
    /// lands live, the `panel-open`-not-toggle contract.
    fn watch_offer(&mut self, cx: &mut Context<Self>) {
        let offered = self.nostr.read(cx).offered_uri.clone();
        if self.seen_offer == offered {
            return;
        }
        self.seen_offer = offered.clone();
        if offered.is_some() {
            self.tab = Tab::Pair;
            self.remember_tab(cx);
        }
    }

    /// The avatar tile: the decoded image when one is ready, the
    /// fallback (a puzzle glyph on an inset tile) while pending,
    /// failed, or never claimed. Signet's gate holds: a claimed URL is
    /// fetched only over https, once per view, cached on disk by pubkey.
    fn avatar(
        &mut self,
        pubkey: &str,
        image: Option<&str>,
        size: f32,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let decoded = match self.avatars.get(pubkey) {
            Some(icon) => icon.clone(),
            None => {
                // First sight this open: try the disk cache off-thread,
                // start the download when the client claims an https
                // image, and show the fallback until one lands.
                self.avatars.insert(pubkey.to_string(), None);
                let pubkey = pubkey.to_string();
                let url = image
                    .filter(|url| nostr::avatar_url_allowed(url))
                    .map(str::to_string);
                let dest = nostr::avatar_path(&pubkey);
                let disk_dest = dest.clone();
                cx.spawn(async move |this, cx| {
                    let from_disk = cx
                        .background_spawn(async move {
                            disk_dest.as_deref().and_then(|path| {
                                path.exists().then(|| crate::imaging::decode_file(path))
                            })
                        })
                        .await
                        .flatten()
                        .map(|raster| IconImage::Raster(raster.into()));
                    let _ = this.update(cx, |this, cx| {
                        match (from_disk, url, dest) {
                            (Some(icon), ..) => {
                                this.avatars.insert(pubkey.clone(), Some(icon));
                            }
                            (None, Some(url), Some(dest)) => {
                                if this.downloads.insert(pubkey.clone()) {
                                    this.start_download(pubkey.clone(), url, dest, cx);
                                }
                            }
                            (None, ..) => {}
                        }
                        cx.notify();
                    });
                })
                .detach();
                None
            }
        };
        let tile = div()
            .flex()
            .items_center()
            .justify_center()
            .size(px(size))
            .flex_shrink_0()
            .rounded_md()
            .bg(rgb(INSET))
            .overflow_hidden();
        match decoded {
            Some(IconImage::Raster(raster)) => tile.child(
                img(gpui::ImageSource::Render(raster))
                    .object_fit(ObjectFit::Cover)
                    .size_full(),
            ),
            Some(IconImage::Svg(bytes)) => tile.child(
                gpui::svg()
                    .data(&bytes)
                    .size(px(size / 2.))
                    .text_color(rgb(ACCENT)),
            ),
            // The stranger's tile: a puzzle glyph until an image exists.
            None => tile.child(
                gpui::svg()
                    .path("icons/puzzle.svg")
                    .size(px(size / 2.))
                    .text_color(rgb(ACCENT)),
            ),
        }
    }

    fn start_download(
        &mut self,
        pubkey: String,
        url: String,
        dest: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_spawn(async move {
                    let parent = dest.parent().map(std::path::Path::to_path_buf);
                    if let Some(parent) = parent {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::process::Command::new("curl")
                        .args(["-fsSL", "--max-time", "10", "-o"])
                        .arg(&dest)
                        .arg(&url)
                        .output()
                        .ok()
                        .filter(|out| out.status.success())
                        .and_then(|_| crate::imaging::decode_file(&dest))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // A failed download stays a fallback until the next
                // open tries again; a landed one renders from here on.
                if let Some(raster) = decoded {
                    this.avatars
                        .insert(pubkey.clone(), Some(IconImage::Raster(raster.into())));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Minting is the act: the copy takes a URI that has never been
    /// spent, not the last one, which a used pairing already burned.
    /// Every arm says something: the copy, the daemon's refusal, or an
    /// answer with no URI in it.
    fn mint_uri(&mut self, cx: &mut Context<Self>) {
        let notifications = self.nostr.read(cx).notifications_handle();
        let nostr = self.nostr.clone();
        cx.spawn(async move |this, cx| {
            let doc = cx.background_spawn(async { mint_cli() }).await;
            let _ = this.update(cx, |_state, cx| {
                match doc.as_ref().and_then(|doc| doc["uri"].as_str()) {
                    Some(uri) => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(uri.to_string()))
                    }
                    None => {
                        let why = match doc {
                            Some(_) => "the mint answered without a URI",
                            None => "the mint failed",
                        };
                        notifications.update(cx, |state, cx| {
                            state.push(crate::nostr::error_notification(why), cx);
                        });
                    }
                }
                nostr.update(cx, |state, cx| state.refresh_all(cx));
            });
        })
        .detach();
    }
}

impl Render for NostrSignerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.watch_offer(cx);

        let state = self.nostr.read(cx);
        let prompts = state.prompts.clone();
        let apps = state.apps.clone();
        let log = state.log.clone();
        let vault = state.vault.clone();
        let offered_uri = state.offered_uri.clone();

        // ── the rail ─────────────────────────────────────────────────
        let rail = div()
            .flex()
            .flex_col()
            .gap_2()
            .w(px(48.))
            .flex_shrink_0()
            .children([Tab::Asks, Tab::Apps, Tab::Log, Tab::Pair].map(|tab| {
                let active = self.tab == tab;
                let count = (tab == Tab::Asks && !prompts.is_empty()).then_some(prompts.len());
                kit::rail_tab(
                    format!("rail-{}", tab.key()),
                    tab.icon(),
                    tab.title(),
                    active,
                    count,
                    cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        if tab == Tab::Pair {
                            this.check_clipboard(cx);
                        }
                        this.remember_tab(cx);
                    }),
                )
            }));

        // ── the pane header: title, badge, the vault's lock state ───
        let unlocked = vault.as_ref().map(|vault| vault.unlocked);
        let badge = (self.tab == Tab::Asks && !prompts.is_empty()).then_some(prompts.len());
        let header = kit::pane_header(self.tab.title())
            .children(badge.map(kit::count_badge))
            .child(
                gpui::svg()
                    .path(if unlocked == Some(true) {
                        "icons/shield-lock.svg"
                    } else {
                        "icons/lock.svg"
                    })
                    .size(px(16.))
                    .text_color(rgb(if unlocked == Some(true) {
                        ACCENT
                    } else {
                        TEXT_DIM
                    })),
            );

        // ── the pane body ────────────────────────────────────────────
        let body = match self.tab {
            Tab::Asks => asks_pane(&prompts, &apps, self, cx),
            Tab::Apps => apps_pane(&apps, self, cx),
            Tab::Log => log_pane(&log, &apps),
            Tab::Pair => pair_pane(&vault, &offered_uri, cx),
        }
        .into_any_element();

        let pane = kit::tabbed_pane(
            header,
            div()
                .id("nostr-pane")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .gap_3()
                .overflow_y_scroll()
                .child(body),
        );

        let content = div()
            .flex()
            .flex_row()
            .gap_3()
            .p(px(14.))
            .size_full()
            .min_h_0()
            .child(rail)
            .child(pane);

        crate::panel::chrome(self.geometry, window, content)
    }
}

// ── the panes ────────────────────────────────────────────────────────

fn asks_pane(
    prompts: &[Prompt],
    apps: &[PairedApp],
    view: &mut NostrSignerView,
    cx: &mut Context<NostrSignerView>,
) -> gpui::Div {
    if prompts.is_empty() {
        return kit::empty_state(
            "icons/check.svg",
            "Nothing is waiting on you",
            "Sign-in and signing asks land here",
        );
    }
    let cards = div().flex().flex_col().gap_3().children(
        prompts
            .iter()
            .map(|prompt| ask_card(prompt, apps, view, cx)),
    );
    wrap_pane(cards)
}

/// One ask: who, the ask in words, the decision's material, three
/// answers.
fn ask_card(
    prompt: &Prompt,
    apps: &[PairedApp],
    view: &mut NostrSignerView,
    cx: &mut Context<NostrSignerView>,
) -> gpui::Stateful<Div> {
    let known = apps.iter().find(|app| app.pubkey == prompt.app);
    let label = match known {
        Some(app) => nostr::display_name(app),
        None if prompt.app.is_empty() => "?".to_string(),
        None => nostr::short(&prompt.app),
    };
    let id = prompt.id.clone();

    // The ask in words: the daemon's label table says what the
    // signature would do, and this line is that sentence, not a method
    // name. A retrying client adds its count here instead of stacking
    // a second card.
    let summary = if prompt.retries > 1 {
        format!("asked {}× · {}", prompt.retries, prompt.summary)
    } else {
        prompt.summary.clone()
    };

    let mut block = kit::card(format!("ask-{id}"))
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(view.avatar(
                    &prompt.app,
                    known.and_then(|app| app.image.as_deref()),
                    38.,
                    cx,
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .text_size(px(12.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT))
                                .truncate()
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(TEXT_DIM))
                                .child(summary),
                        ),
                ),
        )
        // Signet's cue: the kinds that change identity, spend privacy
        // or carry weight wear a warning, so the glance knows which
        // asks deserve the read.
        .when(prompt.sensitive, |el| {
            el.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(URGENT))
                    .child("⚠ Sensitive action: review carefully before approving"),
            )
        })
        .children(prompt.kind.map(|kind| {
            div()
                .text_size(px(11.))
                .text_color(rgb(TEXT_DIM))
                .child(match &prompt.kind_label {
                    Some(label) => format!("kind: {kind} ({label})"),
                    None => format!("kind: {kind}"),
                })
        }))
        .children(prompt.content.clone().map(|content| {
            div()
                .text_size(px(12.))
                .text_color(rgb(TEXT))
                .child(content)
        }))
        .children(prompt.detail.clone().map(|detail| {
            div()
                .text_size(px(11.))
                .text_color(rgb(TEXT_DIM))
                .child(detail)
        }));

    block = block.child(
        div()
            .flex()
            .gap_2()
            .child(kit::button(
                format!("approve-{id}"),
                "Approve",
                Some("icons/check.svg"),
                ButtonVariant::Primary,
                cx.listener({
                    let id = id.clone();
                    move |this, _: &ClickEvent, _, cx| {
                        let id = id.clone();
                        this.nostr
                            .update(cx, |state, cx| state.act(vec!["approve".into(), id], cx));
                    }
                }),
            ))
            .child(kit::button(
                format!("hour-{id}"),
                "An hour",
                None,
                ButtonVariant::Ghost,
                cx.listener({
                    let id = id.clone();
                    move |this, _: &ClickEvent, _, cx| {
                        let id = id.clone();
                        this.nostr.update(cx, |state, cx| {
                            state.act(
                                vec!["approve".into(), id, "--remember".into(), "1".into()],
                                cx,
                            )
                        });
                    }
                }),
            ))
            .child(kit::button(
                format!("deny-{id}"),
                "Deny",
                Some("icons/x.svg"),
                ButtonVariant::Ghost,
                cx.listener({
                    let id = id.clone();
                    move |this, _: &ClickEvent, _, cx| {
                        let id = id.clone();
                        this.nostr
                            .update(cx, |state, cx| state.act(vec!["deny".into(), id], cx));
                    }
                }),
            )),
    );
    block
}

fn apps_pane(
    apps: &[PairedApp],
    view: &mut NostrSignerView,
    cx: &mut Context<NostrSignerView>,
) -> gpui::Div {
    // The detail view: where the acts live. The list's card opened
    // this; the acts are one tap deeper than the list, which is the
    // whole reason the list can stay clean.
    if let Some(selected) = view.selected_app.clone() {
        if let Some(app) = apps.iter().find(|app| app.pubkey == selected) {
            return app_detail(app, view, cx);
        }
        view.selected_app = None; // the app was deleted under the open view
    }
    if apps.is_empty() {
        return kit::empty_state(
            "icons/apps.svg",
            "No apps paired yet",
            "Pair one from the Pair tab",
        );
    }
    let cards = div()
        .flex()
        .flex_col()
        .gap_3()
        .children(apps.iter().map(|app| app_card(app, view, cx)));
    wrap_pane(cards)
}

/// The list's card is a read, not a write: two lines (who the app is
/// and what it has been doing), and a tap opens the detail where the
/// acts live. The tap is a chevron, deliberately not a clickable card:
/// the card keeps its width that way.
fn app_card(
    app: &PairedApp,
    view: &mut NostrSignerView,
    cx: &mut Context<NostrSignerView>,
) -> gpui::Stateful<Div> {
    let pubkey = app.pubkey.clone();
    let revoked = app.revoked_at.is_some();
    let mut line2 = vec![format!("paired {}", nostr::relative(app.paired_at))];
    if app.request_count > 0 {
        line2.push(format!("{} asks", app.request_count));
    }
    if let Some(last) = app.last_used_at {
        line2.push(format!("last {}", nostr::relative(last)));
    }
    kit::card(format!("app-{pubkey}"))
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(view.avatar(&app.pubkey, app.image.as_deref(), 40., cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(if revoked { TEXT_DIM } else { TEXT }))
                        .truncate()
                        .child(nostr::display_name(app)),
                )
                .child(level_badge(&app.level))
                .child(
                    div()
                        .id(SharedString::from(format!("open-{pubkey}")))
                        .flex()
                        .items_center()
                        .px_1()
                        .py_0p5()
                        .rounded_sm()
                        .text_color(rgb(TEXT_DIM))
                        .cursor_pointer()
                        .hover(|el| el.bg(rgb(SURFACE_HOVER)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.selected_app = Some(pubkey.clone());
                            cx.notify();
                        }))
                        .child(
                            gpui::svg()
                                .path("icons/chevron-right.svg")
                                .size(px(14.))
                                .text_color(rgb(TEXT_DIM)),
                        ),
                ),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(if revoked { URGENT } else { TEXT_DIM }))
                .child(format!(
                    "{}{}",
                    if revoked { "revoked · " } else { "" },
                    line2.join(" · ")
                )),
        )
}

/// The detail view: identity, the trust level's three roads, the acts.
fn app_detail(
    app: &PairedApp,
    view: &mut NostrSignerView,
    cx: &mut Context<NostrSignerView>,
) -> gpui::Div {
    let pubkey = app.pubkey.clone();
    let revoked = app.revoked_at.is_some();

    let mut facts = vec![format!("paired {}", nostr::relative(app.paired_at))];
    facts.push(format!("{} asks", app.request_count));
    if let Some(last) = app.last_used_at {
        facts.push(format!("last {}", nostr::relative(last)));
    }

    let pane = div()
        .flex()
        .flex_col()
        .gap_2p5()
        .child(
            // back to the list
            div()
                .id("back-to-apps")
                .flex()
                .items_center()
                .gap_1()
                .px_1()
                .py_0p5()
                .rounded_sm()
                .text_size(px(11.))
                .text_color(rgb(ACCENT))
                .cursor_pointer()
                .hover(|el| el.bg(rgb(SURFACE)))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.selected_app = None;
                    cx.notify();
                }))
                .child(
                    gpui::svg()
                        .path("icons/arrow-left.svg")
                        .size(px(12.))
                        .text_color(rgb(ACCENT)),
                )
                .child("Paired apps"),
        )
        .child(
            kit::card("detail-head")
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(view.avatar(&app.pubkey, app.image.as_deref(), 48., cx))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_size(px(12.5))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(TEXT))
                                        .truncate()
                                        .child(nostr::display_name(app)),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(rgb(TEXT_DIM))
                                        .truncate()
                                        .child(app.pubkey.clone()),
                                ),
                        ),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(TEXT_DIM))
                        .child(facts.join(" · ")),
                ),
        )
        .child(
            kit::card("detail-level")
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .child("Trust level"),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(TEXT_DIM))
                        .child(
                            "ask confirms every act, basic signs the everyday safe list, and \
                             trust signs everything unattended; the doctor grades it Warn",
                        ),
                )
                .child({
                    let mut row = div().flex().gap_2();
                    for level in ["ask", "basic", "trust"] {
                        let pubkey = pubkey.clone();
                        let active = app.level == level;
                        // trust is the loudest thing in the layer: its
                        // pill wears the alarm whether resting or set.
                        row = row.child(kit::button(
                            format!("level-{level}"),
                            level,
                            None,
                            match (level, active) {
                                ("trust", _) => ButtonVariant::Destructive,
                                (_, true) => ButtonVariant::Primary,
                                (_, false) => ButtonVariant::Ghost,
                            },
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                let name = level.to_string();
                                let key = pubkey.clone();
                                this.nostr.update(cx, |state, cx| {
                                    state.act(vec!["level".into(), key, name], cx)
                                });
                            }),
                        ));
                    }
                    row
                }),
        )
        .children(app.perms.clone().map(|perms| {
            kit::card("detail-perms").child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(TEXT_DIM))
                    .child(format!("asks for: {perms}")),
            )
        }))
        .child(
            kit::card("detail-acts")
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .child("This app"),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child({
                            let pubkey = pubkey.clone();
                            if revoked {
                                kit::button(
                                    "unrevoke",
                                    "Un-revoke",
                                    Some("icons/undo.svg"),
                                    ButtonVariant::Ghost,
                                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        let key = pubkey.clone();
                                        this.nostr.update(cx, |state, cx| {
                                            state.act(vec!["unrevoke".into(), key], cx)
                                        });
                                    }),
                                )
                            } else {
                                kit::button(
                                    "revoke",
                                    "Revoke",
                                    Some("icons/shield-off.svg"),
                                    ButtonVariant::Ghost,
                                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        let key = pubkey.clone();
                                        this.nostr.update(cx, |state, cx| {
                                            state.act(vec!["revoke".into(), key], cx)
                                        });
                                    }),
                                )
                            }
                        })
                        .child({
                            let pubkey = pubkey.clone();
                            kit::button(
                                "delete",
                                "Delete",
                                Some("icons/trash.svg"),
                                ButtonVariant::Destructive,
                                cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    let key = pubkey.clone();
                                    this.nostr.update(cx, |state, cx| {
                                        state.act(vec!["delete".into(), key], cx)
                                    });
                                    this.selected_app = None;
                                }),
                            )
                        }),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(TEXT_DIM))
                        .child(if revoked {
                            "a revoked app is refused whatever it carries; delete forgets it outright"
                        } else {
                            "revoke is the ban; delete forgets outright, and a fresh URI pairs again"
                        }),
                ),
        );
    wrap_pane(pane)
}

fn log_pane(entries: &[nostr::LogEntry], apps: &[PairedApp]) -> gpui::Div {
    if entries.is_empty() {
        return kit::empty_state(
            "icons/history.svg",
            "Nothing has happened yet",
            "Asks, approvals and pairings land here as they happen",
        );
    }
    // Newest first: the last thing that happened is the thing to read.
    let rows = div().flex().flex_col().gap_2().children(
        entries
            .iter()
            .enumerate()
            .rev()
            .map(|(index, entry)| log_row(entry, apps, index)),
    );
    wrap_pane(rows)
}

fn log_row(entry: &nostr::LogEntry, apps: &[PairedApp], index: usize) -> gpui::Stateful<Div> {
    let known = apps.iter().find(|app| app.pubkey == entry.app);
    let who = match known {
        Some(app) => nostr::display_name(app),
        None => nostr::short(&entry.app),
    };
    // The verdict wears its weight: a refusal or an expiry is the
    // alarm, an allowance the accent, anything else stays quiet.
    let verdict_color = if entry.verdict.starts_with("denied") || entry.verdict.contains("expired")
    {
        URGENT
    } else if entry.verdict.starts_with("allowed") {
        ACCENT
    } else {
        TEXT_DIM
    };
    let when = chrono::DateTime::from_timestamp(entry.at as i64, 0)
        .map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();
    kit::card(format!("log-{index}-{}", entry.at))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .truncate()
                        .child(who),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(TEXT_DIM))
                        .child(when),
                ),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(TEXT_DIM))
                .child(entry.summary.clone()),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(verdict_color))
                .child(entry.verdict.clone()),
        )
}

fn pair_pane(
    vault: &Option<VaultFact>,
    offered_uri: &Option<String>,
    cx: &mut Context<NostrSignerView>,
) -> gpui::Div {
    let Some(vault) = vault else {
        // The status has not landed yet this open; the mint's own
        // gates below will say what is wrong if the person acts fast.
        return pair_ready(offered_uri, None, cx);
    };
    if !vault.exists {
        // The fresh machine: no identity yet, and unlock is not the
        // road; it fails with "no vault exists" and the person is
        // stuck. Setup is the road, and it asks which one.
        return wrap_pane(
            div()
                .flex()
                .flex_col()
                .gap_2p5()
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .child("No identity yet."),
                )
                .child(div().text_size(px(12.)).text_color(rgb(TEXT_DIM)).child(
                    "Run kuma-nostr setup; it asks which road: a fresh key, or one you \
                             already hold (nsec, hex, a recovery phrase, or an ncryptsec and its \
                             passphrase).",
                )),
        );
    }
    if !vault.unlocked {
        return wrap_pane(
            div()
                .flex()
                .flex_col()
                .gap_2p5()
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .child("The bunker is locked."),
                )
                .child(div().text_size(px(12.)).text_color(rgb(TEXT_DIM)).child(
                    "Unlock from a terminal: kuma-nostr unlock. The keyring is open in \
                             this session, so it costs nothing, and the pairing URI comes with \
                             the unlock.",
                )),
        );
    }
    pair_ready(
        offered_uri,
        vault.inactivity.as_ref().map(|fact| fact.remaining_secs),
        cx,
    )
}

fn pair_ready(
    offered_uri: &Option<String>,
    inactivity: Option<u64>,
    cx: &mut Context<NostrSignerView>,
) -> gpui::Div {
    let pane = div().flex().flex_col().gap_2p5();
    let pane = pane.children(offered_uri.clone().map(|uri| offer_card(uri, cx)));
    let pane = pane
        .child(div().text_size(px(12.)).text_color(rgb(TEXT_DIM)).child(
            "Copy a fresh URI into any NIP-46 app. It pairs one app once; the connect \
                     burns it, so mint another for the next app.",
        ))
        .child(
            div()
                .flex()
                .gap_2()
                .child(kit::button(
                    "mint-uri",
                    "Copy fresh URI",
                    Some("icons/copy.svg"),
                    ButtonVariant::Primary,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.mint_uri(cx);
                    }),
                ))
                .child(kit::button(
                    "rotate",
                    "Rotate",
                    Some("icons/refresh.svg"),
                    ButtonVariant::Ghost,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.nostr
                            .update(cx, |state, cx| state.act(vec!["rotate".into()], cx));
                    }),
                )),
        )
        .child(div().text_size(px(11.)).text_color(rgb(TEXT_DIM)).child(
            "Rotation retires every outstanding URI at once; apps holding old copies \
                     need a fresh one.",
        ));
    let pane = pane.children(inactivity.map(|remaining| {
        div()
            .text_size(px(11.))
            .text_color(rgb(TEXT_DIM))
            .child(format!(
                "the vault locks itself after {remaining}s of no unlock and no keep-alive; \
                 this panel keeps it alive while you are here"
            ))
    }));
    wrap_pane(pane)
}

fn mint_cli() -> Option<serde_json::Value> {
    let output = std::process::Command::new("kuma-nostr")
        .args(["bunker", "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).ok()
}

/// The scheme handler's offering: a nostrconnect:// link clicked
/// anywhere lands here as a question, never as a pairing.
fn offer_card(uri: String, cx: &mut Context<NostrSignerView>) -> gpui::Stateful<Div> {
    let name = uri
        .split("name=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .map(percent_decode);
    let id_key = uri.chars().take(24).collect::<String>();
    kit::card(format!("offer-{id_key}"))
        .child(
            div()
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .child(match &name {
                    Some(name) => format!("A client asked to pair: {name}"),
                    None => "A client asked to pair".to_string(),
                }),
        )
        .child(div().text_size(px(12.)).text_color(rgb(TEXT_DIM)).child(
            "Pairing it signs nothing until you answer its asks. Ignore throws the \
                     invite away.",
        ))
        .child(
            div()
                .flex()
                .gap_2()
                .child(kit::button(
                    "offer-pair",
                    "Pair",
                    Some("icons/check.svg"),
                    ButtonVariant::Primary,
                    cx.listener({
                        let uri = uri.clone();
                        move |this, _: &ClickEvent, _, cx| {
                            let uri = uri.clone();
                            this.nostr.update(cx, |state, cx| {
                                state.act(vec!["connect".into(), uri], cx);
                                state.dismiss_offer(cx);
                            });
                        }
                    }),
                ))
                .child(kit::button(
                    "offer-ignore",
                    "Ignore",
                    Some("icons/x.svg"),
                    ButtonVariant::Ghost,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.nostr.update(cx, |state, cx| state.dismiss_offer(cx));
                    }),
                )),
        )
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── the small vocabulary ─────────────────────────────────────────────
// The card, the button, the empty state: they live in `panel_kit`
// now, shared with the rest of the shell's panels.

fn wrap_pane(pane: gpui::Div) -> gpui::Div {
    div().flex().flex_col().gap_3().child(pane)
}

/// The level badge, the card's right edge: the standing answer, with
/// its weight as the color; trust is the loudest thing in the layer
/// and wears the alarm, basic wears the accent, ask stays quiet.
fn level_badge(level: &str) -> gpui::Div {
    let (icon, color) = match level {
        "trust" => ("icons/shield-lock.svg", URGENT),
        "basic" => ("icons/shield-check.svg", ACCENT),
        _ => ("icons/shield.svg", TEXT_DIM),
    };
    div()
        .flex()
        .items_center()
        .gap_1()
        .flex_shrink_0()
        .child(gpui::svg().path(icon).size(px(13.)).text_color(rgb(color)))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(color))
                .child(level.to_string()),
        )
}
