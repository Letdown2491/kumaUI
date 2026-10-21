//! The system updates chip's state: what `kuma update --check` reports
//! about the locked base's movement, checked at boot and then on a
//! long cadence (a check is one round-trip to the registry, or to
//! Fedora's repos for a composed base's kernel; no pull, no build),
//! plus the parse into per-package rows the panel lists.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Context, Entity};
use log::warn;
use serde::Deserialize;

/// Check at boot, then every four hours: the check costs a network
/// round-trip, and packages do not move faster than that.
const POLL: Duration = Duration::from_secs(4 * 60 * 60);
/// A check's budget: a slow registry counts as a failure, not a hang.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// One moved package, as `kuma update --check --json` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct PackageUpdate {
    pub name: String,
    pub from: String,
    pub to: String,
    pub severity: Option<String>,
}

/// One check's result: the list is empty when the base is current.
#[derive(Debug, PartialEq)]
pub struct CheckResult {
    pub updates: Vec<PackageUpdate>,
    pub security: u32,
}

#[derive(Default)]
pub struct UpdatesState {
    /// What moved at the last successful check, empty until it lands.
    pub updates: Vec<PackageUpdate>,
    /// How many of them carry a security flag, for the chip's tint.
    pub security: u32,
    /// The last check failed: the old list stays on the bar, dimmed.
    pub stale: bool,
    pub checked_at: Option<Instant>,
    pub checking: bool,
    /// An apply is running: the panel's button is down.
    pub applying: bool,
    /// The apply's live position: the newest step line the build has
    /// printed, a poll behind the pipe. The panel prefers it over the
    /// generic wording, so a slow build says which step it is on.
    pub progress: Option<String>,
    /// The system refused the staged reboot.
    pub reboot_refused: bool,
    /// Whether `kuma` was found on this system. False until the boot
    /// probe lands; the chip, its checks, and its panel are kumaOS
    /// machinery, and on a stock distro there is nothing to drive.
    pub available: bool,
    /// The last apply's outcome, kept until the next one starts. Staged
    /// means the result is waiting for the next reboot.
    pub applied: Option<ApplyOutcome>,
}

/// How an apply ended. Cancellation is its own outcome because a
/// dismissed or denied polkit dialog never ran anything: the base is
/// untouched, and "the update failed" would be a lie.
#[derive(Clone, Debug, PartialEq)]
pub enum ApplyOutcome {
    Staged,
    Cancelled,
    Failed(String),
}

impl UpdatesState {
    /// One check: a bounded run of `kuma update --check --json` off the
    /// foreground, the JSON parsed into rows. A failure keeps the last
    /// list and marks it stale; the chip dims instead of vanishing.
    fn check(&mut self, cx: &mut Context<Self>) {
        if self.checking || !self.available {
            return;
        }
        self.checking = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { check_updates() }).await;
            let _ = this.update(cx, |state, cx| {
                state.checking = false;
                match result {
                    Ok(fresh) => {
                        state.updates = fresh.updates;
                        state.security = fresh.security;
                        state.stale = false;
                        state.checked_at = Some(Instant::now());
                    }
                    Err(err) => {
                        warn!("updates check failed: {err}");
                        state.stale = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Stage the update, then re-check so the panel reflects the moved
    /// lock. The button is down while it builds; the polkit agent
    /// (this shell's lock dialog) carries the password prompt, and the
    /// build's own step lines stream into the panel while it works.
    pub fn apply(&mut self, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
        self.applying = true;
        self.applied = None;
        self.progress = None;
        self.reboot_refused = false;
        cx.notify();
        let lines: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let watch = lines.clone();
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        cx.spawn(async move |this, cx| {
            let handle = cx.background_spawn(async move { apply_updates(watch, flag) });
            // the build's position rides a shared line and a small
            // poll: the entity only moves on the foreground executor,
            // and a seven-tenths cadence is cheap next to a build
            loop {
                cx.background_executor().timer(Duration::from_millis(700)).await;
                if done.load(Ordering::Relaxed) {
                    break;
                }
                let latest = lines.lock().ok().and_then(|guard| guard.clone());
                let _ = this.update(cx, |state, cx| {
                    if state.progress != latest {
                        state.progress = latest;
                        cx.notify();
                    }
                });
            }
            let result = handle.await;
            let _ = this.update(cx, |state, cx| {
                state.applying = false;
                state.progress = None;
                state.applied = Some(result);
                state.check(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Apply what is staged: one click, one reboot. The staging is
    /// done, so this is not an update step; if the system refuses it,
    /// the panel says so and the staged result still stands.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let ok = cx
                .background_spawn(async {
                    Command::new("systemctl")
                        .arg("reboot")
                        .status()
                        .map(|status| status.success())
                        .unwrap_or(false)
                })
                .await;
            if !ok {
                let _ = this.update(cx, |state, cx| {
                    state.reboot_refused = true;
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

/// Run the check to completion. Blocking by design: the caller runs it
/// inside `background_spawn`, and `timeout` bounds the wait.
fn check_updates() -> Result<CheckResult, String> {
    let output = Command::new("timeout")
        .args([
            &format!("{}", CHECK_TIMEOUT.as_secs()),
            "kuma",
            "update",
            "--check",
            "--json",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|err| format!("spawn failed: {err}"))?;
    if !output.status.success() {
        return Err(format!("exited {}", output.status));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_check(&text)
}

/// Stage the update: `pkexec` hands the password prompt to the polkit
/// agent (this shell's lock dialog), and `kuma update --yes` pulls,
/// rebuilds, and stages for the next reboot. An image build is not a
/// bounded wait: the timeout is generous by design.
/// Run the apply: `pkexec kuma update` under the generous timeout,
/// with the build's step lines sipped off its stderr by a reader
/// thread (podman numbers its steps, `COMMIT` closes the build, and
/// the post-build staging speaks in dashes). A seven-tenths poll on
/// the far side lifts the newest line into the panel.
fn apply_updates(watch: Arc<Mutex<Option<String>>>, done: Arc<AtomicBool>) -> ApplyOutcome {
    let mut child = match Command::new("timeout")
        .args(["3600", "pkexec", "kuma", "update", "--yes", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            done.store(true, Ordering::Relaxed);
            return ApplyOutcome::Failed(format!("spawn failed: {err}"));
        }
    };
    // kuma's stdout carries the JSON report at the end; the pipe must
    // drain or the writer blocks, so a quiet thread swallows it
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let _ = BufReader::new(stdout).lines().count();
        });
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let line = line.trim_end();
                let keep = line.starts_with("STEP ")
                    || line.starts_with("COMMIT")
                    || line.starts_with("--> ")
                    || line.starts_with("Staging")
                    || line.starts_with("Queued ");
                if keep {
                    // the reader caps the line so a \\r-refreshing
                    // progress bar cannot pile the whole stream into
                    // one shared string
                    let capped: String = line.chars().take(160).collect();
                    if let Ok(mut guard) = watch.lock() {
                        *guard = Some(capped);
                    }
                }
            }
        });
    }
    let outcome = match child.wait() {
        Ok(status) => match status.code() {
            Some(0) => ApplyOutcome::Staged,
            // pkexec's own code for a dismissed auth dialog (126) and
            // for an authorization it could not obtain (127): the
            // update never started, so there is nothing failed to report
            Some(126) | Some(127) => ApplyOutcome::Cancelled,
            code => ApplyOutcome::Failed(format!(
                "kuma update exited {}",
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "from a signal".into())
            )),
        },
        Err(err) => ApplyOutcome::Failed(format!("spawn failed: {err}")),
    };
    done.store(true, Ordering::Relaxed);
    outcome
}

/// The check's JSON into rows. `ok: false` or a missing `updates`
/// object reads as a failure; an empty `moved` list is a current base.
fn parse_check(text: &str) -> Result<CheckResult, String> {
    #[derive(Deserialize)]
    struct CheckJson {
        #[serde(default)]
        ok: bool,
        updates: Option<UpdatesJson>,
    }
    #[derive(Deserialize)]
    struct UpdatesJson {
        #[serde(default)]
        moved: Vec<MovedJson>,
        #[serde(default)]
        security: u32,
    }
    #[derive(Deserialize)]
    struct MovedJson {
        name: String,
        #[serde(default)]
        from: String,
        #[serde(default)]
        to: String,
        severity: Option<String>,
    }

    let json: CheckJson =
        serde_json::from_str(text).map_err(|err| format!("unparseable json: {err}"))?;
    let updates = match json.updates {
        Some(updates) if json.ok => updates,
        _ => return Err("the check did not succeed".to_string()),
    };
    Ok(CheckResult {
        updates: updates
            .moved
            .into_iter()
            .map(|m| PackageUpdate {
                name: m.name,
                from: m.from,
                to: m.to,
                severity: m.severity,
            })
            .collect(),
        security: updates.security,
    })
}

/// Whether this system has `kuma` at all: the widget is built for
/// kumaOS, and a stock distro has nothing for it to drive. Set once by
/// the boot probe; everything that renders the widget asks here, since
/// a capability of the machine is not one view's state to hold.
static KUMA_PROBE: OnceLock<bool> = OnceLock::new();

pub fn kuma_available() -> bool {
    *KUMA_PROBE.get().unwrap_or(&false)
}

/// The capability test: does `kuma` exist on PATH? The checks and the
/// apply both invoke it bare, so its presence under the same
/// environment is the whole test; there is no deeper "am I on kumaOS"
/// mark worth chasing.
fn detect_kuma() -> bool {
    Command::new("sh")
        .args(["-c", "command -v kuma >/dev/null 2>&1"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Wire the poller up: probe for `kuma`, and only on a hit run a check
/// at boot and one per cadence. On a system without `kuma` the widget
/// stays out of the bar and out of the scheduler entirely.
pub fn run(state: &Entity<UpdatesState>, cx: &mut App) {
    let probe = state.downgrade();
    cx.spawn(async move |cx| {
        let found = cx.background_spawn(async { detect_kuma() }).await;
        let _ = probe.update(cx, |state, cx| {
            let _ = KUMA_PROBE.set(found);
            state.available = found;
            if found {
                state.check(cx);
            }
            cx.notify();
        });
    })
    .detach();
    let state = state.downgrade();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(POLL).await;
            let _ = state.update(cx, |state, cx| {
                if state.available {
                    state.check(cx)
                }
            });
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_check_json_parses() {
        // lifted from a live `kuma update --check --json`, trimmed
        let json = r#"{"actions":[],"base":"localhost/kuma-base:m38","composed":true,
            "fedora_release":{"changed":false,"current":"44","from":null,"to":null},
            "locked":true,"manifest_changed":false,"ok":true,
            "updates":{"checked":true,
              "moved":[
                {"from":"3.13.3-1.fc44.x86_64","name":"libaom","severity":"important","to":"3.15.2-1.fc44.x86_64"},
                {"from":"1.16.13-1.fc44.x86_64","name":"bootc","severity":null,"to":"1.17.1-1.fc44.x86_64"}
              ],
              "security":1,"source":"machine"}}"#;
        let result = parse_check(json).unwrap();
        assert_eq!(result.updates.len(), 2);
        assert_eq!(result.security, 1);
        assert_eq!(result.updates[0].name, "libaom");
        assert_eq!(result.updates[0].severity.as_deref(), Some("important"));
        assert_eq!(result.updates[1].severity, None);
    }

    #[test]
    fn a_current_base_moves_nothing() {
        let json = r#"{"ok":true,"updates":{"checked":true,"moved":[],"security":0}}"#;
        let result = parse_check(json).unwrap();
        assert!(result.updates.is_empty());
        assert_eq!(result.security, 0);
    }

    #[test]
    fn an_unsuccessful_check_is_an_error_not_an_empty_list() {
        // an empty list means "up to date"; a failed check means
        // nothing, and conflating them would hide a real batch
        assert!(parse_check(r#"{"ok":false,"updates":{"moved":[],"security":0}}"#).is_err());
        assert!(parse_check(r#"{"ok":true}"#).is_err());
        assert!(parse_check("not json").is_err());
    }
}
