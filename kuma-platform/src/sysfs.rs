//! Kernel-state reads: the /proc and /sys samplers and the statvfs
//! disk read, the copies that kuma-shell's sysmon pass and
//! kuma-monitor's beat loop would otherwise each own. The readers
//! take the file's text (pure, testable); the `read_*` fronts take a
//! path and do the file read. Percentages speak 0-100 (htop's math).

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- cpu

/// One /proc/stat reading: the aggregate (idle, total) jiffies and
/// one (idle, total) pair per core. Idle is user-facing idle plus
/// iowait (fields 4 and 5), total the sum of the line.
#[derive(Clone, Debug, PartialEq)]
pub struct CpuSample {
    pub idle: u64,
    pub total: u64,
    pub cores: Vec<(u64, u64)>,
}

/// Parse /proc/stat's text. The aggregate line must carry at least
/// the five classic fields; the cpuN lines after it contribute the
/// per-core pairs.
pub fn cpu_sample(stat: &str) -> Option<CpuSample> {
    let mut lines = stat.lines();
    let aggregate = lines.next()?;
    let fields: Vec<u64> = aggregate
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    if fields.len() < 5 {
        return None;
    }
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

/// Busy percent of the whole package between two samples (previous,
/// current), 0-100. None when no jiffies elapsed (a sample against
/// itself, or a boot-time read).
pub fn usage_percent(previous: (u64, u64), current: (u64, u64)) -> Option<f32> {
    let d_total = current.1.saturating_sub(previous.1);
    let d_idle = current.0.saturating_sub(previous.0);
    if d_total == 0 {
        return None;
    }
    Some(((d_total - d_idle) as f32 / d_total as f32 * 100.0).clamp(0.0, 100.0))
}

/// Per-core busy percents between the two core lists, positionally
/// paired (core i answers core i). A core with no elapsed jiffies
/// reads 0.0 rather than shifting the list.
pub fn core_percents(previous: &[(u64, u64)], current: &[(u64, u64)]) -> Vec<f32> {
    previous
        .iter()
        .zip(current.iter())
        .map(|(&(p_idle, p_total), &(c_idle, c_total))| {
            usage_percent((p_idle, p_total), (c_idle, c_total)).unwrap_or(0.0)
        })
        .collect()
}

// ------------------------------------------------------------- meminfo

/// One counter from /proc/meminfo, in kB as the kernel reports it.
/// None means the counter is absent (the first line that starts with
/// the name wins; the kernel's fixed order keeps "Slab:" ahead of
/// "SlabUnreclaimable").
pub fn meminfo_kb(meminfo: &str, counter: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix(counter))
        .and_then(|rest| {
            rest.trim_start_matches(':')
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok())
        })
}

// ------------------------------------------------------------ loadavg

/// /proc/loadavg: the three averages and the total process count
/// bundled in the fourth field ("2/2471" -> 2471).
pub fn loadavg(text: &str) -> Option<([f32; 3], u32)> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    let loads = [
        fields.first()?.parse().ok()?,
        fields.get(1)?.parse().ok()?,
        fields.get(2)?.parse().ok()?,
    ];
    let counts = fields.get(3)?;
    let procs = counts.split('/').nth(1)?.parse().ok()?;
    Some((loads, procs))
}

/// /proc/uptime: seconds since boot, fractional.
pub fn uptime_secs(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

// -------------------------------------------------------- temperature

/// The cpu sensor candidates' driver names: coretemp (Intel),
/// k10temp and zenpower (AMD), cpu_thermal (ARM boards). The names
/// are the kernel's stable drivers; anything else is not the package
/// sensor and would read an nvme drive or a motherboard chipset.
const CPU_CHIPS: [&str; 4] = ["coretemp", "k10temp", "zenpower", "cpu_thermal"];

/// The hottest temperature input among the CPU-named hwmon chips'
/// zones, with the thermal_zone fallback (x86_pkg_temp, soc thermal)
/// for machines whose CPU sensor only shows there. Degrees.
pub fn probe_cpu_sensor() -> Option<PathBuf> {
    let mut candidates: Vec<(u32, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let dir = entry.path();
            let Ok(name) = std::fs::read_to_string(dir.join("name")) else {
                continue;
            };
            if !CPU_CHIPS.contains(&name.trim()) {
                continue;
            }
            collect_temp_inputs(&dir, &mut candidates);
        }
    }
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
            // the zone's reading lives in its `temp` file (hwmon's
            // tempN_input equivalent): the zone directory itself is a
            // directory, and reading it never answers
            let file = entry.path().join("temp");
            if let Some(temp) = read_temp_input(&file) {
                candidates.push((temp, file));
            }
        }
    }
    // the hottest zone wins
    let hottest = candidates.into_iter().max_by_key(|(temp, _)| *temp);
    hottest.map(|(_, path)| path)
}

/// The first CPU-named hwmon chip's directory: the chip whose
/// hottest input a per-beat reader wants (one flaky read during the
/// walk skips itself, a driver unbinding mid-enumeration must not
/// cost the whole session its sensor).
pub fn cpu_sensor_chip() -> Option<PathBuf> {
    let entries = std::fs::read_dir("/sys/class/hwmon").ok()?;
    for entry in entries.flatten() {
        let Ok(name) = std::fs::read_to_string(entry.path().join("name")) else {
            continue;
        };
        if CPU_CHIPS.contains(&name.trim()) {
            return Some(entry.path());
        }
    }
    None
}

/// The hottest temperature input under a chip directory, in degrees
/// (coretemp exposes one input per core plus a package: the max is
/// the one that throttles).
pub fn hottest_input(chip: &Path) -> Option<f32> {
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

fn collect_temp_inputs(dir: &Path, into: &mut Vec<(u32, PathBuf)>) {
    let Ok(zones) = std::fs::read_dir(dir) else {
        return;
    };
    for zone in zones.flatten() {
        let file = zone.file_name().to_string_lossy().to_string();
        if file.starts_with("temp") && file.ends_with("_input") {
            if let Some(temp) = read_temp_input(&zone.path()) {
                into.push((temp, zone.path()));
            }
        }
    }
}

/// One hwmon/thermal reading: milli-degrees in the file, degrees
/// out. A missing or unparsable file is no reading at all.
pub fn read_temp_input(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse::<u32>().ok().map(|milli| milli / 1000)
}

// --------------------------------------------------------------- disk

/// One mount's usage, the way df reports it: used over what the user
/// could still fill, so the reserved-root blocks don't make a
/// full-feeling disk read two-thirds.
#[derive(Clone, Debug, PartialEq)]
pub struct DiskUsage {
    pub percent: u8,
    pub used_mib: u64,
    pub total_mib: u64,
    /// The user's available (bavail), not the root's.
    pub user_avail_mib: u64,
}

/// The raw statvfs read, both consumers' core: block size and the
/// three block counts. The caller owns the mount-point policy (an
/// atomic root's virtual filesystem lies about the disk under it).
struct RawStatvfs {
    frsize: u64,
    blocks: u64,
    bfree: u64,
    bavail: u64,
}

fn statvfs_raw(mount: &Path) -> Option<RawStatvfs> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(mount.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    Some(RawStatvfs {
        frsize: u64::try_from(stat.f_frsize).ok()?,
        blocks: stat.f_blocks,
        bfree: stat.f_bfree,
        bavail: stat.f_bavail,
    })
}

/// statvfs on a mount. The caller owns the mount-point policy (an
/// atomic root's virtual filesystem lies about the disk under it);
/// this is the raw read.
pub fn statvfs_disk(mount: &Path) -> Option<DiskUsage> {
    let stat = statvfs_raw(mount)?;
    parse_statvfs(
        stat.frsize,
        stat.blocks,
        stat.bfree,
        stat.bavail,
    )
}

/// The user's free bytes (bavail * frsize) on the volume holding
/// mount: the bytes-shaped answer beside DiskUsage's MiB (Koguma's
/// status line wants a size, not a percent).
pub fn statvfs_free_bytes(mount: &Path) -> Option<u64> {
    let stat = statvfs_raw(mount)?;
    stat.bavail.checked_mul(stat.frsize)
}

/// The statvfs math, separated for tests: block size and the three
/// block counts into a DiskUsage. A filesystem that reports more free
/// than total blocks (btrfs profiles do under some raids) is clamped
/// to zero used, it does not wrap into an astronomic number.
pub fn parse_statvfs(frsize: u64, blocks: u64, bfree: u64, bavail: u64) -> Option<DiskUsage> {
    let total_mib = blocks * frsize / (1024 * 1024);
    let used_mib = blocks.saturating_sub(bfree) * frsize / (1024 * 1024);
    let user_avail_mib = bavail * frsize / (1024 * 1024);
    let denominator = used_mib + user_avail_mib;
    if denominator == 0 {
        return None;
    }
    Some(DiskUsage {
        percent: ((used_mib * 100) / denominator) as u8,
        used_mib,
        total_mib,
        user_avail_mib,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_stat_line_parses() {
        let sample = cpu_sample("cpu  99106 646 27178 5737528 25604 0 9318 0 0 0\n").unwrap();
        assert_eq!(sample.idle, 5737528 + 25604);
        assert_eq!(
            sample.total,
            99106 + 646 + 27178 + 5737528 + 25604 + 0 + 9318 + 0 + 0 + 0
        );
        assert!(cpu_sample("garbage").is_none());
    }

    #[test]
    fn cpu_stat_cores_parse() {
        let text = "cpu  1000 0 1000 8000 0 0 0 0 0 0\n\
                    cpu0 500 0 500 9000 0 0 0 0 0 0\n\
                    cpu1 250 0 250 9500 0 0 0 0 0 0\n\
                    intr 123\n";
        let sample = cpu_sample(text).unwrap();
        assert_eq!(sample.cores.len(), 2);
        assert_eq!(sample.cores[0], (9000, 10000));
        assert_eq!(sample.cores[1], (9500, 10000));
    }

    #[test]
    fn usage_percent_is_htops_math() {
        // half the delta busy
        assert_eq!(usage_percent((1000, 2000), (1500, 3000)), Some(50.0));
        // all of it busy
        assert_eq!(usage_percent((1000, 2000), (1000, 3000)), Some(100.0));
        // no jiffies elapsed: no answer
        assert_eq!(usage_percent((1000, 2000), (1000, 2000)), None);
        // idle counters going backwards saturate, they do not wrap
        assert_eq!(usage_percent((2000, 2000), (1000, 3000)), Some(100.0));
    }

    #[test]
    fn core_percents_stay_positional() {
        let previous = [(9000, 10000), (5000, 10000)];
        let current = [(9500, 11000), (5000, 10000)];
        // core0 got busy (half its new jiffies idle), core1 saw no
        // ticks: it reads 0.0 and does not shift the list
        assert_eq!(core_percents(&previous, &current), vec![50.0, 0.0]);
    }

    #[test]
    fn meminfo_counters_read_as_kb() {
        let text = "MemTotal:       16384000 kB\nMemFree:         1024000 kB\n\
                    MemAvailable:    8192000 kB\nSlab:               1024 kB\n\
                    SlabUnreclaimable:   512 kB\n";
        assert_eq!(meminfo_kb(text, "MemTotal"), Some(16384000));
        assert_eq!(meminfo_kb(text, "MemAvailable"), Some(8192000));
        // the fixed order: Slab wins over SlabUnreclaimable
        assert_eq!(meminfo_kb(text, "Slab"), Some(1024));
        // a counter the kernel does not carry is absent, not zero
        assert_eq!(meminfo_kb(text, "MemZttal"), None);
    }

    #[test]
    fn loadavg_parses_loads_and_processes() {
        let (loads, procs) = loadavg("0.42 0.39 0.31 2/2471 12345\n").unwrap();
        assert_eq!(loads, [0.42, 0.39, 0.31]);
        assert_eq!(procs, 2471);
        assert!(loadavg("0.42 0.39\n").is_none());
    }

    #[test]
    fn uptime_reads_fractional_seconds() {
        assert_eq!(uptime_secs("12345.67 23456.78\n"), Some(12345.67));
        assert_eq!(uptime_secs("\n"), None);
    }

    #[test]
    fn statvfs_numbers_read_as_disk() {
        // 10 GiB of blocks, 5 GiB free, but the user may only take 4
        // (bavail): the percent is the df number, used over what the
        // user could still fill
        let disk = parse_statvfs(4096, 2_621_440, 1_310_720, 1_048_576).unwrap();
        assert_eq!(disk.total_mib, 10240);
        assert_eq!(disk.used_mib, 5120);
        assert_eq!(disk.user_avail_mib, 4096);
        assert_eq!(disk.percent, 55);
        // a zero-block filesystem is no disk at all
        assert!(parse_statvfs(4096, 0, 0, 0).is_none());
        // a filesystem reporting more free than total (btrfs under
        // some raids) clamps to zero used, it does not wrap
        let disk = parse_statvfs(4096, 1_000, 2_000, 1_500).unwrap();
        assert_eq!(disk.used_mib, 0);
        assert_eq!(disk.percent, 0);
    }
}
