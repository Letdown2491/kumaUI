//! The battery threshold notifier: the shell's absorption of the image's
//! kuma-battery-watch script (the end state kumaOS#80 records, the first
//! thing to go when the shell grew threshold notifications). The contract
//! is the script's: warn at 15% (normal) and 5% (critical, "Plug in
//! now."), only while discharging, once per band, re-armed by any plug;
//! a session that opens at 12% on battery warns on the first read; a
//! desktop without a battery never sees a notice.
//!
//! The road is also the script's: `udevadm monitor --kernel
//! --subsystem-match=power_supply`, not a timer. The monitor lives 300
//! seconds and dies of old age, and its death falls through to a safety
//! read of sysfs: a real driver announced the unplug and then stayed
//! SILENT through a whole capacity band (76 down to 15 percent emitted
//! nothing), so a pure event loop would sleep through the threshold it
//! exists to catch. The respawn is the poll: one wakeup per udev event
//! plus 288 safety reads a day, instead of the 1,440 the old 60-second
//! poll cost. With no udevadm the honest floor is a 60-second check,
//! better than a watch that cannot start. The warned state rides this
//! loop, not a subshell, so nothing drops it; the shell is
//! single-instance per session, so the script's flock has no job here.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::Local;
use gpui::{App, AppContext, Entity};
use smol::channel::{Sender, unbounded};

use crate::notifications::{Notification, NotificationState, Urgency};
use crate::sysmon::{Battery, read_battery};

/// The low band's floor: at or below this while discharging, warn.
const LOW: u8 = 15;
/// The critical band's floor: it wins over low, whatever fired first.
const CRITICAL: u8 = 5;
/// The monitor dies of old age after this long; the death is the
/// safety read (the script's `timeout 300`).
const MONITOR_LIFETIME_SECS: u64 = 300;
/// The no-udevadm fallback cadence: the script's poll that cannot
/// exit, the floor under a watch that cannot start.
const FALLBACK_POLL_SECS: u64 = 60;

/// Which band fired last: the warn-once memory (the script's `warned`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Warned {
    #[default]
    Fresh,
    Low,
    Critical,
}

/// What a reading asks the shell to show, carrying the capacity it read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Notice {
    Low(u8),
    Critical(u8),
}

/// One step of the machine, pure and unit-testable: a reading in, the
/// new warning state and any notice out. Anything not Discharging
/// clears the state (a replug re-warns on the way back down); a missing
/// battery changes nothing, the script's early return, so a desktop
/// stays silent; critical wins over low, and a fired band stays quiet
/// until re-armed.
fn evaluate(battery: Option<Battery>, warned: Warned) -> (Warned, Option<Notice>) {
    let Some(battery) = battery else {
        return (warned, None);
    };
    // `on_ac` is parse_battery_status's "not Discharging": Charging,
    // Not charging (plugged, held at a charge threshold), Full, and
    // Unknown all re-arm here, exactly the script's
    // `status != Discharging` branch.
    if battery.on_ac {
        return (Warned::Fresh, None);
    }
    if battery.percent <= CRITICAL && warned != Warned::Critical {
        (Warned::Critical, Some(Notice::Critical(battery.percent)))
    } else if battery.percent <= LOW && warned == Warned::Fresh {
        (Warned::Low, Some(Notice::Low(battery.percent)))
    } else {
        (warned, None)
    }
}

/// The notice as the daemon would have received it from notify-send:
/// the same summaries, the same sentence, the same urgencies.
fn notice_notification(notice: Notice) -> Notification {
    let (summary, body, urgency) = match notice {
        Notice::Low(percent) => (
            format!("Battery low: {percent}%"),
            String::new(),
            Urgency::Normal,
        ),
        Notice::Critical(percent) => (
            format!("Battery critical: {percent}%"),
            "Plug in now.".into(),
            Urgency::Critical,
        ),
    };
    Notification {
        id: 0,
        app_name: "kuma-shell".into(),
        icon: None,
        summary,
        body,
        actions: Vec::new(),
        urgency,
        received_at: Local::now(),
        expire_timeout: -1,
        closed: false,
    }
}

/// Spawn one monitor generation: udevadm under `timeout`, its stdout
/// lines pumped into `events` (one per `change` uevent), and when the
/// stream ends one more send: the safety read. The child is reaped
/// here too, so nothing lingers holding it. False means no udevadm.
fn spawn_monitor(events: Sender<()>) -> bool {
    let Ok(mut child) = Command::new("timeout")
        .arg(MONITOR_LIFETIME_SECS.to_string())
        .arg("udevadm")
        .args(["monitor", "--kernel", "--subsystem-match=power_supply"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // the script's case filter: only a change uevent is a
            // reason to look
            if line.contains("change") {
                let _ = events.try_send(());
            }
        }
        // the monitor died of old age (or otherwise): the death is
        // the safety read, and the respawn below is the poll
        let _ = events.try_send(());
        let _ = child.wait();
    });
    true
}

/// Start the notifier. It holds no windows and pushes through the
/// notification state, so a session that lost its outputs still warns:
/// the toast waits for surfaces to return, the history and unread
/// count land regardless, and the loop dies only when the state does
/// (the weak handle is the exit, the session's other loops' shape).
pub fn run(state: &Entity<NotificationState>, cx: &mut App) {
    let (events_tx, events_rx) = unbounded::<()>();
    let state = state.downgrade();
    cx.spawn(async move |cx| {
        let mut warned = Warned::default();
        loop {
            // the check first: the loop's first pass is the first read,
            // which is not an event. A session that opens at 12% on
            // battery warns now, not at the next capacity step the
            // driver cares to emit.
            let battery = cx.background_spawn(async { read_battery() }).await;
            let (next, notice) = evaluate(battery, warned);
            warned = next;
            if let Some(notice) = notice {
                log::info!("battery: {notice:?}");
                if state
                    .update(cx, |state, cx| state.push(notice_notification(notice), cx))
                    .is_err()
                {
                    break;
                }
            }
            // then the wait for the next excuse to look: one monitor
            // generation's events, ending in its death (the safety
            // read); or, with no udevadm, the fallback tick.
            if spawn_monitor(events_tx.clone()) {
                if events_rx.recv().await.is_err() {
                    break;
                }
            } else {
                cx.background_executor()
                    .timer(Duration::from_secs(FALLBACK_POLL_SECS))
                    .await;
            }
            // a uevent burst is one reading: drain what queued behind
            // the wake before looking
            while events_rx.try_recv().is_ok() {}
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discharging(percent: u8) -> Option<Battery> {
        Some(Battery {
            percent,
            charging: false,
            on_ac: false,
        })
    }

    fn on_ac(percent: u8) -> Option<Battery> {
        Some(Battery {
            percent,
            charging: true,
            on_ac: true,
        })
    }

    #[test]
    fn thresholds_match_the_script() {
        // above both bands, nothing
        assert_eq!(evaluate(discharging(16), Warned::Fresh), (Warned::Fresh, None));
        // at the floors and one step inside: the bands are <=
        assert_eq!(
            evaluate(discharging(15), Warned::Fresh),
            (Warned::Low, Some(Notice::Low(15)))
        );
        assert_eq!(
            evaluate(discharging(5), Warned::Fresh),
            (Warned::Critical, Some(Notice::Critical(5)))
        );
        // six is low, not critical
        assert_eq!(
            evaluate(discharging(6), Warned::Fresh),
            (Warned::Low, Some(Notice::Low(6)))
        );
    }

    #[test]
    fn warn_once_per_band() {
        let (warned, notice) = evaluate(discharging(14), Warned::Fresh);
        assert_eq!(notice, Some(Notice::Low(14)));
        // descending through the band emits one notice, not one per
        // capacity step
        assert_eq!(evaluate(discharging(13), warned), (Warned::Low, None));
        assert_eq!(evaluate(discharging(12), warned), (Warned::Low, None));
        // rising back out without a plug does not fire either
        assert_eq!(evaluate(discharging(16), warned), (Warned::Low, None));
    }

    #[test]
    fn low_escalates_to_critical() {
        let (warned, _) = evaluate(discharging(14), Warned::Fresh);
        assert_eq!(
            evaluate(discharging(4), warned),
            (Warned::Critical, Some(Notice::Critical(4)))
        );
        // and a fired critical band stays quiet below it
        let (warned, _) = evaluate(discharging(4), Warned::Fresh);
        assert_eq!(evaluate(discharging(2), warned), (Warned::Critical, None));
    }

    #[test]
    fn replug_re_arms() {
        let (warned, _) = evaluate(discharging(12), Warned::Fresh);
        // anything not Discharging clears the state: Charging, Not
        // charging, Full, Unknown all read as on_ac here
        assert_eq!(evaluate(on_ac(50), warned), (Warned::Fresh, None));
        // draining back through the band warns again
        assert_eq!(
            evaluate(discharging(14), Warned::Fresh),
            (Warned::Low, Some(Notice::Low(14)))
        );
    }

    #[test]
    fn first_read_warns_immediately() {
        // a session that opens at 12% on battery warns on the first
        // read, not at the next udev event
        assert_eq!(
            evaluate(discharging(12), Warned::Fresh),
            (Warned::Low, Some(Notice::Low(12)))
        );
        // and one that opens at 4% goes straight to critical
        assert_eq!(
            evaluate(discharging(4), Warned::Fresh),
            (Warned::Critical, Some(Notice::Critical(4)))
        );
    }

    #[test]
    fn no_battery_is_silent() {
        // a missing battery changes nothing (the script's early
        // return): no notice, and the state is not disturbed
        assert_eq!(evaluate(None, Warned::Fresh), (Warned::Fresh, None));
        assert_eq!(evaluate(None, Warned::Low), (Warned::Low, None));
    }

    #[test]
    fn notice_texts_match_the_script() {
        let low = notice_notification(Notice::Low(14));
        assert_eq!(low.summary, "Battery low: 14%");
        assert_eq!(low.body, "");
        assert_eq!(low.urgency, Urgency::Normal);
        let critical = notice_notification(Notice::Critical(4));
        assert_eq!(critical.summary, "Battery critical: 4%");
        assert_eq!(critical.body, "Plug in now.");
        assert_eq!(critical.urgency, Urgency::Critical);
    }
}
