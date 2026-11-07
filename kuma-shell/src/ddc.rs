//! External-panel brightness over DDC/CI, through the ddcutil
//! subprocess. Internal panels ride /sys/class/backlight and
//! brightnessctl; an external monitor exposes no backlight, the
//! cable is the only road in, and ddcutil is the boring first cut
//! (an in-process i2c path is the later optimization if the exec
//! churn shows). DDC is slow and flaky in the field: every call is
//! timed and output-capped (the platform's `timed_output`), every
//! parse is total, a monitor that does not answer simply does not
//! appear, and a panel that answered once can refuse later: the
//! refusal surfaces as the panel's error line and the last-known
//! percent stands.

/// One external panel that answers DDC/CI, keyed by ddcutil's
/// display number. `percent` is last-known (the optimistic write's
/// base; None until a read lands), `max` the VCP 10 ceiling the
/// percent math denominates, `error` the last failed operation's
/// honest line, cleared by the next success.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DdcPanel {
    pub display: u8,
    pub model: String,
    pub percent: Option<u8>,
    pub max: u32,
    pub error: Option<String>,
}

/// The panels that answer, as of this pass: one `ddcutil detect`
/// plus one `getvcp 10` read per panel found, background work by
/// the caller's contract. A panel whose read fails lands with
/// `percent: None` and the error line, so the row exists and says
/// what refused. No ddcutil on the system, or no panel answering:
/// an empty list, which is the feature honestly absent.
pub(crate) fn detect() -> Vec<DdcPanel> {
    let output = match kuma_platform::process::timed_output("ddcutil", &["detect"], 30) {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_detect(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .map(|(display, model)| {
            let model = model.unwrap_or_else(|| format!("External display {display}"));
            match read_brightness(display) {
                Ok((percent, max)) => DdcPanel {
                    display,
                    model,
                    percent: Some(percent),
                    max,
                    error: None,
                },
                Err(error) => DdcPanel {
                    display,
                    model,
                    percent: None,
                    max: 100,
                    error: Some(error),
                },
            }
        })
        .collect()
}

/// One absolute brightness write, percent against the panel's VCP
/// 10 max. The caller owns the optimistic state and the rollback;
/// this answers with the honest line when the monitor refuses.
pub(crate) fn set_brightness(display: u8, percent: u8, max: u32) -> Result<(), String> {
    let value = ((percent as u32 * max + 50) / 100).max(1);
    let output = kuma_platform::process::timed_output(
        "ddcutil",
        &[
            "setvcp",
            "10",
            &value.to_string(),
            "--display",
            &display.to_string(),
        ],
        30,
    )
    .map_err(|err| format!("ddcutil setvcp 10 {value} --display {display}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "ddcutil setvcp 10 {value} --display {display}: the monitor refused ({})",
            output.status
        ));
    }
    Ok(())
}

/// (display number, model name) per answering panel, from detect's
/// human format: "Display N:" opens a panel, "Model:" names it. The
/// human shape is the version-stable one (--terse's columns moved
/// between ddcutil releases); a panel without a Model line reads as
/// None and the caller names it by display number.
fn parse_detect(text: &str) -> Vec<(u8, Option<String>)> {
    let mut panels = Vec::new();
    let mut current: Option<(u8, Option<String>)> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Display ") {
            if let Ok(display) = rest.trim_end_matches(':').trim().parse::<u8>() {
                if let Some(done) = current.take() {
                    panels.push(done);
                }
                current = Some((display, None));
            }
        } else if let Some(rest) = trimmed.strip_prefix("Model:") {
            let model = rest.trim();
            if let Some(panel) = current.as_mut()
                && !model.is_empty()
            {
                panel.1 = Some(model.to_string());
            }
        }
    }
    if let Some(done) = current.take() {
        panels.push(done);
    }
    panels
}

/// (percent, max) from one panel's VCP 10 (Luminosity) read:
/// `ddcutil getvcp 10 --display N` reports "Current value = C,
/// Max value = M" per value line. The max denominates the percent
/// math (some panels ship a max of 101, not 100); a read that
/// fails or parses wrong is an honest error line, never a zero.
fn read_brightness(display: u8) -> Result<(u8, u32), String> {
    let output = kuma_platform::process::timed_output(
        "ddcutil",
        &["getvcp", "10", "--display", &display.to_string()],
        30,
    )
    .map_err(|err| format!("ddcutil getvcp 10 --display {display}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "ddcutil getvcp 10 --display {display}: the monitor refused ({})",
            output.status
        ));
    }
    parse_getvcp(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| format!("ddcutil getvcp 10 --display {display}: the answer did not parse"))
}

/// (current, max) off getvcp's value line.
fn parse_getvcp(text: &str) -> Option<(u8, u32)> {
    let line = text.lines().find(|line| line.contains("Current value ="))?;
    let current: u32 = line
        .split("Current value =")
        .nth(1)?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()?;
    let max: u32 = line.split("Max value =").nth(1)?.trim().parse().ok()?;
    if max == 0 {
        return None;
    }
    let percent = ((current as f32 / max as f32) * 100.0).round().min(100.) as u8;
    Some((percent, max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_parses_panels_models_and_display_fallbacks() {
        let text = "\
Display 1:
   I2C bus:                  /dev/i2c-3
   DRM connector:            card0-DP-1
   EDID synopsis:
      Mfg id:                LGP
      Model:                 LG HDR 4K

Display 2:
   I2C bus:                  /dev/i2c-6
   DRM connector:            card0-DP-2
   EDID synopsis:
      Mfg id:                DEL

Display 3:
   I2C bus:                  /dev/i2c-9
";
        let panels = parse_detect(text);
        assert_eq!(panels.len(), 3);
        assert_eq!(panels[0], (1, Some("LG HDR 4K".to_string())));
        assert_eq!(panels[1], (2, None));
        assert_eq!(panels[2], (3, None));
        // junk in the middle must not derail the walk
        let text = "\
not a display line
Display garbage:
Display 4:
   Model:
Display 5:
   Model: Dell U2723QE
";
        let panels = parse_detect(text);
        assert_eq!(panels.len(), 2);
        assert_eq!(panels[0], (4, None));
        assert_eq!(panels[1], (5, Some("Dell U2723QE".to_string())));
    }

    #[test]
    fn getvcp_parses_current_and_max_and_caps_the_percent() {
        let line = "VCP code 0x10 (Luminosity): Current value = 50, Max value = 100";
        assert_eq!(parse_getvcp(line), Some((50, 100)));
        // a max of 101 denominates the percent honestly
        let line = "VCP code 0x10 (Luminosity): Current value = 101, Max value = 101";
        assert_eq!(parse_getvcp(line), Some((100, 101)));
        // zero max parses wrong into None, never a division by zero
        let line = "VCP code 0x10 (Luminosity): Current value = 0, Max value = 0";
        assert_eq!(parse_getvcp(line), None);
        assert_eq!(parse_getvcp("no value line here"), None);
        assert_eq!(parse_getvcp("Current value = , Max value = 100"), None);
    }
}
