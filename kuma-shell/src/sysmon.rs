use std::{process::Command, time::Duration};

use anyhow::Context as _;
use gpui::{App, AppContext, Context, Entity};

#[derive(Clone, Copy, Debug)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
    pub on_ac: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Volume {
    pub percent: u8,
    pub muted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Brightness {
    pub percent: u8,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BluetoothState {
    pub enabled: bool,
    pub devices: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkState {
    pub online: bool,
    pub wifi: bool,
    pub ssid: Option<String>,
    /// The Wi-Fi radio itself (nmcli radio wifi), distinct from being
    /// connected: quick settings toggles the radio.
    pub wifi_enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PowerProfile {
    #[default]
    Balanced,
    Performance,
    PowerSaver,
}

impl PowerProfile {
    /// The string powerprofilesctl speaks.
    pub fn as_str(self) -> &'static str {
        match self {
            PowerProfile::Performance => "performance",
            PowerProfile::Balanced => "balanced",
            PowerProfile::PowerSaver => "power-saver",
        }
    }

    fn parse(text: &str) -> Option<PowerProfile> {
        match text.trim() {
            "performance" => Some(PowerProfile::Performance),
            "balanced" => Some(PowerProfile::Balanced),
            "power-saver" => Some(PowerProfile::PowerSaver),
            _ => None,
        }
    }
}

/// The picker order for the quick page's segmented control.
pub const PROFILES: [PowerProfile; 3] = [
    PowerProfile::PowerSaver,
    PowerProfile::Balanced,
    PowerProfile::Performance,
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Playback {
    #[default]
    Playing,
    Paused,
    Stopped,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaState {
    pub player: String,
    pub status: Playback,
    pub artist: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecordingState {
    pub elapsed_secs: u64,
}

fn read_recording() -> Option<RecordingState> {
    let out = Command::new("pgrep")
        .args(["-x", "wf-recorder"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let pid: u32 = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(RecordingState {
        elapsed_secs: parse_recording_stat(&stat, uptime)?,
    })
}

#[derive(Clone, Copy, Default)]
struct CpuSample {
    idle: u64,
    total: u64,
}

#[derive(Default)]
pub struct SysMon {
    pub battery: Option<Battery>,
    pub volume: Option<Volume>,
    pub brightness: Option<Brightness>,
    pub cpu: Option<f32>,
    pub bluetooth: Option<BluetoothState>,
    pub network: Option<NetworkState>,
    pub power_profile: Option<PowerProfile>,
    pub media: Option<MediaState>,
    pub recording: Option<RecordingState>,
}

/// A volume change request: `Set` carries an already-clamped target (the GUI's
/// optimistic path); `Change` lets wpctl read live volume first (the MSG CLI
/// path, and the GUI's fallback before the first snapshot).
#[derive(Clone, Copy)]
enum VolumeRequest {
    Set(u8),
    Change(i32),
}

impl VolumeRequest {
    fn execute(self) -> anyhow::Result<()> {
        match self {
            VolumeRequest::Set(percent) => set_volume(percent),
            VolumeRequest::Change(delta) => change_volume(delta),
        }
    }
}

impl SysMon {
    /// One request seam for volume changes (ADR-0006): optimistic snapshot
    /// write, spawn, rollback on error. The MSG CLI uses the cx-free
    /// `change_volume` instead; both share `clamp_percent`.
    pub fn request_volume(&mut self, delta: i32, cx: &mut Context<Self>) {
        let previous = self.volume;
        let request = match previous {
            Some(current) => VolumeRequest::Set(clamp_percent(current.percent as i32, delta)),
            None => VolumeRequest::Change(delta),
        };
        if let (VolumeRequest::Set(percent), Some(current)) = (request, previous) {
            self.volume = Some(Volume { percent, ..current });
            cx.notify();
        }
        cx.spawn(async move |this, cx| {
            if let Err(err) = cx.background_spawn(async move { request.execute() }).await {
                log::error!("volume request failed: {err:#}");
                let _ = this.update(cx, |sysmon, cx| {
                    sysmon.volume = previous; // rollback; the next poll reconciles
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// One request seam for mute changes (ADR-0006).
    /// One request seam for mute changes (ADR-0006).
    pub fn request_mute_toggle(&mut self, cx: &mut Context<Self>) {
        let previous = self.volume;
        if let Some(current) = previous {
            self.volume = Some(Volume {
                muted: !current.muted,
                ..current
            });
            cx.notify();
        }
        cx.spawn(async move |this, cx| {
            if let Err(err) = cx.background_spawn(async move { toggle_mute() }).await {
                log::error!("mute toggle failed: {err:#}");
                let _ = this.update(cx, |sysmon, cx| {
                    sysmon.volume = previous; // rollback; the next poll reconciles
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// One request seam for play/pause (ADR-0006): optimistic status flip,
    /// spawn, rollback on error. The MSG CLI uses the cx-free `play_pause`.
    pub fn request_play_pause(&mut self, cx: &mut Context<Self>) {
        let previous = self.media.clone();
        if let Some(current) = &mut self.media {
            current.status = match current.status {
                Playback::Playing => Playback::Paused,
                Playback::Paused | Playback::Stopped => Playback::Playing,
            };
            cx.notify();
        }
        cx.spawn(async move |this, cx| {
            if let Err(err) = cx.background_spawn(async move { play_pause() }).await {
                log::error!("play-pause failed: {err:#}");
                let _ = this.update(cx, |sysmon, cx| {
                    sysmon.media = previous; // rollback; the next poll reconciles
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// One request seam for skipping tracks (+1 next, -1 previous). No
    /// optimistic write: the title arrives with the next poll.
    pub fn request_skip(&mut self, delta: i32, cx: &mut Context<Self>) {
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_spawn(async move {
                    if delta >= 0 {
                        next_track()
                    } else {
                        previous_track()
                    }
                })
                .await;
            if let Err(err) = result {
                log::error!("track skip failed: {err:#}");
            }
        })
        .detach();
    }

    /// Absolute volume set (the quick-settings slider path): optimistic
    /// snapshot write, spawn, rollback on error, same ritual as
    /// `request_volume`, minus the delta math.
    pub fn request_set_volume(&mut self, percent: u8, cx: &mut Context<Self>) {
        let previous = self.volume;
        if let Some(current) = previous {
            self.volume = Some(Volume { percent, ..current });
            cx.notify();
        }
        cx.spawn(async move |this, cx| {
            let request = VolumeRequest::Set(percent);
            if let Err(err) = cx.background_spawn(async move { request.execute() }).await {
                log::error!("volume request failed: {err:#}");
                let _ = this.update(cx, |sysmon, cx| {
                    sysmon.volume = previous; // rollback; the next poll reconciles
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// One request seam for brightness (the quick-settings slider): optimistic
    /// snapshot write, spawn, rollback on error.
    /// One request seam for brightness (the quick-settings slider): optimistic
    /// snapshot write, spawn, rollback on error.
    pub fn request_set_brightness(&mut self, percent: u8, cx: &mut Context<Self>) {
        let previous = self.brightness;
        if previous.is_some() {
            self.brightness = Some(Brightness { percent });
            cx.notify();
        }
        cx.spawn(async move |this, cx| {
            if let Err(err) = cx
                .background_spawn(async move { set_brightness(percent) })
                .await
            {
                log::error!("brightness request failed: {err:#}");
                let _ = this.update(cx, |sysmon, cx| {
                    sysmon.brightness = previous; // rollback; the next poll reconciles
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Brightness by delta (the bar widget's scroll wheel): optimistic
    /// snapshot write, spawn, rollback on error; mirrors `request_volume`.
    pub fn request_brightness(&mut self, delta: i32, cx: &mut Context<Self>) {
        let previous = self.brightness;
        if let Some(current) = previous
            && let percent = (current.percent as i32 + delta).clamp(0, 100) as u8
        {
            self.brightness = Some(Brightness { percent });
            cx.notify();
        }
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let current = read_brightness().context("no backlight found")?;
                    let new = (current.percent as i32 + delta).clamp(0, 100) as u8;
                    set_brightness(new)
                })
                .await;
            if let Err(err) = result {
                log::error!("brightness request failed: {err:#}");
                let _ = this.update(cx, |sysmon, cx| {
                    sysmon.brightness = previous; // rollback; the next poll reconciles
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Toggle the Wi-Fi radio. Optimistic flip with rollback; with no
    /// snapshot to flip, reads the radio live first (the change-style path).
    pub fn request_wifi_toggle(&mut self, cx: &mut Context<Self>) {
        let previous = self.network.clone();
        if let Some(network) = &mut self.network {
            network.wifi_enabled = !network.wifi_enabled;
            cx.notify();
        }
        let target = previous.as_ref().map(|network| !network.wifi_enabled);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let enabled = target.map(Ok).unwrap_or_else(|| {
                        read_wifi_radio()
                            .map(|enabled| !enabled)
                            .context("no wifi radio state to toggle")
                    })?;
                    set_wifi_radio(enabled)
                })
                .await;
            if let Err(err) = result {
                log::error!("wifi toggle failed: {err:#}");
                let _ = this.update(cx, |sysmon, cx| {
                    sysmon.network = previous; // rollback; the next poll reconciles
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Toggle Bluetooth power. Optimistic flip with rollback; with no
    /// snapshot, reads live first.
    pub fn request_bluetooth_toggle(&mut self, cx: &mut Context<Self>) {
        let previous = self.bluetooth.clone();
        if let Some(bluetooth) = &mut self.bluetooth {
            bluetooth.enabled = !bluetooth.enabled;
            cx.notify();
        }
        let target = previous.as_ref().map(|bluetooth| !bluetooth.enabled);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let enabled = target.map(Ok).unwrap_or_else(|| {
                        read_bluetooth()
                            .map(|bluetooth| !bluetooth.enabled)
                            .context("no bluetooth state to toggle")
                    })?;
                    set_bluetooth_power(enabled)
                })
                .await;
            if let Err(err) = result {
                log::error!("bluetooth toggle failed: {err:#}");
                let _ = this.update(cx, |sysmon, cx| {
                    sysmon.bluetooth = previous; // rollback; the next poll reconciles
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Switch the power profile. Optimistic write with rollback.
    pub fn request_power_profile(&mut self, profile: PowerProfile, cx: &mut Context<Self>) {
        let previous = self.power_profile;
        if previous.is_some() {
            self.power_profile = Some(profile);
            cx.notify();
        }
        cx.spawn(async move |this, cx| {
            if let Err(err) = cx
                .background_spawn(async move { set_power_profile(profile) })
                .await
            {
                log::error!("power profile request failed: {err:#}");
                let _ = this.update(cx, |sysmon, cx| {
                    sysmon.power_profile = previous; // rollback; the next poll reconciles
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

pub fn run(state: &Entity<SysMon>, cx: &mut App) {
    let state = state.downgrade();
    cx.spawn(async move |cx| {
        let mut previous: Option<CpuSample> = None;
        loop {
            let (snapshot, previous_next) = cx
                .background_spawn(async move {
                    let mut previous = previous;
                    let snapshot = refresh(&mut previous);
                    (snapshot, previous)
                })
                .await;
            previous = previous_next;
            if state
                .update(cx, |sysmon, cx| {
                    sysmon.battery = snapshot.battery;
                    sysmon.volume = snapshot.volume;
                    sysmon.brightness = snapshot.brightness;
                    sysmon.cpu = snapshot.cpu;
                    sysmon.bluetooth = snapshot.bluetooth.clone();
                    sysmon.network = snapshot.network.clone();
                    sysmon.power_profile = snapshot.power_profile;
                    sysmon.media = snapshot.media.clone();
                    sysmon.recording = snapshot.recording.clone();
                    log::info!("sysmon snapshot: {snapshot:?}");
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
            cx.background_executor().timer(Duration::from_secs(2)).await;
        }
    })
    .detach();
}

#[derive(Clone, Debug, Default)]
struct Snapshot {
    battery: Option<Battery>,
    volume: Option<Volume>,
    brightness: Option<Brightness>,
    cpu: Option<f32>,
    bluetooth: Option<BluetoothState>,
    network: Option<NetworkState>,
    power_profile: Option<PowerProfile>,
    media: Option<MediaState>,
    recording: Option<RecordingState>,
}

fn refresh(previous: &mut Option<CpuSample>) -> Snapshot {
    Snapshot {
        battery: read_battery(),
        volume: read_volume(),
        brightness: read_brightness(),
        cpu: sample_cpu_usage(previous),
        bluetooth: read_bluetooth(),
        network: read_network(),
        power_profile: read_power_profile(),
        media: read_media(),
        recording: read_recording(),
    }
}

pub fn set_volume(percent: u8) -> anyhow::Result<()> {
    let output = Command::new("wpctl")
        .args(["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{percent}%")])
        .output()?;
    anyhow::ensure!(output.status.success(), "wpctl set-volume failed");
    Ok(())
}

/// The one clamp: wpctl happily amplifies past 100% otherwise
fn clamp_percent(current: i32, delta: i32) -> u8 {
    (current + delta).clamp(0, 100) as u8
}

pub fn change_volume(percent_delta: i32) -> anyhow::Result<()> {
    // read current and clamp: wpctl happily amplifies past 100% otherwise
    let current = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()?;
    anyhow::ensure!(current.status.success(), "wpctl get-volume failed");
    let current = parse_wpctl_volume(&String::from_utf8_lossy(&current.stdout))
        .context("unexpected wpctl volume output")?;
    let new = clamp_percent(current.percent as i32, percent_delta);
    set_volume(new)
}

pub fn toggle_mute() -> anyhow::Result<()> {
    let output = Command::new("wpctl")
        .args(["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"])
        .output()?;
    anyhow::ensure!(output.status.success(), "wpctl set-mute failed");
    Ok(())
}

/// The MSG CLI path: read current brightness and step it, clamped.
pub fn change_brightness(percent_delta: i32) -> anyhow::Result<()> {
    let current = read_brightness().context("no backlight found")?;
    let new = (current.percent as i32 + percent_delta).clamp(0, 100) as u8;
    set_brightness(new)
}

pub fn play_pause() -> anyhow::Result<()> {
    playerctl(["play-pause"])
}

pub fn next_track() -> anyhow::Result<()> {
    playerctl(["next"])
}

pub fn previous_track() -> anyhow::Result<()> {
    playerctl(["previous"])
}

pub fn stop_track() -> anyhow::Result<()> {
    playerctl(["stop"])
}

fn playerctl<const N: usize>(args: [&str; N]) -> anyhow::Result<()> {
    let output = Command::new("playerctl").args(args).output()?;
    anyhow::ensure!(output.status.success(), "playerctl {args:?} failed");
    Ok(())
}

fn read_media() -> Option<MediaState> {
    let output = Command::new("playerctl")
        .args([
            "metadata",
            "--format",
            "{{status}}\t{{player}}\t{{artist}}\t{{title}}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_playerctl_metadata(&String::from_utf8_lossy(&output.stdout))
}

// `playerctl metadata --format` with the four template fields separated by
// tabs; the active player is whatever playerctl targets by default
fn parse_playerctl_metadata(text: &str) -> Option<MediaState> {
    let line = text.lines().next()?;
    let mut fields = line.splitn(4, '\t');
    let status = parse_playback(fields.next()?)?;
    let player = fields.next()?.to_string();
    let artist = fields.next()?.to_string();
    let title = fields.next()?.to_string();
    Some(MediaState {
        player,
        status,
        artist,
        title,
    })
}

fn parse_playback(status: &str) -> Option<Playback> {
    match status.trim() {
        "Playing" => Some(Playback::Playing),
        "Paused" => Some(Playback::Paused),
        "Stopped" => Some(Playback::Stopped),
        _ => None,
    }
}

pub fn set_bluetooth_power(enabled: bool) -> anyhow::Result<()> {
    let output = Command::new("bluetoothctl")
        .args(["power", if enabled { "on" } else { "off" }])
        .output()?;
    anyhow::ensure!(output.status.success(), "bluetoothctl power failed");
    Ok(())
}

fn read_bluetooth() -> Option<BluetoothState> {
    let show = Command::new("bluetoothctl").arg("show").output().ok()?;
    if !show.status.success() {
        return None;
    }
    let enabled = String::from_utf8_lossy(&show.stdout)
        .lines()
        .any(|line| line.trim().starts_with("Powered:") && line.contains("yes"));

    let mut devices = Vec::new();
    if enabled
        && let Ok(connected) = Command::new("bluetoothctl")
            .args(["devices", "Connected"])
            .output()
        && connected.status.success()
    {
        for line in String::from_utf8_lossy(&connected.stdout).lines() {
            if let Some((_, rest)) = line.split_once(' ')
                && let Some((_, name)) = rest.split_once(' ')
            {
                devices.push(name.trim().to_string());
            }
        }
    }
    Some(BluetoothState { enabled, devices })
}

fn read_network() -> Option<NetworkState> {
    let connectivity = Command::new("nmcli")
        .args(["networking", "connectivity"])
        .output()
        .ok()?;
    if !connectivity.status.success() {
        return None;
    }
    let online = String::from_utf8_lossy(&connectivity.stdout).trim() != "none";

    let mut network = NetworkState {
        online,
        wifi: false,
        ssid: None,
        wifi_enabled: read_wifi_radio().unwrap_or(false),
    };
    if let Ok(status) = Command::new("nmcli")
        .args(["-t", "-f", "TYPE,STATE,CONNECTION", "device", "status"])
        .output()
        && status.status.success()
    {
        apply_device_status(&mut network, &String::from_utf8_lossy(&status.stdout));
    }
    Some(network)
}

/// `nmcli radio wifi` prints "enabled" / "disabled".
fn read_wifi_radio() -> Option<bool> {
    let output = Command::new("nmcli")
        .args(["radio", "wifi"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim() == "enabled")
}

pub fn set_wifi_radio(enabled: bool) -> anyhow::Result<()> {
    let output = Command::new("nmcli")
        .args(["radio", "wifi", if enabled { "on" } else { "off" }])
        .output()?;
    anyhow::ensure!(output.status.success(), "nmcli radio wifi failed");
    Ok(())
}

fn read_power_profile() -> Option<PowerProfile> {
    let output = Command::new("powerprofilesctl")
        .args(["get"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    PowerProfile::parse(&String::from_utf8_lossy(&output.stdout))
}

pub fn set_power_profile(profile: PowerProfile) -> anyhow::Result<()> {
    let output = Command::new("powerprofilesctl")
        .args(["set", profile.as_str()])
        .output()?;
    anyhow::ensure!(output.status.success(), "powerprofilesctl set failed");
    Ok(())
}

// `nmcli -t` device lines: TYPE:STATE:CONNECTION with short type names
fn apply_device_status(network: &mut NetworkState, output: &str) {
    for line in output.lines() {
        let fields: Vec<&str> = line.splitn(3, ':').collect();
        if fields.len() == 3 && fields[1] == "connected" {
            match fields[0] {
                "wifi" | "802-11-wireless" => {
                    network.wifi = true;
                    network.ssid = Some(fields[2].to_string());
                    return;
                }
                "ethernet" | "802-3-ethernet" => {
                    network.wifi = false;
                    network.ssid = None;
                    return;
                }
                _ => {}
            }
        }
    }
}

fn read_brightness() -> Option<Brightness> {
    let backlight = std::fs::read_dir("/sys/class/backlight")
        .ok()?
        .next()?
        .ok()?;
    let dir = backlight.path();
    let current: u32 = std::fs::read_to_string(dir.join("brightness"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let max: u32 = std::fs::read_to_string(dir.join("max_brightness"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Brightness {
        percent: brightness_percent(current, max)?,
    })
}

fn brightness_percent(current: u32, max: u32) -> Option<u8> {
    if max == 0 {
        return None;
    }
    Some(((current as f32 / max as f32) * 100.0).round().min(100.) as u8)
}

pub fn set_brightness(percent: u8) -> anyhow::Result<()> {
    let output = Command::new("brightnessctl")
        .args(["set", &format!("{percent}%")])
        .output()?;
    anyhow::ensure!(output.status.success(), "brightnessctl set failed");
    Ok(())
}

fn read_battery() -> Option<Battery> {
    let supplies = std::fs::read_dir("/sys/class/power_supply").ok()?;
    let battery = supplies
        .filter_map(|entry| entry.ok())
        .find(|entry| entry.file_name().to_string_lossy().starts_with("BAT"))?;
    let capacity = std::fs::read_to_string(battery.path().join("capacity")).ok()?;
    let status = std::fs::read_to_string(battery.path().join("status")).ok()?;
    let (charging, on_ac) = parse_battery_status(&status);
    Some(Battery {
        percent: capacity.trim().parse().ok()?,
        charging,
        on_ac,
    })
}

// "Charging" / "Not charging" (plugged, held at a charge threshold) / "Discharging" / "Full"
fn parse_battery_status(status: &str) -> (bool, bool) {
    let status = status.trim();
    (status == "Charging", status != "Discharging")
}

// `wpctl get-volume` prints "Volume: 0.75" and appends " [MUTED]" when muted
fn parse_wpctl_volume(text: &str) -> Option<Volume> {
    let level: f32 = text.split_whitespace().nth(1)?.parse().ok()?;
    Some(Volume {
        percent: (level * 100.0).round() as u8,
        muted: text.contains("[MUTED]"),
    })
}

fn read_volume() -> Option<Volume> {
    let output = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_wpctl_volume(&String::from_utf8_lossy(&output.stdout))
}

// `stat` is the contents of /proc/<pid>/stat: starttime is field 22
// (1-indexed); fields after the comm parenthesis start at field 3, so it's
// index 19 of the post-paren fields. Clock ticks are 100/sec.
fn parse_recording_stat(stat: &str, uptime_secs: f64) -> Option<u64> {
    let fields: Vec<&str> = stat.rsplit(')').next()?.split_whitespace().collect();
    let starttime_ticks: f64 = fields.get(19)?.parse().ok()?;
    let elapsed = uptime_secs - starttime_ticks / 100.0;
    Some(elapsed.max(0.0) as u64)
}

fn sample_cpu_usage(previous: &mut Option<CpuSample>) -> Option<f32> {
    let line = std::fs::read_to_string("/proc/stat").ok()?;
    let sample = parse_cpu_stat(&line)?;
    let usage = previous
        .take()
        .and_then(|previous| cpu_delta(previous, sample));
    *previous = Some(sample);
    usage
}

fn parse_cpu_stat(line: &str) -> Option<CpuSample> {
    let fields: Vec<u64> = line
        .lines()
        .next()?
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    if fields.len() < 5 {
        return None;
    }
    Some(CpuSample {
        idle: fields[3] + fields[4],
        total: fields.iter().sum(),
    })
}

fn cpu_delta(previous: CpuSample, sample: CpuSample) -> Option<f32> {
    let total = sample.total.saturating_sub(previous.total);
    let idle = sample.idle.saturating_sub(previous.idle);
    if total == 0 {
        None
    } else {
        Some((1.0 - idle as f32 / total as f32).max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_delta_computes_usage_fraction() {
        let previous = CpuSample {
            idle: 1000,
            total: 2000,
        };
        let busy = CpuSample {
            idle: 1000,
            total: 3000,
        };
        let idle = CpuSample {
            idle: 1500,
            total: 3000,
        };
        assert_eq!(cpu_delta(previous, busy), Some(1.0));
        assert_eq!(cpu_delta(previous, idle), Some(0.5));
        assert_eq!(cpu_delta(previous, previous), None);
    }

    #[test]
    fn cpu_stat_line_parses() {
        let sample = parse_cpu_stat("cpu  99106 646 27178 5737528 25604 0 9318 0 0 0\n").unwrap();
        assert_eq!(sample.idle, 5737528 + 25604);
        assert_eq!(
            sample.total,
            99106 + 646 + 27178 + 5737528 + 25604 + 0 + 9318 + 0 + 0 + 0
        );
        assert!(parse_cpu_stat("garbage").is_none());
    }

    #[test]
    fn battery_status_maps_kernel_states() {
        assert_eq!(parse_battery_status("Charging\n"), (true, true));
        assert_eq!(parse_battery_status("Not charging\n"), (false, true));
        assert_eq!(parse_battery_status("Full\n"), (false, true));
        assert_eq!(parse_battery_status("Discharging\n"), (false, false));
    }

    #[test]
    fn nmcli_device_lines_find_wifi_ssid() {
        let mut network = NetworkState {
            online: true,
            ..Default::default()
        };
        apply_device_status(
            &mut network,
            "wifi:connected:The Vixen\nloopback:connected (externally):lo\nethernet:unavailable:",
        );
        assert!(network.wifi);
        assert_eq!(network.ssid.as_deref(), Some("The Vixen"));

        let mut network = NetworkState {
            online: true,
            ..Default::default()
        };
        apply_device_status(
            &mut network,
            "loopback:connected (externally):lo\nethernet:connected:Wired",
        );
        assert!(!network.wifi);
        assert!(network.ssid.is_none());
    }

    #[test]
    fn wpctl_volume_output_parses() {
        assert_eq!(
            parse_wpctl_volume("Volume: 0.75\n"),
            Some(Volume {
                percent: 75,
                muted: false
            })
        );
        assert_eq!(
            parse_wpctl_volume("Volume: 0.42 [MUTED]\n"),
            Some(Volume {
                percent: 42,
                muted: true
            })
        );
        assert_eq!(parse_wpctl_volume("Volume: nonsense"), None);
        assert_eq!(parse_wpctl_volume("garbage"), None);
    }

    #[test]
    fn recording_stat_indexes_starttime_correctly() {
        // post-paren fields numbered "3 4 5 …": field 22 sits at index 19
        let stat = format!(
            "42 (wf-recorder) R {}",
            (3..=40)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert_eq!(parse_recording_stat(&stat, 100.0), Some(99));
        assert_eq!(parse_recording_stat(&stat, 0.0), Some(0)); // started "in the future" clamps
        assert_eq!(parse_recording_stat("42 (wf-recorder) R 3 4", 100.0), None);
    }

    #[test]
    fn clamp_percent_keeps_wpctl_from_amplifying() {
        assert_eq!(clamp_percent(96, 5), 100);
        assert_eq!(clamp_percent(3, -5), 0);
        assert_eq!(clamp_percent(50, 5), 55);
        assert_eq!(clamp_percent(50, -50), 0);
    }

    #[test]
    fn playerctl_metadata_parses_tab_separated_fields() {
        assert_eq!(
            parse_playerctl_metadata("Playing\tfirefox\tNirvana\tCome As You Are\n"),
            Some(MediaState {
                player: "firefox".into(),
                status: Playback::Playing,
                artist: "Nirvana".into(),
                title: "Come As You Are".into(),
            })
        );
        // empty artist leaves adjacent tabs; title may itself contain tabs
        assert_eq!(
            parse_playerctl_metadata("Paused\tmpv\t\tBlues \t Brothers\n"),
            Some(MediaState {
                player: "mpv".into(),
                status: Playback::Paused,
                artist: String::new(),
                title: "Blues \t Brothers".into(),
            })
        );
        // unknown status or a missing field is no player at all
        assert_eq!(parse_playerctl_metadata("Bogus\tfirefox\t\ttitle"), None);
        assert_eq!(parse_playerctl_metadata("Playing\tfirefox\tartist"), None);
        // no players found prints to stderr and exits nonzero, so empty
        // stdout never reaches here, but garbage must not panic either way
        assert_eq!(parse_playerctl_metadata(""), None);
    }

    #[test]
    fn playback_status_maps_playerctl_strings() {
        assert_eq!(parse_playback("Playing"), Some(Playback::Playing));
        assert_eq!(parse_playback("Paused\n"), Some(Playback::Paused));
        assert_eq!(parse_playback("Stopped"), Some(Playback::Stopped));
        assert_eq!(parse_playback("Bogus"), None);
    }

    #[test]
    fn brightness_percent_clamps_and_rejects_zero_max() {
        assert_eq!(brightness_percent(0, 255), Some(0));
        assert_eq!(brightness_percent(128, 255), Some(50));
        assert_eq!(brightness_percent(255, 255), Some(100));
        // round-to-100 edge: 254/255 is 99.6%, clamps to 100
        assert_eq!(brightness_percent(254, 255), Some(100));
        assert_eq!(brightness_percent(100, 0), None);
    }

    #[test]
    fn power_profile_parses_powerprofilesctl_output() {
        assert_eq!(
            PowerProfile::parse("balanced\n"),
            Some(PowerProfile::Balanced)
        );
        assert_eq!(
            PowerProfile::parse("performance\n"),
            Some(PowerProfile::Performance)
        );
        assert_eq!(
            PowerProfile::parse("power-saver\n"),
            Some(PowerProfile::PowerSaver)
        );
        assert_eq!(PowerProfile::parse("nonsense"), None);
        // round-trips through the CLI's own vocabulary
        assert_eq!(
            PowerProfile::parse(PowerProfile::Performance.as_str()),
            Some(PowerProfile::Performance)
        );
    }
}
