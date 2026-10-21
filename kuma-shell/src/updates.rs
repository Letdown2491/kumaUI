//! The system updates chip's state: what `kuma update --check` reports
//! about the locked base's movement, checked at boot and then on a
//! long cadence (a check is one round-trip to the registry, or to
//! Fedora's repos for a composed base's kernel; no pull, no build),
//! plus the parse into per-package rows the panel lists.

use std::process::Command;
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
}

impl UpdatesState {
    /// One check: a bounded run of `kuma update --check --json` off the
    /// foreground, the JSON parsed into rows. A failure keeps the last
    /// list and marks it stale; the chip dims instead of vanishing.
    fn check(&mut self, cx: &mut Context<Self>) {
        if self.checking {
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

/// Wire the poller up: a check at boot, then one per cadence. The
/// shell calls this once at boot beside the other `run` fns.
pub fn run(state: &Entity<UpdatesState>, cx: &mut App) {
    state.update(cx, |state, cx| state.check(cx));
    let state = state.downgrade();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(POLL).await;
            let _ = state.update(cx, |state, cx| state.check(cx));
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
