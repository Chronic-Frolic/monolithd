//! Storage health and scrub progress, unprivileged (planned 2026-09-25 for filesystem day).
//!
//! Everything here is read without root: mounts from `/proc/self/mountinfo`; each btrfs
//! filesystem's devices, their error counters and whether one is missing (a degraded mirror)
//! from `/sys/fs/btrfs/<fsid>/devinfo/<n>/{error_stats,missing}`; scrub state from
//! `btrfs scrub status`, whose text format comes from btrfs-progs `cmds/scrub.c` (v7.1 has no
//! JSON for it). Scrubs are started by a root timer; this only observes them. Whether a
//! running scrub's progress is visible to an unprivileged user (through btrfs's status file,
//! since the progress ioctl needs root) is still to be confirmed live.
//!
//! Owner decision 2026-09-25: a **Fault** only for data-integrity threats (a watched mount
//! missing, a degraded mirror, uncorrectable scrub errors); a **Warning** for recoverable
//! problems (device error counters above zero, corrected scrub errors). The counters persist
//! until reset (`sudo btrfs device stats -z`), so a warning stays until someone looks.

use crate::reporter::{Ending, Executor};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::time::{interval, MissedTickBehavior};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Scrub {
    /// `running`, `finished`, `aborted` or `interrupted`; `None` when never scrubbed.
    pub status: Option<String>,
    pub percent: Option<f64>,
    pub corrected: u64,
    pub uncorrectable: u64,
}

/// Parse `btrfs scrub status MOUNT` (btrfs-progs `_print_scrub_ss` and `print_scrub_summary`).
pub fn parse_scrub(text: &str) -> Scrub {
    let mut scrub = Scrub::default();
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Status:") {
            scrub.status = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("Bytes scrubbed:") {
            scrub.percent = value.rsplit_once('(').and_then(|(_, rest)| rest.trim_end_matches(')').trim().strip_suffix('%')?.parse().ok());
        } else if let Some(value) = line.strip_prefix("Corrected:") {
            scrub.corrected = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("Uncorrectable:") {
            scrub.uncorrectable = value.trim().parse().unwrap_or(0);
        }
    }
    scrub
}

/// The mount's source device and filesystem type from `/proc/self/mountinfo`.
pub fn mount_source(mountinfo: &str, mount: &str) -> Option<(String, String)> {
    mountinfo.lines().find_map(|line| {
        let (left, right) = line.split_once(" - ")?;
        let fields: Vec<&str> = left.split_whitespace().collect();
        if *fields.get(4)? != mount {
            return None;
        }
        let mut right = right.split_whitespace();
        let fstype = right.next()?.to_owned();
        Some((right.next()?.to_owned(), fstype))
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: String,
    pub missing: bool,
    /// Error counters above zero, by name.
    pub errors: BTreeMap<String, u64>,
}

/// Devices of the btrfs filesystem in `fs_dir` (`/sys/fs/btrfs/<fsid>`).
pub fn devices(fs_dir: &Path) -> Vec<Device> {
    let mut found: Vec<Device> = std::fs::read_dir(fs_dir.join("devinfo"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            let read = |name: &str| std::fs::read_to_string(entry.path().join(name)).unwrap_or_default();
            let errors = read("error_stats")
                .lines()
                .filter_map(|line| {
                    let (name, count) = line.split_once(' ')?;
                    let count: u64 = count.trim().parse().ok()?;
                    (count > 0).then(|| (name.to_owned(), count))
                })
                .collect();
            Device { id: entry.file_name().to_string_lossy().into_owned(), missing: read("missing").trim() == "1", errors }
        })
        .collect();
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

/// `/sys/fs/btrfs/<fsid>` for the filesystem whose devices include `source` (e.g. `/dev/sdb1`).
pub fn filesystem_dir(source: &str) -> Option<std::path::PathBuf> {
    let name = Path::new(source).file_name()?.to_string_lossy().into_owned();
    let real = std::fs::canonicalize(source).ok().and_then(|path| path.file_name().map(|name| name.to_string_lossy().into_owned()));
    std::fs::read_dir("/sys/fs/btrfs").ok()?.flatten().map(|entry| entry.path()).find(|dir| {
        let devices = dir.join("devices");
        devices.join(&name).exists() || real.as_ref().is_some_and(|real| devices.join(real).exists())
    })
}

/// A health finding, with a stable fault ID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Finding {
    Fault { id: String, reason: String },
    Warning { id: String, reason: String },
}

/// What one watched mount looks like right now.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Observed {
    pub mounted_btrfs: bool,
    pub fstype: Option<String>,
    pub devices: Vec<Device>,
    pub scrub: Scrub,
}

/// The health findings for one watched mount (`name` is the mount path).
pub fn findings(name: &str, observed: &Observed) -> Vec<Finding> {
    let id = |what: &str| format!("storage:{name}:{what}");
    let mut out = Vec::new();
    if !observed.mounted_btrfs {
        let reason = match &observed.fstype {
            Some(fstype) => format!("{name} is mounted as {fstype}, not btrfs"),
            None => format!("{name} is not mounted"),
        };
        out.push(Finding::Fault { id: id("mount"), reason });
        return out;
    }
    let missing: Vec<&str> = observed.devices.iter().filter(|device| device.missing).map(|device| device.id.as_str()).collect();
    if !missing.is_empty() {
        out.push(Finding::Fault { id: id("degraded"), reason: format!("{name} is degraded: device {} missing", missing.join(", ")) });
    }
    if observed.scrub.uncorrectable > 0 {
        out.push(Finding::Fault { id: id("uncorrectable"), reason: format!("{name}: the last scrub found {} uncorrectable error(s)", observed.scrub.uncorrectable) });
    }
    let errors: Vec<String> = observed
        .devices
        .iter()
        .filter(|device| !device.errors.is_empty())
        .map(|device| format!("device {}: {}", device.id, device.errors.iter().map(|(kind, count)| format!("{kind} {count}")).collect::<Vec<_>>().join(", ")))
        .collect();
    if !errors.is_empty() {
        out.push(Finding::Warning { id: id("device-errors"), reason: format!("{name} has device errors ({}); reset with btrfs device stats -z after checking", errors.join("; ")) });
    }
    if observed.scrub.corrected > 0 {
        out.push(Finding::Warning { id: id("corrected"), reason: format!("{name}: the last scrub corrected {} error(s)", observed.scrub.corrected) });
    }
    out
}

/// Read one watched mount now.
pub async fn observe(mount: &str) -> Observed {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let Some((source, fstype)) = mount_source(&mountinfo, mount) else { return Observed::default() };
    if fstype != "btrfs" {
        return Observed { fstype: Some(fstype), ..Observed::default() };
    }
    let devices = filesystem_dir(&source).map(|dir| devices(&dir)).unwrap_or_default();
    let scrub = tokio::process::Command::new("btrfs").args(["scrub", "status", mount]).output().await.map(|output| parse_scrub(&String::from_utf8_lossy(&output.stdout))).unwrap_or_default();
    Observed { mounted_btrfs: true, fstype: Some(fstype), devices, scrub }
}

const POLL: Duration = Duration::from_secs(10);
const REASSERT: Duration = Duration::from_secs(30);
const FAULT_PREFIX: &str = "storage:";
const SCRUB_PREFIX: &str = "scrub:";

/// Watch `mounts` for as long as the reporters service runs: raise and clear storage faults
/// and warnings, and show a running scrub as a job (btrfs's own percentage).
pub async fn follow(mounts: Vec<String>, dry_run: bool) -> Result<(), String> {
    let mut executor = Executor::new("storage-reporter", dry_run);
    // Findings currently raised: id -> (is a Fault, reason). Adopted from the controller on
    // start, so a finding that has gone away while the service was down is cleared.
    let mut raised: BTreeMap<String, (bool, String)> = BTreeMap::new();
    let mut scrubs: BTreeMap<String, bool> = BTreeMap::new();
    if let Some(status) = executor.status().await {
        for fault in status["active_faults"].as_array().into_iter().flatten() {
            if let Some(id) = fault["id"].as_str().filter(|id| id.starts_with(FAULT_PREFIX)) {
                raised.insert(id.to_owned(), (fault["severity"] == "fault", fault["reason"].as_str().unwrap_or("").to_owned()));
            }
        }
        for job in status["jobs"].as_array().into_iter().flatten() {
            if let Some(mount) = job["id"].as_str().and_then(|id| id.strip_prefix(SCRUB_PREFIX)) {
                scrubs.insert(mount.to_owned(), true);
            }
        }
    }
    executor.log(if mounts.is_empty() { "no storage mounts to watch yet (config/reporters.toml [storage] mounts)".to_owned() } else { format!("watching {}", mounts.join(", ")) });
    let mut tick = interval(POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_reassert = Instant::now();
    loop {
        tick.tick().await;
        executor.retry().await;
        let mut want: BTreeMap<String, (bool, String)> = BTreeMap::new();
        for mount in &mounts {
            let observed = observe(mount).await;
            for finding in findings(mount, &observed) {
                match finding {
                    Finding::Fault { id, reason } => want.insert(id, (true, reason)),
                    Finding::Warning { id, reason } => want.insert(id, (false, reason)),
                };
            }
            let id = format!("{SCRUB_PREFIX}{mount}");
            let status = observed.scrub.status.clone().unwrap_or_default();
            if status == "running" {
                let percent = (observed.scrub.percent.unwrap_or(0.0).floor() as u32).min(100);
                if !scrubs.get(mount).copied().unwrap_or(false) {
                    let announced = executor.start(&id, &format!("Scrub {mount}"), 100).await;
                    scrubs.insert(mount.clone(), announced);
                }
                if scrubs.get(mount).copied().unwrap_or(false) && !executor.progress(&id, percent, 100).await {
                    scrubs.insert(mount.clone(), false);
                }
            } else if scrubs.remove(mount).unwrap_or(false) {
                let ending = if status == "finished" { Ending::Complete } else { Ending::Fail(format!("scrub {}", if status.is_empty() { "state unknown" } else { status.as_str() })) };
                executor.end(id, ending).await;
            }
        }
        // A scrub job adopted for a mount that is no longer watched cannot be followed.
        for (mount, announced) in std::mem::take(&mut scrubs) {
            if mounts.contains(&mount) {
                scrubs.insert(mount, announced);
            } else if announced {
                executor.end(format!("{SCRUB_PREFIX}{mount}"), Ending::Fail("the mount is no longer watched".to_owned())).await;
            }
        }
        for id in raised.keys().filter(|id| !want.contains_key(*id)).cloned().collect::<Vec<_>>() {
            executor.clear(&id).await;
        }
        let reassert = last_reassert.elapsed() >= REASSERT;
        for (id, (fault, reason)) in &want {
            if raised.get(id) != Some(&(*fault, reason.clone())) {
                if *fault { executor.fault(id, reason).await } else { executor.warn(id, reason).await }
            } else if reassert {
                executor.raise_as(id, if *fault { "fault" } else { "warning" }, reason).await;
            }
        }
        if reassert {
            last_reassert = Instant::now();
        }
        raised = want;
    }
}

/// `monolithd storage-status MOUNT...`: print what the reporter would see, once.
pub async fn run(arguments: Vec<String>) -> Result<(), String> {
    if arguments.is_empty() {
        return Err("usage: monolithd storage-status MOUNT...".to_owned());
    }
    for mount in &arguments {
        let observed = observe(mount).await;
        println!("{mount}: mounted btrfs {}, devices {:?}, scrub {:?}", observed.mounted_btrfs, observed.devices, observed.scrub);
        for finding in findings(mount, &observed) {
            println!("  {finding:?}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEVER: &str = "UUID:             b36d48e5-27c7-45c4-b51f-6ab6d18e7d66\n\tno stats available\nTotal to scrub:   116.80GiB\nRate:             0.00B/s\nError summary:    no errors found\n";
    const RUNNING: &str = "UUID:             4a1c-…\nScrub started:    Sat Sep 26 03:00:01 2026\nStatus:           running\nDuration:         0:12:40\nTime left:        0:31:02\nETA:              Sat Sep 26 03:43:43 2026\nTotal to scrub:   912.00GiB\nBytes scrubbed:   265.12GiB  (29.07%)\nRate:             357.30MiB/s\nError summary:    no errors found\n";
    const FINISHED_WITH_ERRORS: &str = "Scrub started:    Sat Sep 26 03:00:01 2026\nStatus:           finished\nDuration:         0:43:12\nTotal to scrub:   912.00GiB\nRate:             360.11MiB/s\nError summary:    csum=5\n  Corrected:      4\n  Uncorrectable:  1\n  Unverified:     0\n";

    #[test]
    fn parses_never_running_and_finished_scrubs() {
        assert_eq!(parse_scrub(NEVER), Scrub::default());
        assert_eq!(parse_scrub(RUNNING), Scrub { status: Some("running".into()), percent: Some(29.07), corrected: 0, uncorrectable: 0 });
        let finished = parse_scrub(FINISHED_WITH_ERRORS);
        assert_eq!((finished.status.as_deref(), finished.corrected, finished.uncorrectable), (Some("finished"), 4, 1));
    }

    #[test]
    fn finds_the_source_of_a_mount() {
        let mountinfo = "29 1 259:3 /root / rw,relatime shared:1 - btrfs /dev/nvme0n1p3 rw,compress=zstd:1\n88 29 259:3 /home /var/home rw,relatime shared:40 - btrfs /dev/nvme0n1p3 rw\n90 29 8:1 / /run/media/system/monolith-storage rw - ext4 /dev/sda1 rw\n";
        assert_eq!(mount_source(mountinfo, "/var/home"), Some(("/dev/nvme0n1p3".into(), "btrfs".into())));
        assert_eq!(mount_source(mountinfo, "/run/media/system/monolith-storage"), Some(("/dev/sda1".into(), "ext4".into())));
        assert_eq!(mount_source(mountinfo, "/storage/protected"), None);
    }

    #[test]
    fn reads_device_counters_and_missing_flags_from_sysfs() {
        let root = std::env::temp_dir().join(format!("storage-sysfs-{}", std::process::id()));
        for (id, errors, missing) in [("1", "write_errs 0\nread_errs 0\ncorruption_errs 0\n", "0"), ("2", "write_errs 0\nread_errs 3\ncorruption_errs 1\n", "1")] {
            let dir = root.join("devinfo").join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("error_stats"), errors).unwrap();
            std::fs::write(dir.join("missing"), missing).unwrap();
        }
        let found = devices(&root);
        assert_eq!(found[0], Device { id: "1".into(), missing: false, errors: BTreeMap::new() });
        assert_eq!(found[1], Device { id: "2".into(), missing: true, errors: BTreeMap::from([("corruption_errs".into(), 1), ("read_errs".into(), 3)]) });
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn maps_conditions_to_faults_and_warnings() {
        let name = "/storage/protected";
        assert_eq!(findings(name, &Observed::default()), vec![Finding::Fault { id: "storage:/storage/protected:mount".into(), reason: "/storage/protected is not mounted".into() }]);
        let healthy = Observed { mounted_btrfs: true, fstype: Some("btrfs".into()), devices: vec![Device { id: "1".into(), missing: false, errors: BTreeMap::new() }], scrub: parse_scrub(RUNNING) };
        assert!(findings(name, &healthy).is_empty());
        let bad = Observed {
            devices: vec![Device { id: "1".into(), missing: false, errors: BTreeMap::from([("read_errs".into(), 3)]) }, Device { id: "2".into(), missing: true, errors: BTreeMap::new() }],
            scrub: parse_scrub(FINISHED_WITH_ERRORS),
            ..healthy
        };
        let kinds: Vec<(bool, String)> = findings(name, &bad).into_iter().map(|finding| match finding { Finding::Fault { id, .. } => (true, id), Finding::Warning { id, .. } => (false, id) }).collect();
        assert_eq!(kinds, vec![
            (true, "storage:/storage/protected:degraded".into()),
            (true, "storage:/storage/protected:uncorrectable".into()),
            (false, "storage:/storage/protected:device-errors".into()),
            (false, "storage:/storage/protected:corrected".into()),
        ]);
    }
}
