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
    AppContext, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, ObjectFit, Render, SharedString, Window, div, img, prelude::*, px, rgb, rgba,
};

use crate::imaging::IconImage;
use crate::nostr::{self, NostrState, PairedApp, Prompt, VaultFact};
use crate::panel::PanelGeometry;
use crate::panel_kit::{self as kit, ButtonVariant};

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
    Vault,
}

impl Tab {
    fn key(self) -> &'static str {
        match self {
            Tab::Asks => "asks",
            Tab::Apps => "apps",
            Tab::Log => "log",
            Tab::Pair => "pair",
            Tab::Vault => "vault",
        }
    }

    fn from_key(key: &str) -> Option<Tab> {
        match key {
            "asks" => Some(Tab::Asks),
            "apps" => Some(Tab::Apps),
            "log" => Some(Tab::Log),
            "pair" => Some(Tab::Pair),
            "vault" => Some(Tab::Vault),
            _ => None,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Tab::Asks => "Requests",
            Tab::Apps => "Paired apps",
            Tab::Log => "Activity",
            Tab::Pair => "Pair",
            Tab::Vault => "Vault",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Tab::Asks => "icons/bell.svg",
            Tab::Apps => "icons/apps.svg",
            Tab::Log => "icons/history.svg",
            Tab::Pair => "icons/link.svg",
            Tab::Vault => "icons/shield.svg",
        }
    }
}

/// The Vault tab's form state: the one place the panel holds secret
/// text, and only while a person is mid-form. Every secret String is
/// zeroized on drop; a completed or cancelled form leaves no heap
/// copy behind (the lock screen's discipline; a panel runs beside
/// everything, where freed heap is still exposure).
#[derive(PartialEq)]
enum VaultForm {
    None,
    /// Bring an identity: the secret (nsec, hex, mnemonic, or
    /// ncryptsec) and, when the secret is an ncryptsec, the
    /// passphrase it was wrapped in. Both ride stdin, one line each.
    Import {
        secret: String,
        passphrase: String,
        field: usize,
    },
    /// Wrap the key under a fresh passphrase and write the backup:
    /// the phrase (twice, matched here before the CLI sees it) and
    /// where the file lands.
    Export {
        passphrase: String,
        confirm: String,
        path: String,
        field: usize,
    },
    /// The destroy card, opened by the dry run's answer.
    DestroyConfirm { would: String },
}

impl Drop for VaultForm {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        match self {
            VaultForm::Import { secret, passphrase, .. } => {
                secret.zeroize();
                passphrase.zeroize();
            }
            VaultForm::Export {
                passphrase, confirm, ..
            } => {
                passphrase.zeroize();
                confirm.zeroize();
            }
            VaultForm::None | VaultForm::DestroyConfirm { .. } => {}
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
    /// The Vault tab's mid-form state, secrets and all.
    form: VaultForm,
    /// The form's one-line problem (a mismatch, an empty field): shown
    /// under the fields, cleared on the next keystroke.
    form_note: Option<&'static str>,
    /// The keyboard: the form's fields take the typing while a form
    /// is open; without one nothing in this panel wants keys.
    focus_handle: FocusHandle,
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
        // lands the open on Pair. Pending asks outrank the memory:
        // an open with things waiting on a decision belongs to the
        // asks, and the remembered tab returns once they are cleared
        // (the memory is only written by a person's click, so the
        // detour does not erase it).
        let tab = {
            let state = nostr.read(cx);
            if state.offered_uri.is_some() {
                Tab::Pair
            } else if state.apps.is_empty() {
                Tab::Pair
            } else if !state.prompts.is_empty() {
                Tab::Asks
            } else if let Some(key) = state.panel_tab {
                Tab::from_key(key).unwrap_or(Tab::Asks)
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
            form: VaultForm::None,
            form_note: None,
            focus_handle: cx.focus_handle(),
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
            .bg(rgb(crate::theme::current().inset))
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
                    .text_color(rgb(crate::theme::current().accent)),
            ),
            // The stranger's tile: a puzzle glyph until an image exists.
            None => tile.child(
                gpui::svg()
                    .path("icons/puzzle.svg")
                    .size(px(size / 2.))
                    .text_color(rgb(crate::theme::current().accent)),
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
                    // the URL is remote content (a profile's picture
                    // field): https only, size-capped, so it can neither
                    // fill the disk nor ride curl's other protocols
                    std::process::Command::new("curl")
                        .args([
                            "-fsSL",
                            "--max-time",
                            "10",
                            "--max-filesize",
                            "5000000",
                            "--proto",
                            "https",
                            "-o",
                        ])
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
            .children([Tab::Asks, Tab::Apps, Tab::Log, Tab::Pair, Tab::Vault].map(|tab| {
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
                        crate::theme::current().accent
                    } else {
                        crate::theme::current().text_dim
                    })),
            );

        // ── the pane body ────────────────────────────────────────────
        let body = match self.tab {
            Tab::Asks => asks_pane(&prompts, &apps, self, cx),
            Tab::Apps => apps_pane(&apps, self, cx),
            Tab::Log => log_pane(&log, &apps),
            Tab::Pair => pair_pane(&vault, &offered_uri, cx),
            Tab::Vault => vault_pane(vault.as_ref(), self, cx),
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
            // the Vault form's fields take the typing while a form is
            // open; focus rides the handle, the keys ride the bubble
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.vault_key(event, cx);
            }))
            .child(rail)
            .child(pane);

        crate::panel::chrome(self.geometry, window, cx, content)
    }
}

// ── the vault form's keyboard and roads ─────────────────────────────

impl Focusable for NostrSignerView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl NostrSignerView {
    /// The Vault form's keyboard: the lock screen's handler with more
    /// than one field. Tab moves between them, Enter submits, and the
    /// insert rules are the same ones (the character, not the key
    /// name; paste and kill-line included).
    fn vault_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.form == VaultForm::None {
            return;
        }
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "tab" => {
                self.cycle_field(cx);
            }
            "enter" => self.submit_form(cx),
            "backspace" => {
                if let Some(field) = self.field_mut() {
                    field.pop();
                }
                self.form_note = None;
                cx.notify();
            }
            "v" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                if let (Some(field), Some(text)) =
                    (self.field_mut(), crate::paste::pasted_text(cx))
                {
                    field.push_str(&text);
                    self.form_note = None;
                    cx.notify();
                }
            }
            "u" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                if let Some(field) = self.field_mut() {
                    field.clear();
                    self.form_note = None;
                    cx.notify();
                }
            }
            _ if !keystroke.modifiers.control
                && !keystroke.modifiers.alt
                && !keystroke.modifiers.platform
                && !keystroke.modifiers.function =>
            {
                if let (Some(field), Some(character)) =
                    (self.field_mut(), keystroke.key_char.as_deref())
                {
                    field.push_str(character);
                    self.form_note = None;
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// The field the typing lands in: the form's active one.
    fn field_mut(&mut self) -> Option<&mut String> {
        match &mut self.form {
            VaultForm::Import {
                secret,
                passphrase,
                field,
            } => Some(if *field == 0 { secret } else { passphrase }),
            VaultForm::Export {
                passphrase,
                confirm,
                path,
                field,
            } => Some(match *field {
                0 => passphrase,
                1 => confirm,
                _ => path,
            }),
            VaultForm::None | VaultForm::DestroyConfirm { .. } => None,
        }
    }

    fn cycle_field(&mut self, cx: &mut Context<Self>) {
        match &mut self.form {
            VaultForm::Import { field, .. } => *field = (*field + 1) % 2,
            VaultForm::Export { field, .. } => *field = (*field + 1) % 3,
            VaultForm::None | VaultForm::DestroyConfirm { .. } => return,
        }
        cx.notify();
    }

    /// Enter's road: validate the form the way the CLI would (the
    /// mismatch, the empty field), then send it. The secrets leave as
    /// stdin lines and are zeroized inside the transport; the form
    /// closes either way, the toast says if the act failed.
    fn submit_form(&mut self, cx: &mut Context<Self>) {
        match &self.form {
            VaultForm::Import { .. } => {
                let VaultForm::Import {
                    secret, passphrase, ..
                } = &self.form
                else {
                    unreachable!("matched the form above")
                };
                // validate against the form first: a failed validation
                // must not clone a secret into a String the early
                // return drops un-scrubbed
                if secret.trim().is_empty() {
                    self.form_note = Some("paste or type the secret to import");
                    cx.notify();
                    return;
                }
                if secret.starts_with("ncryptsec1") && passphrase.trim().is_empty() {
                    self.form_note = Some("an ncryptsec needs the passphrase it was wrapped with");
                    cx.notify();
                    return;
                }
                let lines = import_lines(secret, passphrase);
                self.form = VaultForm::None;
                self.nostr.update(cx, |state, cx| {
                    state.act_with_stdin(vec!["import".into()], lines, cx)
                });
            }
            VaultForm::Export { .. } => {
                let VaultForm::Export {
                    passphrase,
                    confirm,
                    path,
                    ..
                } = &self.form
                else {
                    unreachable!("matched the form above")
                };
                // same discipline: validate before anything copies
                if passphrase.trim().is_empty() {
                    self.form_note = Some("choose a passphrase for the exported file");
                    cx.notify();
                    return;
                }
                if passphrase.trim() != confirm.trim() {
                    self.form_note = Some("the two passphrases do not match");
                    cx.notify();
                    return;
                }
                if path.trim().is_empty() {
                    self.form_note = Some("name the file the backup lands in");
                    cx.notify();
                    return;
                }
                let lines = vec![passphrase.clone()];
                let args = vec!["export".into(), "--output".into(), path.clone()];
                self.form = VaultForm::None;
                self.nostr.update(cx, |state, cx| {
                    state.act_with_stdin(args, lines, cx)
                });
            }
            // the destroy card confirms by button only: Enter is too
            // easy a way to cross an unrecoverable line
            VaultForm::None | VaultForm::DestroyConfirm { .. } => {}
        }
    }

    /// The destroy road's first step: the dry run, whose "would" text
    /// is what the confirm card quotes. A dry run that cannot run
    /// (daemon wedged, vault already gone) leaves a note instead of a
    /// card, because a confirm built on an unreadable premise
    /// confirms nothing.
    fn dry_destroy(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let would = cx
                .background_spawn(async move {
                    let output = kuma_platform::process::timed_output(
                        "kuma-nostr",
                        &["destroy", "--json"],
                        15,
                    )
                    .ok()?;
                    if !output.status.success() {
                        return None;
                    }
                    let doc: serde_json::Value =
                        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
                            .ok()?;
                    doc["would"].as_str().map(str::to_string)
                })
                .await;
            let _ = this.update(cx, |this, _cx| match would {
                Some(would) => {
                    this.form = VaultForm::DestroyConfirm { would };
                }
                None => {
                    this.form_note = Some("the dry run could not run; nothing is confirmed yet");
                }
            });
        })
        .detach();
    }
}

/// The import's stdin payload: the secret, and for an ncryptsec the
/// passphrase it was wrapped in as the second line. The CLI reads a
/// pipe as one line per answer, so the line count is the contract.
fn import_lines(secret: &str, passphrase: &str) -> Vec<String> {
    let mut lines = vec![secret.to_string()];
    if secret.starts_with("ncryptsec1") {
        lines.push(passphrase.to_string());
    }
    lines
}

/// The export's suggested landing: a datestamped file in the home
/// directory. Datestamped, because the CLI never overwrites and a
/// yesterday's backup by the same name would refuse today's.
fn export_default_path() -> String {
    export_path_in(&std::env::var("HOME").unwrap_or_default())
}

fn export_path_in(home: &str) -> String {
    let date = Local::now().format("%Y-%m-%d");
    format!("{home}/kuma-nostr-{date}.ncryptsec")
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
                                .text_color(rgb(crate::theme::current().text))
                                .truncate()
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(crate::theme::current().text_dim))
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
                    .text_color(rgb(crate::theme::URGENT))
                    .child("⚠ Sensitive action: review carefully before approving"),
            )
        })
        .children(prompt.kind.map(|kind| {
            div()
                .text_size(px(11.))
                .text_color(rgb(crate::theme::current().text_dim))
                .child(match &prompt.kind_label {
                    Some(label) => format!("kind: {kind} ({label})"),
                    None => format!("kind: {kind}"),
                })
        }))
        .children(prompt.content.clone().map(|content| {
            div()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::current().text))
                .child(content)
        }))
        .children(prompt.detail.clone().map(|detail| {
            div()
                .text_size(px(11.))
                .text_color(rgb(crate::theme::current().text_dim))
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
                        .text_color(rgb(if revoked { crate::theme::current().text_dim } else { crate::theme::current().text }))
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
                        .text_color(rgb(crate::theme::current().text_dim))
                        .cursor_pointer()
                        .hover(|el| el.bg(rgb(crate::theme::current().surface_hover)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.selected_app = Some(pubkey.clone());
                            cx.notify();
                        }))
                        .child(
                            gpui::svg()
                                .path("icons/chevron-right.svg")
                                .size(px(14.))
                                .text_color(rgb(crate::theme::current().text_dim)),
                        ),
                ),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(if revoked { crate::theme::URGENT } else { crate::theme::current().text_dim }))
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
                .text_color(rgb(crate::theme::current().accent))
                .cursor_pointer()
                .hover(|el| el.bg(rgb(crate::theme::current().surface)))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.selected_app = None;
                    cx.notify();
                }))
                .child(
                    gpui::svg()
                        .path("icons/arrow-left.svg")
                        .size(px(12.))
                        .text_color(rgb(crate::theme::current().accent)),
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
                                        .text_color(rgb(crate::theme::current().text))
                                        .truncate()
                                        .child(nostr::display_name(app)),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(rgb(crate::theme::current().text_dim))
                                        .truncate()
                                        .child(app.pubkey.clone()),
                                ),
                        ),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(crate::theme::current().text_dim))
                        .child(facts.join(" · ")),
                ),
        )
        .child(
            kit::card("detail-level")
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(crate::theme::current().text))
                        .child("Trust level"),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(crate::theme::current().text_dim))
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
                    .text_color(rgb(crate::theme::current().text_dim))
                    .child(format!("asks for: {perms}")),
            )
        }))
        .child(
            kit::card("detail-acts")
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(crate::theme::current().text))
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
                        .text_color(rgb(crate::theme::current().text_dim))
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
        crate::theme::URGENT
    } else if entry.verdict.starts_with("allowed") {
        crate::theme::current().accent
    } else {
        crate::theme::current().text_dim
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
                        .text_color(rgb(crate::theme::current().text))
                        .truncate()
                        .child(who),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(crate::theme::current().text_dim))
                        .child(when),
                ),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::current().text_dim))
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
        // The fresh machine: no identity yet. Setup's roads moved into
        // the panel (the Vault tab): a fresh key is one act, and an
        // import is a form whose fields ride stdin.
        return wrap_pane(
            div()
                .flex()
                .flex_col()
                .gap_2p5()
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(crate::theme::current().text))
                        .child("No identity yet."),
                )
                .child(div().text_size(px(12.)).text_color(rgb(crate::theme::current().text_dim)).child(
                    "Generate a key or import one on the Vault tab; pairing waits on an \
                             identity.",
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
                        .text_color(rgb(crate::theme::current().text))
                        .child("The bunker is locked."),
                )
                .child(div().text_size(px(12.)).text_color(rgb(crate::theme::current().text_dim)).child(
                    "Unlock it on the Vault tab. The keyring is open in this session, so \
                             it costs nothing, and the pairing URI comes with the unlock.",
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
        .child(div().text_size(px(12.)).text_color(rgb(crate::theme::current().text_dim)).child(
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
        .child(div().text_size(px(11.)).text_color(rgb(crate::theme::current().text_dim)).child(
            "Rotation retires every outstanding URI at once; apps holding old copies \
                     need a fresh one.",
        ));
    let pane = pane.children(inactivity.map(|remaining| {
        div()
            .text_size(px(11.))
            .text_color(rgb(crate::theme::current().text_dim))
            .child(format!(
                "the vault locks itself after {remaining}s of no unlock and no keep-alive; \
                 this panel keeps it alive while you are here"
            ))
    }));
    wrap_pane(pane)
}

/// The Vault tab: the whole vault's life in one pane, the roads the
/// CLI held alone before this. The state machine reads off the vault
/// fact: not yet known (the status has not landed), no vault (setup's
/// two roads), locked (unlock), unlocked (identity, session, backup,
/// danger). A mid-form render wins over all of it: the form's card
/// replaces the body until it is sent or cancelled.
fn vault_pane(
    vault: Option<&VaultFact>,
    view: &mut NostrSignerView,
    cx: &mut Context<NostrSignerView>,
) -> gpui::Div {
    if let Some(note) = view.form_note {
        return wrap_pane(
            div()
                .flex()
                .flex_col()
                .gap_2p5()
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(crate::theme::URGENT))
                        .child(note),
                )
                .child(cancel_button(view, cx)),
        );
    }
    let Some(vault) = vault else {
        // the status has not landed this open; the poll brings it
        return wrap_pane(
            div()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::current().text_dim))
                .child("Reading the vault's state."),
        );
    };
    if !vault.exists {
        return setup_pane(view, cx);
    }
    if !vault.unlocked {
        return locked_pane(vault, view, cx);
    }
    unlocked_pane(vault, view, cx)
}

/// The fresh machine: setup's two roads live here instead of a
/// terminal detour. Generate is one act; import opens the form whose
/// fields ride stdin.
fn setup_pane(view: &mut NostrSignerView, cx: &mut Context<NostrSignerView>) -> gpui::Div {
    if let VaultForm::Import { .. } = view.form {
        return import_form_pane(view, cx);
    }
    wrap_pane(
        div()
            .flex()
            .flex_col()
            .gap_2p5()
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(crate::theme::current().text))
                    .child("No identity yet."),
            )
            .child(
                div().text_size(px(12.)).text_color(rgb(crate::theme::current().text_dim)).child(
                    "Generate a fresh key, or import one you already hold: an nsec, a hex \
                     key, a recovery phrase, or an ncryptsec file and its passphrase.",
                ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(kit::button(
                        "vault-generate",
                        "Generate a new identity",
                        Some("icons/shield.svg"),
                        ButtonVariant::Primary,
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.nostr
                                .update(cx, |state, cx| state.act(vec!["generate".into()], cx));
                        }),
                    ))
                    .child(kit::button(
                        "vault-import-open",
                        "Import one you hold",
                        None,
                        ButtonVariant::Ghost,
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.form = VaultForm::Import {
                                secret: String::new(),
                                passphrase: String::new(),
                                field: 0,
                            };
                            this.focus_handle.focus(window, cx);
                            cx.notify();
                        }),
                    )),
            ),
    )
}

/// The vault is there and locked: one act gets it back, and the
/// identity is named so the person knows which vault they hold.
fn locked_pane(
    vault: &VaultFact,
    _view: &mut NostrSignerView,
    cx: &mut Context<NostrSignerView>,
) -> gpui::Div {
    wrap_pane(
        div()
            .flex()
            .flex_col()
            .gap_2p5()
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(crate::theme::current().text))
                    .child("The vault is locked."),
            )
            .children(vault.pubkey.as_deref().map(|npub| {
                div()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::current().text_dim))
                    .child(format!("identity {}", nostr::short(npub)))
            }))
            .child(
                div().text_size(px(12.)).text_color(rgb(crate::theme::current().text_dim)).child(
                    "Signing, pairing and export all wait on the unlock. The keyring is open \
                     in this session, so it costs nothing.",
                ),
            )
            .child(
                kit::button(
                    "vault-unlock",
                    "Unlock",
                    Some("icons/shield-lock.svg"),
                    ButtonVariant::Primary,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.nostr
                            .update(cx, |state, cx| state.act(vec!["unlock".into()], cx));
                    }),
                ),
            ),
    )
}

/// The unlocked vault: the identity, the session with its inactivity
/// switch, the backup that makes destroy survivable, and the danger
/// zone that quotes the dry run.
fn unlocked_pane(
    vault: &VaultFact,
    view: &mut NostrSignerView,
    cx: &mut Context<NostrSignerView>,
) -> gpui::Div {
    // a form mid-flight replaces the body: the fields are the pane
    // until they are sent or cancelled
    match &view.form {
        VaultForm::Export { .. } => return export_form_pane(view, cx),
        VaultForm::DestroyConfirm { .. } => return destroy_confirm_pane(view, cx),
        _ => {}
    }

    let identity = kit::card("vault-identity")
        .child(
            div()
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(crate::theme::current().text))
                .child("Identity"),
        )
        .children(vault.pubkey.as_deref().map(|npub| {
            div()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::current().text_dim))
                .truncate()
                .child(npub.to_string())
        }))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(crate::theme::current().text_dim))
                .child(format!(
                    "{} relays configured, {} connected",
                    vault.relays.len(),
                    vault.connected.len()
                )),
        );

    let session = kit::card("vault-session")
        .child(
            div()
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(crate::theme::current().text))
                .child("Session"),
        )
        .children(vault.inactivity.as_ref().map(|switch| {
            div()
                .text_size(px(12.))
                .text_color(rgb(crate::theme::current().text_dim))
                .child(format!(
                    "self-locks in {}s of its {}s window; this panel being here keeps it alive",
                    switch.remaining_secs, switch.window_secs
                ))
        }))
        .child(
            kit::button(
                "vault-lock",
                "Lock now",
                Some("icons/lock.svg"),
                ButtonVariant::Ghost,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.nostr
                        .update(cx, |state, cx| state.act(vec!["lock".into()], cx));
                }),
            ),
        );

    let backup = kit::card("vault-backup")
        .child(
            div()
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(crate::theme::current().text))
                .child("Backup"),
        )
        .child(
            div().text_size(px(12.)).text_color(rgb(crate::theme::current().text_dim)).child(
                "Wrap the key under a passphrase you choose and write the ncryptsec file. \
                 The file is created private and never overwritten; importing it back is \
                 the import road on any machine.",
            ),
        )
        .child(
            kit::button(
                "vault-export-open",
                "Export a backup",
                Some("icons/download.svg"),
                ButtonVariant::Primary,
                cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.form = VaultForm::Export {
                        passphrase: String::new(),
                        confirm: String::new(),
                        path: export_default_path(),
                        field: 0,
                    };
                    this.focus_handle.focus(window, cx);
                    cx.notify();
                }),
            ),
        );

    let danger = kit::card("vault-danger")
        .child(
            div()
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(crate::theme::URGENT))
                .child("Danger zone"),
        )
        .child(
            div().text_size(px(12.)).text_color(rgb(crate::theme::current().text_dim)).child(
                "Delete the vault outright. The key is unrecoverable afterwards; the dry \
                 run names the cost before anything is asked twice.",
            ),
        )
        .child(
            kit::button(
                "vault-destroy-dry",
                "Destroy the vault",
                None,
                ButtonVariant::Destructive,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.dry_destroy(cx);
                }),
            ),
        );

    wrap_pane(
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(identity)
            .child(session)
            .child(backup)
            .child(danger),
    )
}

/// The import form: the secret, masked, and the ncryptsec's
/// passphrase when the secret says it needs one. Both leave through
/// stdin, one line each; neither ever touches a command line.
fn import_form_pane(view: &mut NostrSignerView, cx: &mut Context<NostrSignerView>) -> gpui::Div {
    let VaultForm::Import {
        secret,
        passphrase,
        field,
    } = &view.form
    else {
        unreachable!("the import pane renders the import form")
    };
    let is_ncryptsec = secret.starts_with("ncryptsec1");
    let mut card = kit::card("vault-import-form")
        .child(
            div()
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(crate::theme::current().text))
                .child("Import an identity"),
        )
        .child(vault_field(
            "vault-import-secret",
            secret,
            field == &0,
            "nsec, hex key, or recovery phrase (an ncryptsec works too)",
            true,
        ))
        .when(is_ncryptsec, |el| {
            el.child(vault_field(
                "vault-import-passphrase",
                passphrase,
                field == &1,
                "the passphrase the ncryptsec was wrapped with",
                true,
            ))
        })
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(crate::theme::current().text_dim))
                .child("Tab moves between the fields; the secret rides stdin, never a command line."),
        );
    card = card.child(
        div()
            .flex()
            .gap_2()
            .child(kit::button(
                "vault-import-send",
                "Import",
                Some("icons/check.svg"),
                ButtonVariant::Primary,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.submit_form(cx);
                }),
            ))
            .child(cancel_button(view, cx)),
    );
    wrap_pane(div().child(card))
}

/// The export form: the fresh passphrase twice, the file's road, and
/// the CLI's honesty carried onto the card (0600 at write, never an
/// overwrite).
fn export_form_pane(view: &mut NostrSignerView, cx: &mut Context<NostrSignerView>) -> gpui::Div {
    let VaultForm::Export {
        passphrase,
        confirm,
        path,
        field,
    } = &view.form
    else {
        unreachable!("the export pane renders the export form")
    };
    let (passphrase, confirm, path, field) =
        (passphrase.clone(), confirm.clone(), path.clone(), *field);
    let mut card = kit::card("vault-export-form")
        .child(
            div()
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(crate::theme::current().text))
                .child("Export a backup"),
        )
        .child(vault_field(
            "vault-export-passphrase",
            &passphrase,
            field == 0,
            "a passphrase for the file (16+ characters, letters and digits)",
            true,
        ))
        .child(vault_field(
            "vault-export-confirm",
            &confirm,
            field == 1,
            "the same passphrase again",
            true,
        ))
        .child(vault_field(
            "vault-export-path",
            &path,
            field == 2,
            "where the backup lands",
            false,
        ))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(crate::theme::current().text_dim))
                .child("The vault must be unlocked to export; the file is created 0600 and an existing file is never overwritten."),
        );
    card = card.child(
        div()
            .flex()
            .gap_2()
            .child(kit::button(
                "vault-export-send",
                "Write the backup",
                Some("icons/check.svg"),
                ButtonVariant::Primary,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.submit_form(cx);
                }),
            ))
            .child(cancel_button(view, cx)),
    );
    wrap_pane(div().child(card))
}

/// The destroy card, opened by the dry run: the cost named in the
/// daemon's own words, the way out (a backup) one click away, and the
/// unrecoverable button last.
fn destroy_confirm_pane(view: &mut NostrSignerView, cx: &mut Context<NostrSignerView>) -> gpui::Div {
    let VaultForm::DestroyConfirm { would } = &view.form else {
        unreachable!("the destroy pane renders the confirm card")
    };
    let would = would.clone();
    wrap_pane(div().child(
        kit::card("vault-destroy-confirm")
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(crate::theme::URGENT))
                    .child("Destroy the vault?"),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(crate::theme::current().text))
                    .child(would),
            )
            .child(
                div().text_size(px(12.)).text_color(rgb(crate::theme::current().text_dim)).child(
                    "The key is unrecoverable after this. Every paired app loses the \
                     signer; nothing brings the identity back except a backup you made.",
                ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(kit::button(
                        "vault-destroy-export",
                        "Export a backup first",
                        Some("icons/download.svg"),
                        ButtonVariant::Ghost,
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.form = VaultForm::Export {
                                passphrase: String::new(),
                                confirm: String::new(),
                                path: export_default_path(),
                                field: 0,
                            };
                            this.focus_handle.focus(window, cx);
                            cx.notify();
                        }),
                    ))
                    .child(cancel_button(view, cx))
                    .child(kit::button(
                        "vault-destroy-yes",
                        "Destroy for good",
                        None,
                        ButtonVariant::Destructive,
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.form = VaultForm::None;
                            this.nostr.update(cx, |state, cx| {
                                state.act(vec!["destroy".into(), "--yes".into()], cx)
                            });
                        }),
                    )),
            ),
    ))
}

fn cancel_button(
    _view: &mut NostrSignerView,
    cx: &mut Context<NostrSignerView>,
) -> gpui::Stateful<Div> {
    kit::button(
        "vault-cancel",
        "Cancel",
        None,
        ButtonVariant::Ghost,
        cx.listener(|this, _: &ClickEvent, _, cx| {
            this.form = VaultForm::None;
            this.form_note = None;
            cx.notify();
        }),
    )
}

/// One form field: the lock screen's masked shape at the panel's
/// width. `masked` renders dots over the value (the path field is the
/// exception: a road is not a secret); the outline answers to focus.
fn vault_field(
    id: &str,
    value: &str,
    focused: bool,
    placeholder: &str,
    masked: bool,
) -> gpui::Stateful<Div> {
    let shown: SharedString = if value.is_empty() {
        placeholder.into()
    } else if masked {
        "•".repeat(value.chars().count()).into()
    } else {
        value.to_string().into()
    };
    div()
        .id(id.to_string())
        .flex()
        .items_center()
        .w_full()
        .px_3()
        .py_2()
        .rounded_lg()
        .bg(rgba(crate::theme::current().panel_bg))
        .border_1()
        .border_color(rgb(if focused {
            crate::theme::current().accent
        } else {
            crate::theme::current().divider
        }))
        .text_size(px(12.5))
        .text_color(rgb(if value.is_empty() {
            crate::theme::current().text_dim
        } else {
            crate::theme::current().text
        }))
        .truncate()
        .child(shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_import_pipes_one_line_per_answer_the_cli_reads() {
        // the nsec road: one line. The ncryptsec road: the secret and
        // the passphrase it was wrapped in, in that order, because the
        // CLI's pipe reads answers by position.
        assert_eq!(import_lines("nsec1abc", ""), vec!["nsec1abc"]);
        assert_eq!(
            import_lines("ncryptsec1abc", "lakeCreature"),
            vec!["ncryptsec1abc", "lakeCreature"]
        );
        // a passphrase typed for a plain nsec is dropped, not sent:
        // the daemon asks nothing on that road
        assert_eq!(import_lines("nsec1abc", "ignored"), vec!["nsec1abc"]);
    }

    #[test]
    fn the_export_path_is_datestamped_in_the_home() {
        // datestamped so a rerun never lands on yesterday's file: the
        // CLI refuses an existing target, and a refused backup should
        // read as a feature, not a fault
        let path = export_path_in("/var/home/martin");
        assert!(path.starts_with("/var/home/martin/"));
        assert!(path.ends_with(".ncryptsec"));
        assert!(path.contains("kuma-nostr-"));
    }
}

fn mint_cli() -> Option<serde_json::Value> {
    // bounded like every other CLI read (nostr.rs's fetch_cli): a hung
    // vault daemon costs this task one wait, never forever
    let output = kuma_platform::process::timed_output("kuma-nostr", &["bunker", "--json"], 15)
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
                .text_color(rgb(crate::theme::current().text))
                .child(match &name {
                    Some(name) => format!("A client asked to pair: {name}"),
                    None => "A client asked to pair".to_string(),
                }),
        )
        .child(div().text_size(px(12.)).text_color(rgb(crate::theme::current().text_dim)).child(
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
    // the form-encoding quirk rides along: nostr links spell a space
    // `+` in their query components
    kuma_platform::files::percent_decode(text, true)
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
        "trust" => ("icons/shield-lock.svg", crate::theme::URGENT),
        "basic" => ("icons/shield-check.svg", crate::theme::current().accent),
        _ => ("icons/shield.svg", crate::theme::current().text_dim),
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
