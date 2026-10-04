use std::{
    process::Command,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use gpui::{App, AppContext, Context, Entity};

use crate::settings::Settings;

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

/// The default capture device's state: mute and capture gain, the
/// gain only reaching the UI when the mic widget's panel asks for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mic {
    pub muted: bool,
    pub percent: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Brightness {
    pub percent: u8,
}

/// The machine's memory state: usage percent for the widget's text,
/// the absolute pair for its tooltip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ram {
    pub percent: u8,
    /// Mebibytes: meminfo speaks kB, the struct speaks MiB to keep the
    /// arithmetic integral.
    pub used_mib: u64,
    pub total_mib: u64,
}

/// A mount's disk state: usage percent for the widget's text, the
/// absolute pair for its tooltip, and the mount actually measured
/// (a virtual configured mount resolves to the data mount under it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub percent: u8,
    pub used_mib: u64,
    pub total_mib: u64,
    pub mount: String,
}

/// One playback stream (a sink input): the volume panel's per-app
/// rows. `binary` is the client executable in the status output's
/// brackets, the stable handle for desktop-file icon matching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub id: u32,
    pub name: String,
    pub binary: String,
    pub percent: u8,
    pub muted: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BluetoothState {
    pub enabled: bool,
    /// Every device the controller knows: paired ones and, when a
    /// discovery scan has run recently, nearby unpaired ones.
    pub devices: Vec<BluetoothDevice>,
}

/// One device from `bluetoothctl info`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BluetoothDevice {
    pub mac: String,
    pub alias: String,
    pub connected: bool,
    pub paired: bool,
    /// Trusted devices reconnect on their own; a paired-but-untrusted
    /// one waits for permission.
    pub trusted: bool,
    /// The battery percentage, when the device reports one.
    pub battery: Option<u8>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkState {
    pub online: bool,
    pub wifi: bool,
    pub ssid: Option<String>,
    /// The Wi-Fi radio itself (nmcli radio wifi), distinct from being
    /// connected: quick settings toggles the radio.
    pub wifi_enabled: bool,
    /// The visible access points, scanned only while the Wi-Fi panel
    /// is open (the list call is too heavy for the idle shell).
    pub access_points: Vec<AccessPoint>,
    /// The remembered wireless connections, same scan gate.
    pub saved: Vec<String>,
}

/// One visible access point from `nmcli dev wifi list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessPoint {
    pub ssid: String,
    /// Signal strength, 0 to 100.
    pub strength: u8,
    /// WPA/WPA2/802.1X or open: an open network joins without a
    /// password, a secured unknown one asks.
    pub secured: bool,
    /// The network the machine is on right now.
    pub active: bool,
    /// In the saved connections: joins without asking.
    pub known: bool,
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

    /// The display name, title case: the OSD card's value.
    pub fn title(self) -> &'static str {
        match self {
            PowerProfile::Performance => "Performance",
            PowerProfile::Balanced => "Balanced",
            PowerProfile::PowerSaver => "Power Saver",
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

#[derive(Clone, Default)]
struct CpuSample {
    idle: u64,
    total: u64,
    /// One (idle, total) pair per core, from the cpuN lines.
    cores: Vec<(u64, u64)>,
}

/// One poll's CPU reading: the total usage fraction and per-core
/// fractions (same length every poll, the poll loop guarantees that).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CpuUsage {
    pub total: f32,
    pub cores: Vec<f32>,
}

#[derive(Default)]
pub struct SysMon {
    pub battery: Option<Battery>,
    pub volume: Option<Volume>,
    pub mic: Option<Mic>,
    pub brightness: Option<Brightness>,
    pub cpu: Option<f32>,
    /// Per-core usage fractions, same order as the kernel's cpuN lines.
    pub cpu_cores: Option<Vec<f32>>,
    /// The total usage's recent history, newest last, capped at
    /// HISTORY_LEN samples (one per full poll).
    pub cpu_history: Vec<f32>,
    /// The other sysinfo widgets' histories, same cadence, as percents.
    pub ram_history: Vec<u32>,
    pub temp_history: Vec<u32>,
    pub disk_history: Vec<u32>,
    pub battery_history: Vec<u32>,
    /// (1, 5, 15 minute) load averages and the process count.
    pub loadavg: Option<([f32; 3], u32)>,
    pub uptime: Option<u64>,
    /// Playback streams, populated only while the volume panel is
    /// open (see the scan gate in the poll loop).
    pub streams: Vec<Stream>,
    pub ram: Option<Ram>,
    pub temp: Option<u32>,
    pub disk: Option<Disk>,
    /// The mount the disk widget watches: user state, mirrored from
    /// the settings' sysinfo section.
    pub disk_mount: std::path::PathBuf,
    pub bluetooth: Option<BluetoothState>,
    pub network: Option<NetworkState>,
    pub power_profile: Option<PowerProfile>,
    pub media: Option<MediaState>,
    pub recording: Option<RecordingState>,
    /// When the last audio/brightness request was stamped: the fast
    /// poll skips those fields while one is still landing, so a
    /// mid-flight read can't revert the optimistic value.
    av_request_at: Option<Instant>,
    /// When the sink mute last toggled: the mute key double-fires on
    /// some laptops (the field one emitted pairs 14-18ms apart), and
    /// the second event is the bounce, not a second press.
    sink_mute_at: Option<Instant>,
    /// When the mic mute last toggled: same bounce guard, own clock so
    /// a sink press never eats a deliberate mic press.
    mic_mute_at: Option<Instant>,
    /// A poll read of the trio that differed from the state and waits
    /// for the next read to confirm it (see `absorb_av`).
    av_confirming: Option<AvRead>,
    /// The serialized av worker's queue, started by the first request.
    av_sender: Option<smol::channel::Sender<AvRequest>>,
    /// The Wi-Fi panel's last connect/forget failure: the panel shows
    /// it inline; the next action or scan clears it.
    pub wifi_error: Option<String>,
    /// The SSID a connect request is in flight for: the panel's
    /// "joining..." tag. Cleared by the scan that confirms it.
    pub connecting_ssid: Option<String>,
    /// The Bluetooth panel's failure line, same life as `wifi_error`.
    pub bt_error: Option<String>,
    /// The device MAC an act is in flight for: the panel's
    /// "working..." tag.
    pub bt_busy: Option<String>,
}

/// One poll read of the audio and brightness trio: the candidate state.
#[derive(Clone, Copy, Default, PartialEq)]
struct AvRead {
    volume: Option<Volume>,
    mic: Option<Mic>,
    brightness: Option<Brightness>,
}

/// A volume change request: `Set` carries an already-clamped target (the GUI's
/// optimistic path); `Change` lets wpctl read live volume first (the MSG CLI
/// path, and the GUI's fallback before the first snapshot); `Mute` carries
/// the mute target when the shell has a belief (it names what the press
/// meant instead of re-asking wpctl what toggle lands on), and toggles
/// blind when there is none.
#[derive(Clone, Copy)]
enum VolumeRequest {
    Set(u8),
    Change(i32),
    Mute(Option<bool>),
}

impl VolumeRequest {
    fn execute(self) -> anyhow::Result<()> {
        match self {
            VolumeRequest::Set(percent) => set_volume(percent),
            VolumeRequest::Change(delta) => change_volume(delta),
            VolumeRequest::Mute(Some(true)) => set_mute("1", false),
            VolumeRequest::Mute(Some(false)) => set_mute("0", false),
            VolumeRequest::Mute(None) => toggle_mute(),
        }
    }
}

/// One queued audio/brightness change: the request plus the optimistic
/// state it replaced, for the rollback when its call fails. All of
/// them ride one worker in submission order: each request is an
/// absolute write, so a raced completion order would land the wrong
/// final value under a rapid scroll or a held keybind.
enum AvRequest {
    Volume(VolumeRequest, Option<Volume>),
    BrightnessDelta(i32, Option<Brightness>),
    BrightnessAbsolute(u8, Option<Brightness>),
    Mic(Option<Mic>),
    MicVolume(u8, Option<Mic>),
    /// A stream's absolute volume: the panel's slider path.
    StreamVolume(u32, u8),
    /// A stream's mute toggle: the panel's mute button.
    StreamMute(u32),
}

impl AvRequest {
    async fn execute(self, this: &gpui::WeakEntity<SysMon>, cx: &mut gpui::AsyncApp) {
        match self {
            AvRequest::Volume(request, previous) => {
                if let Err(err) = cx.background_spawn(async move { request.execute() }).await {
                    log::error!("volume request failed: {err:#}");
                    let _ = this.update(cx, |sysmon, cx| {
                        sysmon.volume = previous; // rollback; the next poll reconciles
                        cx.notify();
                    });
                }
            }
            AvRequest::BrightnessDelta(delta, previous) => {
                if let Err(err) = cx
                    .background_spawn(async move { change_brightness(delta) })
                    .await
                {
                    log::error!("brightness request failed: {err:#}");
                    let _ = this.update(cx, |sysmon, cx| {
                        sysmon.brightness = previous; // rollback; the next poll reconciles
                        cx.notify();
                    });
                }
            }
            AvRequest::BrightnessAbsolute(percent, previous) => {
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
            }
            AvRequest::Mic(previous) => {
                // the target is the optimistic flip's answer: unmute a
                // mic the belief had muted, mute one it hadn't; a blind
                // toggle only when there is no belief to name
                let result = async move {
                    match previous {
                        Some(mic) => set_mute(if mic.muted { "0" } else { "1" }, true),
                        None => toggle_mic(),
                    }
                };
                if let Err(err) = cx.background_spawn(result).await {
                    log::error!("mic mute toggle failed: {err:#}");
                    let _ = this.update(cx, |sysmon, cx| {
                        sysmon.mic = previous; // rollback; the next poll reconciles
                        cx.notify();
                    });
                }
            }
            AvRequest::MicVolume(percent, previous) => {
                if let Err(err) = cx
                    .background_spawn(async move { set_mic_volume(percent) })
                    .await
                {
                    log::error!("mic volume request failed: {err:#}");
                    let _ = this.update(cx, |sysmon, cx| {
                        sysmon.mic = previous; // rollback; the next poll reconciles
                        cx.notify();
                    });
                }
            }
            AvRequest::StreamVolume(id, percent) => {
                if let Err(err) = cx
                    .background_spawn(async move { set_stream_volume(id, percent) })
                    .await
                {
                    // no optimistic state to roll back: the stream's
                    // row reconciles on the next panel-open scan
                    log::error!("stream volume request failed: {err:#}");
                }
            }
            AvRequest::StreamMute(id) => {
                if let Err(err) = cx
                    .background_spawn(async move { toggle_stream_mute(id) })
                    .await
                {
                    log::error!("stream mute toggle failed: {err:#}");
                }
            }
        }
    }
}

impl SysMon {
    /// The serialized av executor's queue side: the first request
    /// starts the one worker; every request lines up behind it.
    fn queue(&mut self, request: AvRequest, cx: &mut Context<Self>) {
        let sender = match &self.av_sender {
            Some(sender) => sender.clone(),
            None => {
                let (sender, receiver) = smol::channel::unbounded::<AvRequest>();
                self.av_sender = Some(sender.clone());
                cx.spawn(async move |this, cx| {
                    while let Ok(request) = receiver.recv().await {
                        request.execute(&this, cx).await;
                    }
                })
                .detach();
                sender
            }
        };
        if let Err(err) = sender.send_blocking(request) {
            log::error!("queueing av request failed: {err:#}");
        }
    }

    /// One request seam for volume changes (ADR-0006): optimistic snapshot
    /// write, queue, rollback on error. The MSG CLI uses the cx-free
    /// `change_volume` instead; both share `clamp_percent`.
    pub fn request_volume(&mut self, delta: i32, cx: &mut Context<Self>) {
        let previous = self.volume;
        let request = match previous {
            Some(current) => VolumeRequest::Set(clamp_percent(current.percent as i32, delta)),
            None => VolumeRequest::Change(delta),
        };
        if let (VolumeRequest::Set(percent), Some(current)) = (request, previous) {
            self.volume = Some(Volume { percent, ..current });
            self.av_request_at = Some(Instant::now());
            cx.notify();
        }
        self.queue(AvRequest::Volume(request, previous), cx);
    }

    /// One request seam for mute changes (ADR-0006): optimistic flip,
    /// queue the named target, rollback on error. Bounce-guarded: the
    /// mute key double-fires on some laptops.
    pub fn request_mute_toggle(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if bounced(self.sink_mute_at, now) {
            return;
        }
        self.sink_mute_at = Some(now);
        let previous = self.volume;
        let target = previous.map(|volume| !volume.muted);
        if let (Some(muted), Some(current)) = (target, previous) {
            self.volume = Some(Volume { muted, ..current });
            self.av_request_at = Some(Instant::now());
            cx.notify();
        }
        self.queue(AvRequest::Volume(VolumeRequest::Mute(target), previous), cx);
    }

    /// One request seam for the microphone mute (ADR-0006): optimistic
    /// flip, queue the named target (the worker sets what the press
    /// meant rather than re-asking wpctl what toggle lands on),
    /// rollback on error; the standalone CLI keeps using cx-free
    /// `toggle_mic`. No mic snapshot to flip means nothing to preview:
    /// the fast poll brings the change within the half second.
    /// Bounce-guarded like the sink mute.
    pub fn request_mic_toggle(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if bounced(self.mic_mute_at, now) {
            return;
        }
        self.mic_mute_at = Some(now);
        let previous = self.mic;
        if let Some(current) = previous {
            self.mic = Some(Mic {
                muted: !current.muted,
                percent: current.percent,
            });
            self.av_request_at = Some(Instant::now());
            cx.notify();
        }
        self.queue(AvRequest::Mic(previous), cx);
    }

    /// One request seam for the capture gain (ADR-0006): optimistic
    /// write, queue, rollback on error. The gain rides the same
    /// absolute-write serialization as the sink's volume.
    pub fn request_set_mic_volume(&mut self, percent: u8, cx: &mut Context<Self>) {
        let previous = self.mic;
        if let Some(current) = previous {
            self.mic = Some(Mic {
                percent,
                ..current
            });
            self.av_request_at = Some(Instant::now());
            cx.notify();
        }
        self.queue(AvRequest::MicVolume(percent, previous), cx);
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
    /// snapshot write, queue, rollback on error, same ritual as
    /// `request_volume`, minus the delta math.
    pub fn request_set_volume(&mut self, percent: u8, cx: &mut Context<Self>) {
        let previous = self.volume;
        if let Some(current) = previous {
            self.volume = Some(Volume { percent, ..current });
            self.av_request_at = Some(Instant::now());
            cx.notify();
        }
        self.queue(AvRequest::Volume(VolumeRequest::Set(percent), previous), cx);
    }

    /// A stream's absolute volume: the volume panel's per-app slider.
    /// Optimistic write on the matching row (when the snapshot still
    /// has it), queue; the next panel-open scan reconciles.
    pub fn request_set_stream_volume(&mut self, id: u32, percent: u8, cx: &mut Context<Self>) {
        if let Some(stream) = self.streams.iter_mut().find(|stream| stream.id == id) {
            stream.percent = percent;
            cx.notify();
        }
        self.queue(AvRequest::StreamVolume(id, percent), cx);
    }

    /// A stream's mute toggle: the volume panel's per-app button.
    /// Optimistic flip on the matching row, queue, scan reconciles.
    pub fn request_stream_mute_toggle(&mut self, id: u32, cx: &mut Context<Self>) {
        if let Some(stream) = self.streams.iter_mut().find(|stream| stream.id == id) {
            stream.muted = !stream.muted;
            cx.notify();
        }
        self.queue(AvRequest::StreamMute(id), cx);
    }

    /// A fresh stream scan right now, off the poll's clock: the panel
    /// calls this when it opens so the rows are there on the first
    /// paint instead of a full pass later. The poll keeps its own
    /// panel-open cadence after that.
    pub fn scan_streams_now(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let streams = cx.background_spawn(async move { read_streams() }).await;
            let _ = this.update(cx, |sysmon, cx| {
                sysmon.streams = streams;
                cx.notify();
            });
        })
        .detach();
    }

    /// A fresh AP scan right now, same pattern: the Wi-Fi panel calls
    /// this when it opens and on its rescan press.
    pub fn scan_wifi_now(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| self_scan(this, cx).await)
            .detach();
    }

    /// A trigger of the NIC's own scan, then a fresh list: the
    /// panel's rescan press. The rescan takes seconds; the list
    /// refreshes when it lands.
    pub fn request_wifi_rescan(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let _ = cx
                .background_spawn(async move {
                    Command::new("nmcli")
                        .args(["dev", "wifi", "rescan"])
                        .output()
                })
                .await;
            self_scan(this, cx).await;
        })
        .detach();
    }

    /// Join a network: nmcli uses saved credentials on its own for a
    /// known SSID, so a password only rides along for a secured
    /// unknown one. The next scan reconciles; a failure surfaces in
    /// the panel as `wifi_error`.
    pub fn request_wifi_connect(
        &mut self,
        ssid: String,
        password: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.connecting_ssid = Some(ssid.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let mut command = Command::new("nmcli");
                    command.args(["dev", "wifi", "connect", &ssid]);
                    if let Some(password) = &password {
                        command.args(["password", password]);
                    }
                    let output = command.output()?;
                    anyhow::ensure!(output.status.success(), "nmcli connect failed");
                    Ok(())
                })
                .await;
            let _ = this.update(cx, |sysmon, cx| {
                sysmon.wifi_error = result.err().map(|err| format!("{err:#}"));
                cx.notify();
            });
            // the scan that confirms the join (or the error's state)
            self_scan(this, cx).await;
        })
        .detach();
    }

    /// Delete a remembered connection. The next scan reconciles; a
    /// failure surfaces in the panel as `wifi_error`.
    pub fn request_wifi_forget(&mut self, ssid: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let output = Command::new("nmcli")
                        .args(["connection", "delete", &ssid])
                        .output()?;
                    anyhow::ensure!(output.status.success(), "nmcli delete failed");
                    Ok(())
                })
                .await;
            let _ = this.update(cx, |sysmon, cx| {
                sysmon.wifi_error = result.err().map(|err| format!("{err:#}"));
                cx.notify();
            });
            self_scan(this, cx).await;
        })
        .detach();
    }

    /// A fresh bluetooth device scan right now: the panel calls this
    /// when it opens, same as the Wi-Fi panel's.
    pub fn scan_bluetooth_now(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let devices = cx
                .background_spawn(async move {
                    read_bluetooth(true)
                        .map(|state| state.devices)
                        .unwrap_or_default()
                })
                .await;
            let _ = this.update(cx, |sysmon, cx| {
                if let Some(state) = &mut sysmon.bluetooth {
                    state.devices = devices;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Run discovery for a bounded window, then refresh: the panel's
    /// scan-for-nearby press. `--timeout` is what makes this work:
    /// bare `bluetoothctl scan on` in non-interactive mode exits at
    /// EOF on stdin, and its exit tears discovery down before it
    /// hears a single advertisement. `--timeout` keeps the client
    /// alive for the window and stops cleanly after.
    pub fn request_bluetooth_discovery(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let _ = cx
                .background_spawn(async move {
                    Command::new("bluetoothctl")
                        .args(["--timeout", "12", "scan", "on"])
                        .output()
                })
                .await;
            let devices = cx
                .background_spawn(async move {
                    read_bluetooth(true)
                        .map(|state| state.devices)
                        .unwrap_or_default()
                })
                .await;
            let _ = this.update(cx, |sysmon, cx| {
                if let Some(state) = &mut sysmon.bluetooth {
                    state.devices = devices;
                }
                sysmon.bt_busy = None;
                sysmon.bt_error = None;
                cx.notify();
            });
        })
        .detach();
    }

    /// One bluetoothctl act on a device: connect, disconnect, pair
    /// (pair, then trust, then connect: a paired-but-untrusted device
    /// waits for permission, so the panel never leaves one behind),
    /// or remove. `busy` tags the row while it runs; a failure lands
    /// in `bt_error` and the next scan reconciles.
    pub fn request_bluetooth_act(
        &mut self,
        act: BluetoothAct,
        mac: String,
        cx: &mut Context<Self>,
    ) {
        self.bt_busy = Some(mac.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { act.execute(&mac) })
                .await;
            let _ = this.update(cx, |sysmon, cx| {
                sysmon.bt_busy = None;
                sysmon.bt_error = result.err().map(|err| format!("{err:#}"));
                cx.notify();
            });
            let devices = cx
                .background_spawn(async move {
                    read_bluetooth(true)
                        .map(|state| state.devices)
                        .unwrap_or_default()
                })
                .await;
            let _ = this.update(cx, |sysmon, cx| {
                if let Some(state) = &mut sysmon.bluetooth {
                    state.devices = devices;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Delete a remembered connection. The next scan reconciles; a

    /// One request seam for brightness (the quick-settings slider): optimistic
    /// snapshot write, queue, rollback on error.
    pub fn request_set_brightness(&mut self, percent: u8, cx: &mut Context<Self>) {
        let previous = self.brightness;
        if previous.is_some() {
            self.brightness = Some(Brightness { percent });
            self.av_request_at = Some(Instant::now());
            cx.notify();
        }
        self.queue(AvRequest::BrightnessAbsolute(percent, previous), cx);
    }

    /// Brightness by delta (the bar widget's scroll wheel): optimistic
    /// snapshot write, queue, rollback on error; mirrors `request_volume`.
    pub fn request_brightness(&mut self, delta: i32, cx: &mut Context<Self>) {
        let previous = self.brightness;
        if let Some(current) = previous
            && let percent = (current.percent as i32 + delta).clamp(0, 100) as u8
        {
            self.brightness = Some(Brightness { percent });
            self.av_request_at = Some(Instant::now());
            cx.notify();
        }
        self.queue(AvRequest::BrightnessDelta(delta, previous), cx);
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
                        read_bluetooth(false)
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

/// The poll's base cadence: fast enough that a change made outside the
/// shell (a keybind running wpctl, a hardware key) reads as real time,
/// cheap enough to keep forever: two wpctl calls and two sysfs reads.
const FAST_INTERVAL: Duration = Duration::from_millis(500);

/// The sysinfo widgets' sparkline depth: one sample per full poll, so
/// this is the recent window the little graphs show.
const HISTORY_LEN: usize = 60;
/// When a poll read differs from the state and waits for the next
/// read to confirm it (the confirm gate), the re-check rides a short
/// timer instead of the next full tick: an outside change should not
/// wait half a second twice. One-tick garbage reads still fail it:
/// they last well under the gap.
const CONFIRM_RECHECK: Duration = Duration::from_millis(120);
/// The full pass rides every fourth tick: everything else refreshes at
/// the old two-second pace.
const FULL_PASS_EVERY: u32 = 4;
/// How long after a request the fast poll leaves the audio and
/// brightness fields alone: a request's own optimistic write is the
/// truth until its wpctl/brightnessctl call has landed.
const REQUEST_LANDING: Duration = Duration::from_millis(250);

/// The mute keys double-fire on some laptops: the mic key emitted
/// pairs 14-18ms apart in the field. A mute toggle this soon after
/// the last one is the bounce, not a second press.
const MUTE_BOUNCE: Duration = Duration::from_millis(150);

fn bounced(last: Option<Instant>, now: Instant) -> bool {
    last.is_some_and(|at| now.duration_since(at) < MUTE_BOUNCE)
}

/// The AP-and-saved scan the Wi-Fi actions finish with: one read,
/// one state write, one notify.
async fn self_scan(this: gpui::WeakEntity<SysMon>, cx: &mut gpui::AsyncApp) {
    let scan = cx
        .background_spawn(async move {
            let mut network = NetworkState::default();
            network.access_points = read_access_points();
            network.saved = read_saved_wifi();
            for point in &mut network.access_points {
                point.known = network.saved.contains(&point.ssid);
            }
            network
        })
        .await;
    let _ = this.update(cx, |sysmon, cx| {
        if let Some(network) = &mut sysmon.network {
            network.access_points = scan.access_points;
            network.saved = scan.saved;
        }
        sysmon.connecting_ssid = None;
        cx.notify();
    });
}

pub fn run(state: &Entity<SysMon>, settings: &Entity<Settings>, cx: &mut App) {
    // the disk mount is user state: seeded once, then mirrored on
    // settings changes (no re-mint needed, the poll reads it per tick)
    let mount = settings.read(cx).sysinfo.disk_mount.clone();
    state.update(cx, |sysmon, _| sysmon.disk_mount = mount);
    {
        let state = state.downgrade();
        cx.observe(settings, move |settings, cx| {
            let mount = settings.read(cx).sysinfo.disk_mount.clone();
            let _ = state.update(cx, |sysmon, _| {
                if sysmon.disk_mount != mount {
                    sysmon.disk_mount = mount;
                }
            });
        })
        .detach();
    }

    let state = state.downgrade();
    cx.spawn(async move |cx| {
        let mut previous: Option<CpuSample> = None;
        let mut tick = 0u32;
        loop {
            let full = tick % FULL_PASS_EVERY == 0;
            tick = tick.wrapping_add(1);
            let Ok(disk_mount) = state.update(cx, |sysmon, _| sysmon.disk_mount.clone()) else {
                break;
            };
            // the stream scan rides only while the volume panel is
            // open: the status dump is the heaviest call in the pass.
            // The same gate drives the Wi-Fi and Bluetooth panels'
            // device scans.
            let scan_streams = full
                && cx
                    .update(|cx| crate::panel::is_open(&crate::panel::PanelKind::Volume, cx));
            let scan_wifi = full
                && cx.update(|cx| {
                    crate::panel::is_open(&crate::panel::PanelKind::Wifi, cx)
                        || crate::panel::is_open(&crate::panel::PanelKind::Bluetooth, cx)
                });
            let (snapshot, previous_next) = cx
                .background_spawn(async move {
                    let mut previous = previous;
                    let snapshot = if full {
                        refresh(&mut previous, &disk_mount, scan_streams, scan_wifi)
                    } else {
                        refresh_av()
                    };
                    (snapshot, previous)
                })
                .await;
            previous = previous_next;
            let Ok(quick_recheck) = state.update(cx, |sysmon, cx| {
                let moved = if full {
                    sysmon.battery = snapshot.battery;
                    sysmon.cpu = snapshot.cpu;
                    sysmon.cpu_cores = snapshot.cpu_cores.clone();
                    sysmon.loadavg = snapshot.loadavg;
                    sysmon.uptime = snapshot.uptime;
                    if let Some(usage) = snapshot.cpu {
                        sysmon.cpu_history.push(usage);
                        if sysmon.cpu_history.len() > HISTORY_LEN {
                            sysmon.cpu_history.remove(0);
                        }
                    }
                    if let Some(ram) = &snapshot.ram {
                        sysmon.ram_history.push(u32::from(ram.percent));
                        if sysmon.ram_history.len() > HISTORY_LEN {
                            sysmon.ram_history.remove(0);
                        }
                    }
                    if let Some(temp) = snapshot.temp {
                        sysmon.temp_history.push(temp);
                        if sysmon.temp_history.len() > HISTORY_LEN {
                            sysmon.temp_history.remove(0);
                        }
                    }
                    if let Some(disk) = &snapshot.disk {
                        sysmon.disk_history.push(u32::from(disk.percent));
                        if sysmon.disk_history.len() > HISTORY_LEN {
                            sysmon.disk_history.remove(0);
                        }
                    }
                    if let Some(battery) = &snapshot.battery {
                        sysmon.battery_history.push(u32::from(battery.percent));
                        if sysmon.battery_history.len() > HISTORY_LEN {
                            sysmon.battery_history.remove(0);
                        }
                    }
                    sysmon.ram = snapshot.ram;
                    sysmon.temp = snapshot.temp;
                    sysmon.disk = snapshot.disk.clone();
                    sysmon.streams = snapshot.streams.clone();
                    sysmon.bluetooth = snapshot.bluetooth.clone();
                    sysmon.network = snapshot.network.clone();
                    sysmon.power_profile = snapshot.power_profile;
                    sysmon.media = snapshot.media.clone();
                    sysmon.recording = snapshot.recording.clone();
                    log::info!("sysmon snapshot: {snapshot:?}");
                    // the trio rides the same confirm gate as the
                    // fast pass: the full pass doesn't get to
                    // bypass it
                    sysmon.absorb_av(&snapshot);
                    true
                } else {
                    sysmon.absorb_av(&snapshot)
                };
                if moved {
                    cx.notify();
                }
                // a stored confirm candidate re-checks soon
                sysmon.av_confirming.is_some()
            }) else {
                break;
            };
            if quick_recheck {
                cx.background_executor().timer(CONFIRM_RECHECK).await;
            } else {
                cx.background_executor().timer(FAST_INTERVAL).await;
            }
        }
    })
    .detach();
}

#[derive(Clone, Debug, Default)]
struct Snapshot {
    battery: Option<Battery>,
    volume: Option<Volume>,
    mic: Option<Mic>,
    brightness: Option<Brightness>,
    cpu: Option<f32>,
    cpu_cores: Option<Vec<f32>>,
    loadavg: Option<([f32; 3], u32)>,
    uptime: Option<u64>,
    /// Playback streams, scanned only while the volume panel is open
    /// (the status dump is too heavy for the idle shell).
    streams: Vec<Stream>,
    ram: Option<Ram>,
    temp: Option<u32>,
    disk: Option<Disk>,
    bluetooth: Option<BluetoothState>,
    network: Option<NetworkState>,
    power_profile: Option<PowerProfile>,
    media: Option<MediaState>,
    recording: Option<RecordingState>,
}

fn refresh(
    previous: &mut Option<CpuSample>,
    disk_mount: &std::path::Path,
    scan_streams: bool,
    scan_wifi: bool,
) -> Snapshot {    let usage = sample_cpu_usage(previous);
    Snapshot {
        battery: read_battery(),
        volume: read_volume(),
        mic: read_mic(),
        brightness: read_brightness(),
        cpu: usage.as_ref().map(|usage| usage.total),
        cpu_cores: usage.as_ref().map(|usage| usage.cores.clone()),
        loadavg: read_loadavg(),
        uptime: read_uptime(),
        streams: if scan_streams {
            read_streams()
        } else {
            Vec::new()
        },
        ram: read_ram(),
        temp: read_cpu_temp(),
        disk: read_disk(disk_mount),
        bluetooth: read_bluetooth(scan_wifi),
        network: read_network(scan_wifi),
        power_profile: read_power_profile(),
        media: read_media(),
        recording: read_recording(),
    }
}

/// The fast pass: only what a keybind or a hardware key changes out
/// from under the shell. Everything else keeps the full pass's pace.
fn refresh_av() -> Snapshot {
    Snapshot {
        volume: read_volume(),
        mic: read_mic(),
        brightness: read_brightness(),
        ..Default::default()
    }
}

impl SysMon {
    /// The poll's apply for the audio and brightness trio. A read that
    /// differs from the state must be confirmed by the next read before
    /// it counts: around a routing change WirePlumber's defaults flap,
    /// and one transient read showed the sink muted when only the mic
    /// had moved, toasting the wrong card twice. Our own requests are
    /// above this: while one is landing the trio is left alone.
    /// Reports whether the state moved.
    fn absorb_av(&mut self, snapshot: &Snapshot) -> bool {
        let busy = self
            .av_request_at
            .map(|at| at.elapsed() < REQUEST_LANDING)
            .unwrap_or(false);
        if busy {
            return false;
        }
        let read = AvRead {
            volume: snapshot.volume,
            mic: snapshot.mic,
            brightness: snapshot.brightness,
        };
        let state = AvRead {
            volume: self.volume,
            mic: self.mic,
            brightness: self.brightness,
        };
        if read == state {
            self.av_confirming = None;
            return false;
        }
        // a differing read must repeat before it applies
        if self.av_confirming.replace(read) == Some(read) {
            self.av_confirming = None;
            self.volume = read.volume;
            self.mic = read.mic;
            self.brightness = read.brightness;
            return true;
        }
        false
    }
}

pub fn set_volume(percent: u8) -> anyhow::Result<()> {
    let output = Command::new("wpctl")
        .args(["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{percent}%")])
        .output()?;
    anyhow::ensure!(output.status.success(), "wpctl set-volume failed");
    Ok(())
}

/// A stream's absolute volume, same shape as the sink's: the id names
/// the sink input, clamped to 100 so wpctl can't amplify.
fn set_stream_volume(id: u32, percent: u8) -> anyhow::Result<()> {
    let output = Command::new("wpctl")
        .args(["set-volume", &id.to_string(), &format!("{percent}%")])
        .output()?;
    anyhow::ensure!(output.status.success(), "wpctl set-volume failed");
    Ok(())
}

fn toggle_stream_mute(id: u32) -> anyhow::Result<()> {
    let output = Command::new("wpctl")
        .args(["set-mute", &id.to_string(), "toggle"])
        .output()?;
    anyhow::ensure!(output.status.success(), "wpctl set-mute failed");
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
    set_mute("toggle", false)
}

/// Absolute mute set: the toggle verbs read live state and flip it,
/// which races the shell's own belief under the poll's confirm gate;
/// naming the target says what the press meant.
fn set_mute(target: &str, source: bool) -> anyhow::Result<()> {
    let object = if source {
        "@DEFAULT_AUDIO_SOURCE@"
    } else {
        "@DEFAULT_AUDIO_SINK@"
    };
    let output = Command::new("wpctl")
        .args(["set-mute", object, target])
        .output()?;
    anyhow::ensure!(output.status.success(), "wpctl set-mute failed");
    Ok(())
}

/// The MSG CLI's mic verb: the capture device, not the sink.
pub fn toggle_mic() -> anyhow::Result<()> {
    set_mute("toggle", true)
}

/// Absolute capture-gain set, the sink's `set_volume` for the source.
pub fn set_mic_volume(percent: u8) -> anyhow::Result<()> {
    let output = Command::new("wpctl")
        .args(["set-volume", "@DEFAULT_AUDIO_SOURCE@", &format!("{percent}%")])
        .output()?;
    anyhow::ensure!(output.status.success(), "wpctl set-volume failed");
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

fn read_bluetooth(scan_devices: bool) -> Option<BluetoothState> {
    let show = Command::new("bluetoothctl").arg("show").output().ok()?;
    if !show.status.success() {
        return None;
    }
    let enabled = String::from_utf8_lossy(&show.stdout)
        .lines()
        .any(|line| line.trim().starts_with("Powered:") && line.contains("yes"));

    let mut devices = Vec::new();
    if enabled {
        // panel open: the full picture, every known device with its
        // info block. Panel closed: the bar widget needs only who is
        // connected, and that is one cheap call instead of N+1.
        if scan_devices {
            if let Ok(list) = Command::new("bluetoothctl").arg("devices").output()
                && list.status.success()
            {
                // "Device XX:XX:XX:XX:XX:XX Alias" for every known device,
                // paired and (after a discovery scan) nearby-unpaired
                for line in String::from_utf8_lossy(&list.stdout).lines() {
                    let Some((_, rest)) = line.trim().split_once("Device ") else {
                        continue;
                    };
                    let Some((mac, alias)) = rest.split_once(' ') else {
                        continue;
                    };
                    if let Some(device) = read_device_info(mac.trim(), alias.trim()) {
                        devices.push(device);
                    }
                }
            }
            devices.sort_by(|a, b| {
                b.connected
                    .cmp(&a.connected)
                    .then(b.paired.cmp(&a.paired))
                    .then(a.alias.to_lowercase().cmp(&b.alias.to_lowercase()))
            });
        } else if let Ok(connected) = Command::new("bluetoothctl")
            .args(["devices", "Connected"])
            .output()
            && connected.status.success()
        {
            for line in String::from_utf8_lossy(&connected.stdout).lines() {
                let Some((_, rest)) = line.trim().split_once("Device ") else {
                    continue;
                };
                let Some((mac, alias)) = rest.split_once(' ') else {
                    continue;
                };
                devices.push(BluetoothDevice {
                    mac: mac.trim().to_string(),
                    alias: alias.trim().to_string(),
                    connected: true,
                    paired: true,
                    trusted: true,
                    battery: None,
                });
            }
        }
    }
    Some(BluetoothState { enabled, devices })
}

/// One device's `bluetoothctl info` block: the connected/paired/
/// trusted trio and the battery line when the device reports one.
fn read_device_info(mac: &str, alias: &str) -> Option<BluetoothDevice> {
    let output = Command::new("bluetoothctl").args(["info", mac]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_device_info(
        mac.to_string(),
        alias.to_string(),
        &String::from_utf8_lossy(&output.stdout),
    ))
}

fn parse_device_info(mac: String, alias: String, text: &str) -> BluetoothDevice {
    let mut device = BluetoothDevice {
        mac,
        alias,
        connected: false,
        paired: false,
        trusted: false,
        battery: None,
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Connected:") {
            device.connected = value.trim() == "yes";
        } else if let Some(value) = line.strip_prefix("Paired:") {
            device.paired = value.trim() == "yes";
        } else if let Some(value) = line.strip_prefix("Trusted:") {
            device.trusted = value.trim() == "yes";
        } else if line.starts_with("Battery Percentage:") {
            // "Battery Percentage: 0x50 (80)"
            device.battery = line
                .rsplit('(')
                .next()
                .and_then(|inner| inner.trim_end_matches(')').trim().parse().ok());
        }
    }
    device
}

/// The acts the Bluetooth panel's rows ask for, each one
/// `bluetoothctl` verb bounded by `timeout` so a hanging device can't
/// wedge the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BluetoothAct {
    Connect,
    Disconnect,
    /// Pair, then trust, then connect: a paired-but-untrusted device
    /// waits for permission, so the panel never leaves one behind.
    Pair,
    /// Unpair and forget.
    Remove,
}

impl BluetoothAct {
    fn execute(self, mac: &str) -> anyhow::Result<()> {
        match self {
            BluetoothAct::Connect => {
                let status = Command::new("timeout")
                    .args(["15", "bluetoothctl", "connect", mac])
                    .status()?;
                anyhow::ensure!(status.success(), "connect failed");
            }
            BluetoothAct::Disconnect => {
                let status = Command::new("timeout")
                    .args(["10", "bluetoothctl", "disconnect", mac])
                    .status()?;
                anyhow::ensure!(status.success(), "disconnect failed");
            }
            BluetoothAct::Pair => {
                let status = Command::new("timeout")
                    .args(["20", "bluetoothctl", "pair", mac])
                    .status()?;
                anyhow::ensure!(status.success(), "pairing failed");
                if let Ok(trust) = Command::new("bluetoothctl")
                    .args(["trust", mac])
                    .status()
                    && !trust.success()
                {
                    log::info!("bluetooth: trust after pair failed for {mac}");
                }
                let status = Command::new("timeout")
                    .args(["15", "bluetoothctl", "connect", mac])
                    .status()?;
                anyhow::ensure!(status.success(), "paired, but connect failed");
            }
            BluetoothAct::Remove => {
                let status = Command::new("timeout")
                    .args(["10", "bluetoothctl", "remove", mac])
                    .status()?;
                anyhow::ensure!(status.success(), "remove failed");
            }
        }
        Ok(())
    }
}

fn read_network(scan_wifi: bool) -> Option<NetworkState> {
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
        access_points: Vec::new(),
        saved: Vec::new(),
    };
    if let Ok(status) = Command::new("nmcli")
        .args(["-t", "-f", "TYPE,STATE,CONNECTION", "device", "status"])
        .output()
        && status.status.success()
    {
        apply_device_status(&mut network, &String::from_utf8_lossy(&status.stdout));
    }
    // the AP list and the saved connections ride the panel-open gate:
    // two more nmcli calls, only paid while someone is picking
    if scan_wifi && network.wifi_enabled {
        network.access_points = read_access_points();
        network.saved = read_saved_wifi();
        for point in &mut network.access_points {
            point.known = network.saved.contains(&point.ssid);
        }
    }
    Some(network)
}

/// The visible access points: SSID, signal, security, and whether the
/// machine is on it now. A missing SSID (a hidden network) drops out.
fn read_access_points() -> Vec<AccessPoint> {
    let Ok(output) = Command::new("nmcli")
        .args([
            "-t",
            "-f",
            "SSID,SIGNAL,SECURITY,ACTIVE",
            "dev",
            "wifi",
            "list",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_access_points(&String::from_utf8_lossy(&output.stdout))
}

/// The remembered wireless connections: names whose type is the
/// wifi one in `nmcli -t -f NAME,TYPE connection show`.
fn read_saved_wifi() -> Vec<String> {
    let Ok(output) = Command::new("nmcli")
        .args(["-t", "-f", "NAME,TYPE", "connection", "show"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.rsplit_once(':'))
        .filter(|(_, kind)| *kind == "802-11-wireless" || *kind == "wifi")
        .map(|(name, _)| name.to_string())
        .collect()
}

/// `nmcli -t` escapes a literal colon in a field as `\:`; split on
/// the unescaped colons only.
fn split_nmcli_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            field.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == ':' {
            fields.push(std::mem::take(&mut field));
        } else {
            field.push(character);
        }
    }
    fields.push(field);
    fields
}

/// The AP list: `SSID:SIGNAL:SECURITY:ACTIVE`, trailing colons
/// padding empty fields. An empty SSID is a hidden network: dropped.
fn parse_access_points(output: &str) -> Vec<AccessPoint> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(split_nmcli_line)
        .filter_map(|fields| {
            let ssid = fields.first()?.trim().to_string();
            if ssid.is_empty() || ssid == "--" {
                return None;
            }
            let signal = fields.get(1)?.trim().parse().ok()?;
            let security = fields.get(2).map(|field| field.trim()).unwrap_or("");
            let active = fields.get(3).map(|field| field.trim() == "yes")?;
            Some(AccessPoint {
                ssid,
                strength: signal,
                secured: !security.is_empty() && security != "--",
                active,
                // filled in by the caller against the saved list
                known: false,
            })
        })
        .collect()
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

/// The Audio section's stream list: `(id, display name, client
/// binary)` per playback stream. Only the Audio section counts (Video
/// keeps its own Streams block), and only the top-level stream lines
/// (the channel routing lines under each stream carry " > ").
fn parse_streams(text: &str) -> Vec<(u32, String, String)> {
    let mut in_audio = false;
    let mut in_streams = false;
    let mut streams = Vec::new();
    for line in text.lines() {
        let top_level = !line.starts_with(' ');
        if top_level {
            in_audio = line.trim() == "Audio";
            in_streams = false;
            continue;
        }
        if !in_audio {
            continue;
        }
        if line.trim().ends_with("Streams:") {
            in_streams = true;
            continue;
        }
        if !in_streams || line.contains(" > ") {
            continue;
        }
        // "        67. PipeWire ALSA [gst-launch-1.0]"
        let trimmed = line.trim();
        let Some((id, rest)) = trimmed.split_once(". ") else {
            continue;
        };
        let Ok(id) = id.parse() else {
            continue;
        };
        let (name, binary) = match rest.rsplit_once(" [").and_then(|(name, binary)| {
            binary
                .strip_suffix(']')
                .map(|binary| (name.trim().to_string(), binary.to_string()))
        }) {
            Some(pair) => pair,
            None => (rest.trim().to_string(), rest.trim().to_string()),
        };
        streams.push((id, name, binary));
    }
    streams
}

/// Every playback stream on the default server, with each stream's
/// volume read one call later. Only called while the volume panel is
/// open: `wpctl status` dumps the whole graph, too heavy for the idle
/// shell.
fn read_streams() -> Vec<Stream> {
    let Ok(output) = Command::new("wpctl").arg("status").output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_streams(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .filter_map(|(id, name, binary)| {
            let volume = Command::new("wpctl")
                .args(["get-volume", &id.to_string()])
                .output()
                .ok()?;
            if !volume.status.success() {
                return None;
            }
            let volume = parse_wpctl_volume(&String::from_utf8_lossy(&volume.stdout))?;
            // filter hard: a stream parked at zero is an app holding
            // its sink input open, not something anyone mixes
            if volume.percent == 0 {
                return None;
            }
            Some(Stream {
                id,
                name,
                binary,
                percent: volume.percent,
                muted: volume.muted,
            })
        })
        .collect()
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

/// The microphone rides the same parser: `wpctl` prints the identical
/// shape for the default source, muting is the only field the shell
/// keeps.
fn read_mic() -> Option<Mic> {
    let output = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SOURCE@"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_wpctl_volume(&String::from_utf8_lossy(&output.stdout)).map(|volume| Mic {
        muted: volume.muted,
        percent: volume.percent,
    })
}

/// Memory pressure from /proc/meminfo: in use is total minus
/// MemAvailable (available is what the kernel could hand out before
/// swapping, the honest "free" for a machine with page cache).
fn read_ram() -> Option<Ram> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo(&text)
}

fn parse_meminfo(text: &str) -> Option<Ram> {
    let field = |name: &str| -> Option<u64> {
        text.lines().find_map(|line| {
            let value = line.strip_prefix(name)?;
            let value = value.trim().trim_end_matches(" kB").trim();
            value.parse::<u64>().ok()
        })
    };
    // meminfo reports kB; MiB keeps the numbers in widget range
    let total_mib = field("MemTotal:")? / 1024;
    let available_mib = field("MemAvailable:")? / 1024;
    if total_mib == 0 {
        return None;
    }
    let used_mib = total_mib.saturating_sub(available_mib);
    Some(Ram {
        percent: ((used_mib * 100) / total_mib) as u8,
        used_mib,
        total_mib,
    })
}

/// The CPU's temperature: the kernel's hwmon tree, the chip named for
/// the CPU preferred (coretemp on Intel, k10temp or zenpower on AMD),
/// its hottest zone speaking; thermal zones that name a cpu are the
/// fallback. Degrees, not milli-degrees.
fn read_cpu_temp() -> Option<u32> {
    const CPU_CHIPS: [&str; 3] = ["coretemp", "k10temp", "zenpower"];
    let mut candidates: Vec<(u8, u32)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let dir = entry.path();
            let Ok(name) = std::fs::read_to_string(dir.join("name")) else {
                continue;
            };
            let priority = u8::from(CPU_CHIPS.contains(&name.trim()));
            let Ok(zones) = std::fs::read_dir(&dir) else {
                continue;
            };
            for zone in zones.flatten() {
                let file = zone.file_name().to_string_lossy().to_string();
                if file.starts_with("temp") && file.ends_with("_input") {
                    if let Some(temp) = read_milli_degrees(&zone.path()) {
                        candidates.push((priority, temp));
                    }
                }
            }
        }
    }
    // the thermal_zone fallback: machines whose CPU sensor only shows
    // there (x86_pkg_temp, soc thermal)
    if let Ok(entries) = std::fs::read_dir("/sys/class/thermal") {
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let Some(rest) = file_name.strip_prefix("thermal_zone") else {
                continue;
            };
            if !rest.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Ok(ty) = std::fs::read_to_string(entry.path().join("type")) else {
                continue;
            };
            if !ty.trim().contains("cpu") {
                continue;
            }
            if let Some(temp) = read_milli_degrees(&entry.path()) {
                candidates.push((1, temp));
            }
        }
    }
    // a named CPU chip outranks a thermal zone; the hottest zone wins
    // within a chip
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    candidates.first().map(|(_, temp)| *temp)
}

/// One hwmon/thermal reading: milli-degrees in, degrees out. A
/// missing or unparsable file is no reading at all.
fn read_milli_degrees(path: &std::path::Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse::<u32>().ok().map(|milli| milli / 1000)
}

/// A mount's disk usage from statvfs: the percent is the df number,
/// used over (used plus the user's available), so the reserved-root
/// blocks don't make a full-feeling disk read two-thirds.
///
/// Atomic distros mount the root at a virtual filesystem (composefs,
/// overlay) whose statvfs numbers describe the deployment image, not
/// the disk under it; a virtual mount walks the data mounts that back
/// it (/var, then /sysroot) instead. A traditional distro's root is a
/// real filesystem and reads directly, so one path serves both.
fn read_disk(mount: &std::path::Path) -> Option<Disk> {
    let real = |path: &std::path::Path| -> Option<Disk> {
        match mount_fstype(path) {
            Some(fstype) if !is_virtual_fs(&fstype) => statvfs_disk(path).map(|disk| Disk {
                mount: path.display().to_string(),
                ..disk
            }),
            _ => None,
        }
    };
    real(mount)
        .or_else(|| real(std::path::Path::new("/var")))
        .or_else(|| real(std::path::Path::new("/sysroot")))
}

/// Filesystems whose statvfs doesn't describe a disk the user fills:
/// the atomic root views and the kernel's own trees.
fn is_virtual_fs(fstype: &str) -> bool {
    matches!(
        fstype,
        "composefs" | "overlay" | "squashfs" | "tmpfs" | "ramfs" | "devtmpfs" | "proc" | "sysfs"
            | "cgroup2" | "bpf" | "tracefs" | "debugfs" | "securityfs" | "pstore" | "mqueue"
            | "hugetlbfs" | "efivarfs" | "configfs" | "autofs" | "binfmt_misc"
    )
}

/// The filesystem type of the deepest entry in /proc/mounts whose
/// mount point contains the path. No /proc/mounts, no answer.
fn mount_fstype(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string("/proc/mounts").ok()?;
    mount_fstype_of(&text, &path.to_string_lossy())
}

fn mount_fstype_of(mounts: &str, path: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let _device = fields.next()?;
        let point = fields.next()?;
        let fstype = fields.next()?;
        let point = unescape_mount_path(point);
        let covers = point == "/"
            || (path.starts_with(&point)
                && (path.len() == point.len() || path[point.len()..].starts_with('/')));
        // equal depth: the later entry shadowed the earlier one
        if covers && best.as_ref().map_or(true, |(len, _)| point.len() >= *len) {
            best = Some((point.len(), fstype.to_string()));
        }
    }
    best.map(|(_, fstype)| fstype)
}

/// /proc/mounts escapes spaces (and friends) in mount points as
/// backslash-octal; decode the common case.
fn unescape_mount_path(point: &str) -> String {
    let bytes = point.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(&point[i + 1..i + 4], 8) {
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn statvfs_disk(mount: &std::path::Path) -> Option<Disk> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(mount.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    parse_statvfs(
        u64::try_from(stat.f_frsize).ok()?,
        stat.f_blocks,
        stat.f_bfree,
        stat.f_bavail,
    )
}

fn parse_statvfs(frsize: u64, blocks: u64, bfree: u64, bavail: u64) -> Option<Disk> {
    let total_mib = blocks * frsize / (1024 * 1024);
    let used_mib = (blocks - bfree) * frsize / (1024 * 1024);
    let user_avail_mib = bavail * frsize / (1024 * 1024);
    let denominator = used_mib + user_avail_mib;
    if denominator == 0 {
        return None;
    }
    Some(Disk {
        percent: ((used_mib * 100) / denominator) as u8,
        used_mib,
        total_mib,
        // the caller names the mount it actually read
        mount: String::new(),
    })
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

fn sample_cpu_usage(previous: &mut Option<CpuSample>) -> Option<CpuUsage> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let sample = parse_cpu_stat(&text)?;
    let usage = previous
        .take()
        .and_then(|previous| cpu_usage(previous, sample.clone()));
    *previous = Some(sample);
    usage
}

fn parse_cpu_stat(text: &str) -> Option<CpuSample> {
    let mut lines = text.lines();
    let aggregate = lines.next()?;
    let fields: Vec<u64> = aggregate
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    if fields.len() < 5 {
        return None;
    }
    // the cpuN lines after the aggregate: one (idle, total) per core
    let cores = lines
        .filter(|line| line.starts_with("cpu") && line.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
        .filter_map(|line| {
            let fields: Vec<u64> = line
                .split_whitespace()
                .skip(1)
                .filter_map(|field| field.parse().ok())
                .collect();
            if fields.len() < 5 {
                return None;
            }
            Some((fields[3] + fields[4], fields.iter().sum()))
        })
        .collect();
    Some(CpuSample {
        idle: fields[3] + fields[4],
        total: fields.iter().sum(),
        cores,
    })
}

fn cpu_usage(previous: CpuSample, sample: CpuSample) -> Option<CpuUsage> {
    let total = cpu_delta(previous.clone(), sample.clone())?;
    let cores = previous
        .cores
        .iter()
        .zip(sample.cores.iter())
        .filter_map(|(previous, sample)| {
            let (p_idle, p_total) = *previous;
            let (s_idle, s_total) = *sample;
            let delta = s_total.saturating_sub(p_total);
            if delta == 0 {
                None
            } else {
                Some((1.0 - s_idle.saturating_sub(p_idle) as f32 / delta as f32).max(0.0))
            }
        })
        .collect();
    Some(CpuUsage { total, cores })
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

/// /proc/loadavg: the three averages and the running/total process
/// counts bundled in the fourth field.
fn read_loadavg() -> Option<([f32; 3], u32)> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    parse_loadavg(&text)
}

fn parse_loadavg(text: &str) -> Option<([f32; 3], u32)> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    let loads = [
        fields.first()?.parse().ok()?,
        fields.get(1)?.parse().ok()?,
        fields.get(2)?.parse().ok()?,
    ];
    // "x/y entities": y is the total process count
    let counts = fields.get(3)?;
    let procs = counts.split('/').nth(1)?.parse().ok()?;
    Some((loads, procs))
}

/// Seconds since boot, from /proc/uptime.
fn read_uptime() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    text.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .map(|secs| secs as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nmcli_ap_lines_parse() {
        // real shapes: open and secured networks, an escaped colon in
        // an SSID, the hidden network's empty SSID, padding colons
        let text = "HomeNet:82:WPA2:yes\n\
                    Coffee Shop:45:--:no\n\
                    W\\:ired:70:WPA1 WPA2:no\n\
                    :38:WPA2:no\n";
        let points = parse_access_points(text);
        assert_eq!(points.len(), 3, "the hidden network drops out");
        assert_eq!(points[0].ssid, "HomeNet");
        assert!(points[0].secured && points[0].active);
        assert_eq!(points[0].strength, 82);
        assert!(!points[1].secured, "open network");
        assert_eq!(points[2].ssid, "W:ired", "escaped colon is literal");
        assert!(!points[2].active);
    }

    #[test]
    fn nmcli_saved_connections_filter_wireless() {
        let text = "HomeNet:802-11-wireless\n\
                    Wired bridge:ethernet\n\
                    VPN:vpn\n\
                   SSID with spaces:802-11-wireless\n";
        let saved: Vec<String> = text
            .lines()
            .filter_map(|line| line.rsplit_once(':'))
            .filter(|(_, kind)| *kind == "802-11-wireless" || *kind == "wifi")
            .map(|(name, _)| name.to_string())
            .collect();
        assert_eq!(saved, vec!["HomeNet", "SSID with spaces"]);
    }

    #[test]
    fn bluetoothctl_info_block_parses() {
        let text = "Device 1A:2B:3C:4D:5E:6E Headphones\n\
                    \tName: Headphones\n\
                    \tPaired: yes\n\
                    \tTrusted: yes\n\
                    \tConnected: yes\n\
                    \tBattery Percentage: 0x50 (80)\n";
        let device = parse_device_info(
            "1A:2B:3C:4D:5E:6E".to_string(),
            "Headphones".to_string(),
            text,
        );
        assert!(device.connected && device.paired && device.trusted);
        assert_eq!(device.battery, Some(80));
    }

    #[test]
    fn bluetoothctl_info_without_battery_parses() {
        let text = "Device 1A:2B:3C:4D:5E:6E Mouse\n\
                    \tPaired: no\n\
                    \tTrusted: no\n\
                    \tConnected: no\n";
        let device = parse_device_info("mac".to_string(), "Mouse".to_string(), text);
        assert!(!device.connected && !device.paired && !device.trusted);
        assert_eq!(device.battery, None);
    }

    #[test]
    fn cpu_delta_computes_usage_fraction() {
        let previous = CpuSample {
            idle: 1000,
            total: 2000,
            cores: vec![],
        };
        let busy = CpuSample {
            idle: 1000,
            total: 3000,
            cores: vec![],
        };
        let idle = CpuSample {
            idle: 1500,
            total: 3000,
            cores: vec![],
        };
        assert_eq!(cpu_delta(previous.clone(), busy), Some(1.0));
        assert_eq!(cpu_delta(previous.clone(), idle), Some(0.5));
        assert_eq!(cpu_delta(previous.clone(), previous), None);
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
    fn wpctl_status_streams_parse() {
        // the shape wpctl 1.6 prints: top-level section headers, the
        // Audio block's Streams list with channel routing lines under
        // each stream, and a second Streams block under Video that
        // must not leak in
        let text = [
            "PipeWire 'pipewire-0' [1.6.9, martin@motherbox, cookie:3081952736]",
            " └─ Clients:",
            "        33. WirePlumber [1.6.9, martin@motherbox]",
            "Audio",
            " ├─ Devices:",
            " │      42. Radeon HD Audio Controller [alsa]",
            " ├─ Sinks:",
            " │  *   51. Ryzen HD Audio Controller Analog Stereo [vol: 0.50]",
            " ├─ Filters:",
            " │",
            " └─ Streams:",
            "        67. PipeWire ALSA [gst-launch-1.0]                              ",
            "             70. output_FL       > ALC257 Analog:playback_FL\t[active]",
            "        89. Firefox [firefox]",
            "",
            "Video",
            " └─ Streams:",
            "        55. Integrated Camera: Integrated C [libcamera]",
        ]
        .join("\n");
        let streams = parse_streams(&text);
        assert_eq!(
            streams,
            vec![
                (67u32, "PipeWire ALSA".to_string(), "gst-launch-1.0".to_string()),
                (89u32, "Firefox".to_string(), "firefox".to_string()),
            ]
        );
    }

    #[test]
    fn wpctl_status_without_streams_parses_empty() {
        let text = "Audio\n ├─ Sinks:\n │  *   51. Analog Stereo [vol: 0.50]\n └─ Streams:\n\nVideo\n";
        assert!(parse_streams(text).is_empty());
    }

    #[test]
    fn cpu_stat_cores_parse() {
        let text = "cpu  1000 0 1000 8000 0 0 0 0 0 0\n\
                    cpu0 500 0 500 9000 0 0 0 0 0 0\n\
                    cpu1 250 0 250 9500 0 0 0 0 0 0\n\
                    intr 123\n";
        let sample = parse_cpu_stat(text).unwrap();
        assert_eq!(sample.cores.len(), 2);
        assert_eq!(sample.cores[0], (9000, 10000));
        assert_eq!(sample.cores[1], (9500, 10000));
    }

    #[test]
    fn cpu_usage_reads_per_core() {
        let previous = parse_cpu_stat(
            "cpu  1000 0 1000 8000 0 0 0 0 0 0\ncpu0 500 0 500 9000 0 0 0 0 0 0\n",
        )
        .unwrap();
        let sample = parse_cpu_stat(
            "cpu  1500 0 1000 8500 0 0 0 0 0 0\ncpu0 750 0 500 9250 0 0 0 0 0 0\n",
        )
        .unwrap();
        let usage = cpu_usage(previous, sample).unwrap();
        assert_eq!(usage.total, 0.5);
        // core0: half its delta is idle
        assert_eq!(usage.cores, vec![0.5]);
    }

    #[test]
    fn loadavg_parses_loads_and_processes() {
        let (loads, procs) = parse_loadavg("0.42 0.39 0.31 2/2471 12345\n").unwrap();
        assert_eq!(loads, [0.42, 0.39, 0.31]);
        assert_eq!(procs, 2471);
        assert!(parse_loadavg("0.42 0.39\n").is_none());
    }

    #[test]
    fn meminfo_line_parses_to_ram() {
        let text = "MemTotal:       16384000 kB\nMemFree:         1024000 kB\n\
                    MemAvailable:    8192000 kB\nCached:          2048000 kB\n";
        let ram = parse_meminfo(text).unwrap();
        // 16384000 kB = 16000 MiB total, 8000 MiB available, half in use
        assert_eq!(ram.total_mib, 16000);
        assert_eq!(ram.used_mib, 8000);
        assert_eq!(ram.percent, 50);
        // a total of zero is no machine at all
        assert!(parse_meminfo("MemTotal:          0 kB\nMemAvailable:      0 kB\n").is_none());
        // missing fields are None, not a panic
        assert!(parse_meminfo("MemTotal:       16384000 kB\n").is_none());
    }

    #[test]
    fn statvfs_numbers_read_as_disk() {
        // 10 GiB of blocks, 5 GiB free, but the user may only take 4
        // (bavail): the percent is the df number, used over what the
        // user could still fill
        let disk = parse_statvfs(4096, 2_621_440, 1_310_720, 1_048_576).unwrap();
        assert_eq!(disk.total_mib, 10240);
        assert_eq!(disk.used_mib, 5120);
        assert_eq!(disk.percent, 55);
        // a zero-block filesystem is no disk at all
        assert!(parse_statvfs(4096, 0, 0, 0).is_none());
    }

    #[test]
    fn virtual_filesystems_are_denied_the_disk_widget() {
        // the atomic root views and the kernel's trees
        for fstype in ["composefs", "overlay", "squashfs", "tmpfs", "proc"] {
            assert!(is_virtual_fs(fstype), "{fstype} should be virtual");
        }
        for fstype in ["ext4", "btrfs", "xfs", "zfs"] {
            assert!(!is_virtual_fs(fstype), "{fstype} should be real");
        }
    }

    #[test]
    fn the_deepest_mount_names_the_fstype() {
        let mounts = "composefs / overlay ro\n\
                      /dev/sda1 / btrfs ro\n\
                      /dev/sda1 /var btrfs rw\n\
                      /dev/sda2 /var/home ext4 rw\n";
        assert_eq!(
            mount_fstype_of(mounts, "/var/home/martin").as_deref(),
            Some("ext4")
        );
        assert_eq!(mount_fstype_of(mounts, "/var").as_deref(), Some("btrfs"));
        assert_eq!(mount_fstype_of(mounts, "/etc").as_deref(), Some("btrfs"));
        // paths the table covers, answers for; a stranger has none
        assert_eq!(mount_fstype_of("", "/var"), None);
    }

    #[test]
    fn escaped_mount_points_decode() {
        assert_eq!(unescape_mount_path("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape_mount_path("/plain"), "/plain");
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
    fn mute_bounce_wins_the_18ms_double_fire_and_loses_a_human_press() {
        // the field measurement: the mic key emitted pairs 14-18ms apart
        let now = Instant::now();
        assert!(bounced(Some(now - Duration::from_millis(14)), now));
        assert!(bounced(Some(now - Duration::from_millis(18)), now));
        // a deliberate second press lands well past the window
        assert!(!bounced(Some(now - Duration::from_millis(200)), now));
        // and no history is never a bounce
        assert!(!bounced(None, now));
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

    #[test]
    fn the_poll_confirms_a_change_before_it_counts() {
        let mut sysmon = SysMon::default();
        sysmon.volume = Some(Volume {
            percent: 50,
            muted: false,
        });
        sysmon.mic = Some(Mic { muted: true, percent: 42 });
        // a garbage read (the sink shows the mic's mute during a
        // routing flap): stored, not applied
        let garbage = Snapshot {
            volume: Some(Volume {
                percent: 71,
                muted: true,
            }),
            mic: Some(Mic { muted: true, percent: 42 }),
            ..Default::default()
        };
        assert!(!sysmon.absorb_av(&garbage));
        assert_eq!(
            sysmon.volume,
            Some(Volume {
                percent: 50,
                muted: false
            })
        );
        // the garbage is gone on the next read: discarded quietly
        let calm = Snapshot {
            volume: Some(Volume {
                percent: 50,
                muted: false,
            }),
            mic: Some(Mic { muted: true, percent: 42 }),
            ..Default::default()
        };
        assert!(!sysmon.absorb_av(&calm));
        assert_eq!(
            sysmon.volume,
            Some(Volume {
                percent: 50,
                muted: false
            })
        );
        // a real change repeats and then applies
        let mut real = calm;
        real.mic = Some(Mic { muted: false, percent: 42 });
        assert!(!sysmon.absorb_av(&real));
        assert_eq!(sysmon.mic, Some(Mic { muted: true, percent: 42 }));
        assert!(sysmon.absorb_av(&real));
        assert_eq!(sysmon.mic, Some(Mic { muted: false, percent: 42 }));
    }
}
