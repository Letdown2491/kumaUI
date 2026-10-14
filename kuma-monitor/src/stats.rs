//! The machine's vitals: the reads that are not about processes.
//! Load, uptime, pressure (PSI), network and disk throughput, and
//! per-core cpu. Every source is one small file read per beat, and
//! the parsers are pure so the tests can pin them.

use std::path::PathBuf;
use std::time::Instant;

/// One beat of the machine's state, at the same 1s tick as the table.
#[derive(Clone, Debug, Default)]
pub(crate) struct Vitals {
    /// 1, 5 and 15 minute load averages.
    pub load: [f32; 3],
    /// Seconds since boot.
    pub uptime_secs: f64,
    /// Pressure: "some" tasks stalled, avg10, for cpu, memory and io.
    /// None where the kernel predates PSI or the file is unreadable.
    pub psi: [Option<f32>; 3],
    /// Throughput in bytes per second since the previous beat.
    pub net_rx: f64,
    pub net_tx: f64,
    pub io_read: f64,
    pub io_write: f64,
    /// The cpu's temperature in degrees celsius. None where no
    /// known sensor chip exists (containers, some VMs).
    pub cpu_temp: Option<f32>,
}

pub(crate) struct Stats {
    net: Option<Counters>,
    io: Option<Counters>,
    cores: Vec<(u64, u64)>,
    /// The hwmon chip to read the temperature from, discovered once:
    /// the sensor does not move between beats.
    temp_chip: Option<PathBuf>,
    last: Instant,
}

/// Cumulative byte counters.
struct Counters {
    rx: u64,
    tx: u64,
}

impl Stats {
    pub(crate) fn new() -> Self {
        Self {
            net: None,
            io: None,
            cores: Vec::new(),
            temp_chip: discover_temp_chip(),
            last: Instant::now(),
        }
    }

    pub(crate) fn beat(&mut self) -> (Vitals, Vec<f32>) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64();
        self.last = now;

        let load = std::fs::read_to_string("/proc/loadavg")
            .as_deref()
            .map(parse_loadavg)
            .unwrap_or([0.; 3]);
        let uptime_secs = std::fs::read_to_string("/proc/uptime")
            .as_deref()
            .map(parse_uptime)
            .unwrap_or(0.);
        let psi = [
            read_pressure("/proc/pressure/cpu"),
            read_pressure("/proc/pressure/memory"),
            read_pressure("/proc/pressure/io"),
        ];
        let cpu_temp = self.temp_chip.as_ref().and_then(|chip| read_temp(chip));

        let net = std::fs::read_to_string("/proc/net/dev")
            .as_deref()
            .map(parse_net_dev)
            .unwrap_or((0, 0));
        let (net_rx, net_tx) = Self::rates(self.net.as_ref(), &net, dt);
        self.net = Some(Counters {
            rx: net.0,
            tx: net.1,
        });

        let io = std::fs::read_to_string("/proc/diskstats")
            .as_deref()
            .map(parse_diskstats)
            .unwrap_or((0, 0));
        let (io_read, io_write) = Self::rates(self.io.as_ref(), &io, dt);
        self.io = Some(Counters {
            rx: io.0,
            tx: io.1,
        });

        let core_usage = self.core_beat();

        (
            Vitals {
                load,
                uptime_secs,
                psi,
                net_rx,
                net_tx,
                io_read,
                io_write,
                cpu_temp,
            },
            core_usage,
        )
    }

    /// Deltas against the previous beat's counters; the first beat
    /// has nothing to differ against and reads zero.
    fn rates(prev: Option<&Counters>, current: &(u64, u64), dt: f64) -> (f64, f64) {
        match prev {
            Some(prev) if dt > 0. => (
                current.0.saturating_sub(prev.rx) as f64 / dt,
                current.1.saturating_sub(prev.tx) as f64 / dt,
            ),
            _ => (0., 0.),
        }
    }

    /// Per-core usage against the previous beat: (busy time, total
    /// time) deltas per core, htop's math from /proc/stat.
    fn core_beat(&mut self) -> Vec<f32> {
        let text = std::fs::read_to_string("/proc/stat").unwrap_or_default();
        let cores: Vec<(u64, u64)> = text
            .lines()
            .filter(|line| line.starts_with("cpu") && !line.starts_with("cpu "))
            .map(|line| {
                let fields: Vec<u64> = line
                    .split_whitespace()
                    .skip(1)
                    .filter_map(|f| f.parse().ok())
                    .collect();
                let total: u64 = fields.iter().sum();
                let idle = fields.get(3).unwrap_or(&0) + fields.get(4).unwrap_or(&0);
                (idle, total)
            })
            .collect();
        let usage = cores
            .iter()
            .zip(self.cores.iter())
            .map(|(&(idle, total), &(prev_idle, prev_total))| {
                let d_total = total.saturating_sub(prev_total) as f32;
                let d_idle = idle.saturating_sub(prev_idle) as f32;
                if d_total > 0. {
                    ((d_total - d_idle) / d_total * 100.).clamp(0., 100.)
                } else {
                    0.
                }
            })
            .collect();
        self.cores = cores;
        usage
    }
}

/// "0.52 0.58 0.59 2/1234 5678"
fn parse_loadavg(text: &str) -> [f32; 3] {
    let mut load = [0.; 3];
    for (slot, field) in load.iter_mut().zip(text.split_whitespace()) {
        *slot = field.parse().unwrap_or(0.);
    }
    load
}

/// "12345.67 23456.78"
fn parse_uptime(text: &str) -> f64 {
    text.split_whitespace()
        .next()
        .and_then(|f| f.parse().ok())
        .unwrap_or(0.)
}

/// "some avg10=0.34 avg60=1.20 avg300=2.00 total=6000": the "some"
/// avg10 line, in percent of the time any task was stalled.
fn parse_pressure(text: &str) -> Option<f32> {
    let line = text.lines().find(|line| line.starts_with("some"))?;
    let field = line.split_whitespace().find(|f| f.starts_with("avg10="))?;
    field
        .trim_start_matches("avg10=")
        .trim_end_matches('%')
        .parse()
        .ok()
}

fn read_pressure(path: &str) -> Option<f32> {
    parse_pressure(&std::fs::read_to_string(path).ok()?)
}

/// The cpu's sensor chip under /sys/class/hwmon: k10temp or zenpower
/// (AMD), coretemp (Intel), cpu_thermal (ARM boards). The names are
/// the kernel's stable drivers; anything else is not the package
/// sensor and would read a nvme drive or a motherboard chipset.
fn discover_temp_chip() -> Option<PathBuf> {
    for entry in std::fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let name = std::fs::read_to_string(entry.path().join("name")).ok()?;
        if matches!(
            name.trim(),
            "k10temp" | "zenpower" | "coretemp" | "cpu_thermal"
        ) {
            return Some(entry.path());
        }
    }
    None
}

/// The hottest input of the chip, in degrees celsius. coretemp
/// exposes one input per core plus a package: the max is the one
/// that throttles.
fn read_temp(chip: &std::path::Path) -> Option<f32> {
    let mut hottest = None;
    for entry in std::fs::read_dir(chip).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("temp") || !name.ends_with("_input") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(milli) = text.trim().parse::<f32>() else {
            continue;
        };
        hottest = Some(hottest.map_or(milli, |best: f32| best.max(milli)));
    }
    hottest.map(|milli| milli / 1000.)
}

/// Summed rx/tx byte counters over every interface but lo.
fn parse_net_dev(text: &str) -> (u64, u64) {
    let mut totals = (0, 0);
    for line in text.lines() {
        let Some((name, fields)) = line.split_once(':') else {
            continue;
        };
        if name.trim() == "lo" {
            continue;
        }
        let fields: Vec<u64> = fields
            .split_whitespace()
            .filter_map(|f| f.parse().ok())
            .collect();
        // rx bytes then seven fields, then tx bytes
        totals.0 += fields.first().copied().unwrap_or(0);
        totals.1 += fields.get(8).copied().unwrap_or(0);
    }
    totals
}

/// Summed read/write sectors over whole disks only (partitions would
/// double-count their parent's sectors; loops and zram are not disk
/// throughput). Sectors are 512 bytes in practice.
fn parse_diskstats(text: &str) -> (u64, u64) {
    let mut totals = (0, 0);
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 || !is_disk(fields[2]) {
            continue;
        }
        totals.0 += fields.get(5).and_then(|f| f.parse().ok()).unwrap_or(0);
        totals.1 += fields.get(9).and_then(|f| f.parse().ok()).unwrap_or(0);
    }
    totals
}

/// Whole disks: sdX, hdX, vdX, mmcblkN, nvmeNnM. Partitions carry a
/// trailing digit or a pN tail; loop/ram/dm/zram/sr are not disks.
fn is_disk(name: &str) -> bool {
    for prefix in ["loop", "ram", "dm-", "zram", "sr", "fd"] {
        if name.starts_with(prefix) {
            return false;
        }
    }
    for prefix in ["sd", "hd", "vd"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_lowercase());
        }
    }
    if let Some(rest) = name.strip_prefix("nvme") {
        let mut parts = rest.splitn(2, 'n');
        return match (parts.next(), parts.next()) {
            (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => {
                a.bytes().all(|c| c.is_ascii_digit())
                    && b.bytes().all(|c| c.is_ascii_digit())
            }
            _ => false,
        };
    }
    if let Some(rest) = name.strip_prefix("mmcblk") {
        return !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_load_uptime_and_pressure() {
        assert_eq!(parse_loadavg("0.52 0.58 0.59 2/1234 5678"), [0.52, 0.58, 0.59]);
        assert_eq!(parse_loadavg("junk"), [0., 0., 0.]);
        assert_eq!(parse_uptime("12345.67 23456.78"), 12345.67);
        let psi = "some avg10=0.34 avg60=1.20 avg300=2.00 total=6000\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0";
        assert_eq!(parse_pressure(psi), Some(0.34));
        assert_eq!(parse_pressure("full avg10=1.00"), None);
    }

    #[test]
    fn sums_interfaces_but_not_the_loopback() {
        let text = "lo: 100 1 0 0 0 0 0 0 200 1 0 0 0 0 0 0\n\
                    wlan0: 1000 2 0 0 0 0 0 0 3000 3 0 0 0 0 0 0\n\
                    eth0: 10 1 0 0 0 0 0 0 20 1 0 0 0 0 0 0";
        assert_eq!(parse_net_dev(text), (1010, 3020));
    }

    #[test]
    fn sums_whole_disks_only() {
        let text = "   8       0 sda 100 0 2048 0 5 0 1024 0 0 0 0 0 0 0 0 0 0\n\
                    8       1 sda1 50 0 1024 0 3 0 512 0 0 0 0 0 0 0 0 0 0\n\
                    259       0 nvme0n1 200 0 4096 0 10 0 2048 0 0 0 0 0 0 0 0 0 0\n\
                    259       1 nvme0n1p1 100 0 2048 0 5 0 1024 0 0 0 0 0 0 0 0 0 0\n\
                    7       0 loop0 999 0 999 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(parse_diskstats(text), (2048 + 4096, 1024 + 2048));
    }

    #[test]
    fn disk_names_sort_disks_from_partitions() {
        assert!(is_disk("sda"));
        assert!(!is_disk("sda1"));
        assert!(is_disk("nvme0n1"));
        assert!(!is_disk("nvme0n1p1"));
        assert!(is_disk("mmcblk0"));
        assert!(!is_disk("mmcblk0p2"));
        assert!(!is_disk("loop3"));
        assert!(!is_disk("zram0"));
        assert!(!is_disk("dm-0"));
    }

    /// A live vitals beat: uptime and cores are real, throughput is
    /// finite, pressure reads or reads as absent.
    #[test]
    fn a_live_stats_beat_is_sane() {
        let mut stats = Stats::new();
        let (vitals, cores) = stats.beat();
        assert!(vitals.uptime_secs > 0.);
        assert!(vitals.load[0] >= 0.);
        assert!(vitals.net_rx >= 0. && vitals.net_tx >= 0.);
        assert!(vitals.io_read >= 0. && vitals.io_write >= 0.);
        assert!(vitals.psi.iter().all(|p| p.is_none_or(|v| v >= 0.)));
        // a sensor may or may not exist here (the build container has
        // no hwmon); when it does it reads like a temperature
        assert!(vitals
            .cpu_temp
            .is_none_or(|t| (0. ..=150.).contains(&t)));
        // the first beat has no delta baseline: usage comes empty,
        // like the throughput rates
        assert!(cores.is_empty());
        // a second beat produces per-core usage (deltas exist now)
        let (_vitals, cores) = stats.beat();
        assert!(cores.len() >= 1);
        assert!(cores.iter().all(|pct| (0. ..=100.).contains(pct)));
    }
}
