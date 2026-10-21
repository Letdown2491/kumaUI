//! The system updates chip's state: what `kuma update --check` reports
//! about the locked base's movement, checked at boot and then on a
//! long cadence (a check is one round-trip to the registry, or to
//! Fedora's repos for a composed base's kernel; no pull, no build),
//! plus the parse into per-package rows the panel lists.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
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
                        // a current base retires the staged flag: the
                        // staged result was either applied by the
                        // reboot or superseded, and either way there
                        // is nothing waiting on the user anymore
                        if state.updates.is_empty() {
                            state.applied = None;
                            clear_staged();
                        }
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
                state.applied = Some(result.clone());
                if result == ApplyOutcome::Staged {
                    persist_staged();
                } else if read_staged().is_some_and(|id| id == boot_id()) {
                    // a failed retry does not unstage the last good
                    // result: the file still vouches for it, so the
                    // panel keeps saying staged
                    state.applied = Some(ApplyOutcome::Staged);
                }
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

// the staged flag: one small state file beside config.toml, because a
// shell restart must not lie about a staged result. The chip and the
// panel read `applied`, and that lived only in the old process's
// memory: after a crash or a `systemctl --user restart kuma-shell`,
// the panel went back to offering Update now on a base that was
// already staged, and the user would run the whole build again. The
// file carries the boot id of the apply; a reboot is precisely the
// event that retires the flag, so a different boot id on load means
// the stage is over and the check's own verdict takes over.

/// The state file's path: beside config.toml, same resolution.
fn state_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("kuma-shell").join("updates-state.toml")
}

/// This boot's identity, or empty when the kernel does not provide
/// one (a non-Linux test box): an empty id never matches a stored
/// one, so the flag retires instead of sticking forever.
fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|id| id.trim().to_string())
        .unwrap_or_default()
}

/// The stored flag's boot id, if a staged result is on file.
fn read_staged() -> Option<String> {
    read_staged_at(&state_path())
}

fn read_staged_at(path: &PathBuf) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct StagedState {
        staged: bool,
        #[serde(default)]
        boot_id: String,
    }
    let text = std::fs::read_to_string(path).ok()?;
    let state: StagedState = toml::from_str(&text).ok()?;
    state.staged.then_some(state.boot_id)
}

/// Record that this boot's apply staged. Best effort: a failure to
/// write costs the flag on the next restart, never the staging
/// itself, so a warning is the whole complaint.
fn persist_staged() {
    persist_staged_at(&state_path(), &boot_id())
}

fn persist_staged_at(path: &PathBuf, boot: &str) {
    let body = format!("staged = true\nboot_id = \"{boot}\"\n");
    // one atomic rename: a torn state file reads as no state, which
    // would ask the user for a second build of the same stage
    let tmp = path.with_extension("toml.tmp");
    let write = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(err) = write {
        warn!("updates: could not persist the staged flag: {err}");
        let _ = std::fs::remove_file(&tmp);
    }
}

fn clear_staged() {
    clear_staged_at(&state_path())
}

fn clear_staged_at(path: &PathBuf) {
    if let Err(err) = std::fs::remove_file(path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!("updates: could not clear the staged flag: {err}");
        }
    }
}

/// What a stored flag means for this boot: the same boot keeps the
/// stage alive, any other answer (no file, other boot) restores
/// nothing and lets the check's own verdict speak. An unknown current
/// boot id (no /proc) never matches, so the flag retires instead of
/// sticking forever.
fn restore_decision(stored: Option<String>, current: &str) -> Option<ApplyOutcome> {
    if current.is_empty() {
        return None;
    }
    stored
        .filter(|id| id == current)
        .map(|_| ApplyOutcome::Staged)
}

/// Wire the poller up: probe for `kuma`, and only on a hit run a check
/// at boot and one per cadence. On a system without `kuma` the widget
/// stays out of the bar and out of the scheduler entirely.
pub fn run(state: &Entity<UpdatesState>, cx: &mut App) {
    // restore a staged result from before the last process restart:
    // same boot means the stage still stands (a crash, a reinstall,
    // a service restart mid-session must not turn "staged, restart
    // to apply" back into "Update now"), and a different boot means
    // the machine did reboot, which is exactly the event that
    // retires the flag
    state.update(cx, |state, cx| {
        let stored = read_staged();
        state.applied = restore_decision(stored.clone(), &boot_id());
        if stored.is_some() && state.applied.is_none() {
            clear_staged();
        }
        if state.applied.is_some() {
            cx.notify();
        }
    });
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

    #[test]
    fn the_staged_flag_round_trips_and_retires() {
        // no tempfile dependency, so the tests build their own path
        let path = std::env::temp_dir().join(format!("kuma-updates-flag-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // no file: nothing to restore
        assert_eq!(read_staged_at(&path), None);
        assert_eq!(restore_decision(read_staged_at(&path), "boot-a"), None);

        // a staged result survives a process restart on the same boot
        persist_staged_at(&path, "boot-a");
        assert_eq!(read_staged_at(&path).as_deref(), Some("boot-a"));
        assert_eq!(
            restore_decision(read_staged_at(&path), "boot-a"),
            Some(ApplyOutcome::Staged)
        );

        // a reboot is the event that retires it
        assert_eq!(restore_decision(read_staged_at(&path), "boot-b"), None);

        // junk in the file restores nothing and does not panic
        std::fs::write(&path, "staged = false\nboot_id = \"boot-a\"\n").unwrap();
        assert_eq!(read_staged_at(&path), None);
        std::fs::write(&path, "not toml at all").unwrap();
        assert_eq!(read_staged_at(&path), None);

        // clearing a file that is not there is quiet
        clear_staged_at(&path);
        clear_staged_at(&path);
        assert_eq!(read_staged_at(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unknown_boot_never_restores_the_flag() {
        // a stored flag plus no way to know this boot: the flag
        // retires, it does not stick forever
        assert_eq!(restore_decision(Some(String::new()), ""), None);
        assert_eq!(restore_decision(Some("boot-a".into()), ""), None);
    }
}
