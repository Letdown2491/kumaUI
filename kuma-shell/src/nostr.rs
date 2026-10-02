//! The nostr signer's state and transport: a poller that watches the
//! bunker's ask queue for the bar widget, the shared snapshot the panel
//! reads, and the CLI road every act rides.
//!
//! The daemon stays the policy, the CLI stays the transport; the same
//! division the noctalia plugin carried ("no Luau ever holds a key",
//! and no Rust here does either). Every call is argv-form through
//! [`std::process::Command`]: a nostrconnect URI is all `&` and `?`, and
//! a shell line would shatter it.
//!
//! A fetch's answer is a fact only when the fetch worked. A wedged
//! daemon, a missing binary, or a nonzero exit comes back as `None`,
//! and the last-known snapshot stays; an answer that said "no asks"
//! because the socket was dead would be a lie the next poll corrects.

use std::process::Command;
use std::sync::Arc;

use chrono::Local;
use gpui::{App, AppContext, Context, Entity};
use serde::Deserialize;

use crate::imaging::IconImage;
use crate::notifications::Notification;

/// The bar widget's cadence: the ask queue is the one fact the bar
/// needs, and a count a few seconds stale is no count at all.
const WIDGET_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// What `prompts --json` shows: one ask, enough to decide on. Field
/// names mirror the CLI's `PromptView` (kuma's policy.rs); the shapes
/// are a contract, pinned by the daemon's own tests.
#[derive(Clone, Debug, Deserialize)]
pub struct Prompt {
    pub id: String,
    pub app: String,
    /// The raw method, as the app sent it: the panel maps it to a glyph.
    pub method: String,
    pub summary: String,
    #[serde(default)]
    pub kind: Option<u64>,
    #[serde(default)]
    pub kind_label: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub retries: u64,
    #[serde(default)]
    pub detail: Option<String>,
}

/// One paired app: identity, level, and when it was paired. `name`,
/// `image`, `url` and `perms` are the client's own claims; display
/// hints, nothing authorizes by them.
#[derive(Clone, Debug, Deserialize)]
pub struct PairedApp {
    pub pubkey: String,
    pub level: String,
    pub paired_at: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub perms: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub revoked_at: Option<u64>,
    #[serde(default)]
    pub request_count: u64,
    #[serde(default)]
    pub last_used_at: Option<u64>,
}

/// One line of the activity log: what was asked, by whom, and why it
/// went the way it went.
#[derive(Clone, Debug, Deserialize)]
pub struct LogEntry {
    pub at: u64,
    pub app: String,
    pub method: String,
    pub summary: String,
    pub verdict: String,
}

/// The inactivity switch's state, when it is armed.
#[derive(Clone, Debug, Deserialize)]
pub struct InactivityFact {
    #[serde(default)]
    pub window_secs: u64,
    pub remaining_secs: u64,
}

/// What `status --json` reports about the vault under its `vault` key.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct VaultFact {
    #[serde(default)]
    pub exists: bool,
    #[serde(default)]
    pub unlocked: bool,
    #[serde(default)]
    pub inactivity: Option<InactivityFact>,
}

/// The signer's shared snapshot: last-known answers from the CLI, the
/// offer a nostrconnect:// link handed in, and the pending count the
/// bar's rise notification compares against.
pub struct NostrState {
    pub prompts: Vec<Prompt>,
    pub apps: Vec<PairedApp>,
    pub vault: Option<VaultFact>,
    pub log: Vec<LogEntry>,
    /// A nostrconnect:// link the scheme handler (or the clipboard)
    /// handed in: the panel's offer card fills from it. None is also
    /// the dismissed state; Ignore clears it.
    pub offered_uri: Option<String>,
    /// The last tab a person chose, by key: the Luau's module globals
    /// were the memory across panel opens; here the entity is.
    pub panel_tab: Option<&'static str>,
    notifications: Entity<crate::notifications::NotificationState>,
    /// The pending count the last successful prompts poll reported: a
    /// rise over it is a toast.
    last_pending: usize,
}

impl NostrState {
    pub fn new(notifications: Entity<crate::notifications::NotificationState>) -> Self {
        Self {
            prompts: Vec::new(),
            apps: Vec::new(),
            vault: None,
            log: Vec::new(),
            offered_uri: None,
            panel_tab: None,
            notifications,
            last_pending: 0,
        }
    }

    /// The error-toast road for callers outside this module (the
    /// panel's mint): the same named sender the rise and act failures
    /// use.
    pub(crate) fn notifications_handle(&self) -> Entity<crate::notifications::NotificationState> {
        self.notifications.clone()
    }

    /// The scheme handler's landing: a URI arrived from outside. The
    /// caller opens the panel; the view watches for the offer and
    /// lands on Pair.
    pub fn offer(&mut self, uri: String, cx: &mut Context<Self>) {
        self.offered_uri = Some(uri);
        // Open or act, the panel says a person is here.
        self.touch();
        cx.notify();
    }

    pub fn dismiss_offer(&mut self, cx: &mut Context<Self>) {
        self.offered_uri = None;
        cx.notify();
    }

    /// The keep-alive, fire and forget: answering a prompt four
    /// minutes in must not race the vault's own lock.
    pub fn touch(&self) {
        let _ = Command::new("kuma-nostr").arg("touch").spawn();
    }

    /// One act of the panel (approve, deny, revoke, level, rotate),
    /// with the keep-alive riding along and a failed act saying so.
    /// The refresh follows in both cases: the answer moves the lists.
    pub fn act(&mut self, mut args: Vec<String>, cx: &mut Context<Self>) {
        self.touch();
        args.insert(0, "kuma-nostr".into());
        let notifications = self.notifications.clone();
        cx.spawn(async move |this, cx| {
            let output = cx
                .background_spawn(async move { Command::new(&args[0]).args(&args[1..]).output() })
                .await;
            let failure = match output {
                Err(_) => Some("the act could not run".to_string()),
                Ok(out) if !out.status.success() => {
                    let said = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_string();
                    let why = if !out.stderr.is_empty() {
                        said(&out.stderr)
                    } else if !out.stdout.is_empty() {
                        said(&out.stdout)
                    } else {
                        "the act failed".to_string()
                    };
                    Some(why)
                }
                Ok(_) => None,
            };
            if let Some(why) = failure {
                notifications.update(cx, |state, cx| {
                    state.push(error_notification(&why), cx);
                });
            }
            let _ = this.update(cx, |this, cx| this.refresh_all(cx));
        })
        .detach();
    }

    /// The panel's refresh: all four reads, fired concurrently, each
    /// keeping the last-known answer on failure.
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        for field in Field::ALL {
            let fetch = fetch_cli(field);
            cx.spawn(async move |this, cx| {
                if let Some(doc) = cx.background_spawn(async move { fetch }).await {
                    let _ = this.update(cx, |this, cx| {
                        this.apply(field, doc);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
    }

    fn apply(&mut self, field: Field, doc: serde_json::Value) {
        match field {
            Field::Prompts => {
                if let Ok(items) = serde_json::from_value::<Vec<Prompt>>(doc["prompts"].clone()) {
                    self.prompts = items;
                }
            }
            Field::Apps => {
                if let Ok(items) = serde_json::from_value::<Vec<PairedApp>>(doc["apps"].clone()) {
                    self.apps = items;
                }
            }
            Field::Status => {
                if let Ok(vault) = serde_json::from_value::<VaultFact>(doc["vault"].clone()) {
                    self.vault = Some(vault);
                }
            }
            Field::Log => {
                if let Ok(items) = serde_json::from_value::<Vec<LogEntry>>(doc["log"].clone()) {
                    self.log = items;
                }
            }
        }
    }
}

/// Which read a background fetch answers.
#[derive(Clone, Copy)]
enum Field {
    Prompts,
    Apps,
    Status,
    Log,
}

impl Field {
    const ALL: [Field; 4] = [Field::Prompts, Field::Apps, Field::Status, Field::Log];
}

/// One CLI read: the JSON document on success, None on any failure,
/// and None is "keep what you have", never "nothing is there".
fn fetch_cli(field: Field) -> Option<serde_json::Value> {
    let args: &[&str] = match field {
        Field::Prompts => &["prompts", "--json"],
        Field::Apps => &["apps", "--json"],
        Field::Status => &["status", "--json"],
        Field::Log => &["log", "--json"],
    };
    let output = Command::new("kuma-nostr").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).ok()
}

/// The widget's poll: the ask queue every [`WIDGET_POLL`], a toast when
/// the count rises. Runs for the shell's whole life; the bar's glyph
/// is only as honest as this loop.
pub fn run(state: &Entity<NostrState>, cx: &mut App) {
    let state = state.downgrade();
    cx.spawn(async move |cx| {
        loop {
            let doc = cx
                .background_spawn(async { fetch_cli(Field::Prompts) })
                .await;
            let alive = state.update(cx, |state, cx| {
                if let Some(doc) = doc
                    && let Ok(items) = serde_json::from_value::<Vec<Prompt>>(doc["prompts"].clone())
                {
                    let count = items.len();
                    if count > state.last_pending {
                        let body = format!("{} waiting on you", ask_word(count));
                        let notifications = state.notifications.clone();
                        notifications.update(cx, |state, cx| {
                            state.push(rise_notification(&body), cx);
                        });
                    }
                    state.last_pending = count;
                    state.prompts = items;
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
            cx.background_executor().timer(WIDGET_POLL).await;
        }
    })
    .detach();
}

fn ask_word(count: usize) -> String {
    format!("{count} ask{}", if count == 1 { "" } else { "s" })
}

/// The rise toast, through the shell's own daemon: DND, history and
/// toasts behave exactly as a client's Notify would.
fn rise_notification(body: &str) -> Notification {
    named_notification(body)
}

/// A failed act (or mint) says so: the CLI's stderr is the sentence
/// the person reads; the silence once made a working revoke look
/// broken.
pub(crate) fn error_notification(why: &str) -> Notification {
    named_notification(why)
}

fn named_notification(body: &str) -> Notification {
    Notification {
        id: 0,
        app_name: WIDGET_LABEL.into(),
        icon: Some(IconImage::Svg(Arc::from(
            include_bytes!("../icons/shield-lock.svg").as_ref(),
        ))),
        summary: WIDGET_LABEL.into(),
        body: body.into(),
        actions: Vec::new(),
        urgency: crate::notifications::Urgency::Normal,
        received_at: Local::now(),
        expire_timeout: -1,
        closed: false,
    }
}

/// The widget's public name: the bar's registry label, the toast's
/// app name, and the panel header's source of truth.
pub const WIDGET_LABEL: &str = "Nostr Signer";

// ── display vocabulary ───────────────────────────────────────────────

/// The pubkey's shortened form: the stranger's name of last resort.
pub fn short(pubkey: &str) -> String {
    let mut cut = pubkey.chars().take(12).collect::<String>();
    cut.push('…');
    cut
}

/// The display name, the daemon's claim order: the name the client
/// claimed, then the name derived from the url it claimed (last two
/// host labels; account.nostr.build is nostr.build), then the pubkey
/// fragment. All of it is the client's own word; none of it decides
/// anything.
pub fn display_name(app: &PairedApp) -> String {
    if let Some(name) = &app.name
        && !name.is_empty()
    {
        return name.clone();
    }
    if let Some(url) = &app.url
        && let Some(host) = host_of(url)
    {
        let labels: Vec<&str> = host.split('.').collect();
        if labels.len() >= 2 {
            let taken = &labels[labels.len() - 2..];
            return taken.join(".").to_lowercase();
        }
        return host.to_lowercase();
    }
    short(&app.pubkey)
}

fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = after_scheme
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or_default();
    (!host.is_empty()).then(|| host.to_string())
}

/// A relative time, the list's second line: paired and last asked as
/// ago-words, not unix numbers.
pub fn relative(ts: u64) -> String {
    let now = Local::now().timestamp().max(0) as u64;
    let delta = now.saturating_sub(ts);
    match delta {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86400),
    }
}

/// The avatar's cache path: the state dir the shell owns, one file per
/// pubkey fragment: the same shape the plugin's ICON_DIR carried.
pub fn avatar_path(pubkey: &str) -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| std::path::Path::new(&home).join(".local/state"))
        })?;
    Some(base.join("kuma-shell").join("nostr-icons").join(format!(
        "{}.img",
        pubkey.chars().take(16).collect::<String>()
    )))
}

/// Signet's gate: the avatar URL is fetched only over https. A client
/// that claims a cleartext avatar gets the fallback like everyone else.
pub fn avatar_url_allowed(url: &str) -> bool {
    url.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_envelope_parses_with_the_contract_fields() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"prompts": [{
                "id": "abc", "app": "pubkey123", "method": "sign_event",
                "summary": "Sign a note", "kind": 1, "kind_label": "Short text note",
                "content": "hello", "sensitive": false, "retries": 3
            }]}"#,
        )
        .unwrap();
        let prompts: Vec<Prompt> = serde_json::from_value(doc["prompts"].clone()).unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].summary, "Sign a note");
        assert_eq!(prompts[0].retries, 3);
        assert_eq!(prompts[0].kind_label.as_deref(), Some("Short text note"));
    }

    #[test]
    fn apps_envelope_parses_with_display_hints() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"apps": [{
                "pubkey": "pubkey123", "level": "basic", "paired_at": 1,
                "name": "Signet", "image": "https://example.com/a.png",
                "perms": "sign_event:1", "url": "https://app.nostr.build/x",
                "request_count": 7, "last_used_at": 2
            }]}"#,
        )
        .unwrap();
        let apps: Vec<PairedApp> = serde_json::from_value(doc["apps"].clone()).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].level, "basic");
        assert_eq!(display_name(&apps[0]), "Signet");
    }

    #[test]
    fn vault_envelope_tolerates_a_missing_inactivity_switch() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"vault": {"exists": true, "unlocked": true, "pubkey": "npub", "relays": [], "connected": [], "uri": null}}"#,
        )
        .unwrap();
        let vault: VaultFact = serde_json::from_value(doc["vault"].clone()).unwrap();
        assert!(vault.exists && vault.unlocked);
        assert!(vault.inactivity.is_none());
    }

    #[test]
    fn a_name_derives_from_the_url_when_the_client_never_claimed_one() {
        let mut app = sample_app();
        app.name = None;
        app.url = Some("https://account.nostr.build/x".into());
        assert_eq!(display_name(&app), "nostr.build");
        app.url = Some("nostr.wine".into());
        assert_eq!(display_name(&app), "nostr.wine");
        app.url = None;
        assert_eq!(display_name(&app), short(&app.pubkey));
    }

    #[test]
    fn relative_time_speaks_ago_words() {
        let now = Local::now().timestamp() as u64;
        assert_eq!(relative(now), "just now");
        assert_eq!(relative(now - 120), "2m ago");
        assert_eq!(relative(now - 7200), "2h ago");
        assert_eq!(relative(now - 172800), "2d ago");
    }

    #[test]
    fn avatar_urls_are_https_only() {
        assert!(avatar_url_allowed("https://example.com/a.png"));
        assert!(!avatar_url_allowed("http://example.com/a.png"));
        assert!(!avatar_url_allowed("ftp://example.com/a.png"));
    }

    fn sample_app() -> PairedApp {
        PairedApp {
            pubkey: "pubkey1234567890".into(),
            level: "ask".into(),
            paired_at: 0,
            name: Some("Signet".into()),
            image: None,
            perms: None,
            url: None,
            revoked_at: None,
            request_count: 0,
            last_used_at: None,
        }
    }
}
