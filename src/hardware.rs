//! Hardware resets: a Fault on the lights when the machine reset itself after a hardware error
//! (owner request 2026-09-27, after an idle "data fabric sync flood" reset went unnoticed
//! overnight: the machine came back up on its own and nothing looked wrong).
//!
//! On AMD CPUs the kernel logs why the previous reset happened, once per boot:
//! `x86/amd: Previous system reset reason [0x08000800]: an uncorrected error caused a data
//! fabric sync flood event`. A normal reboot logs one too (`software wrote 0x6 to reset control
//! register 0xCF9`), so the reason text decides. Everything is read without root from the
//! journal (`journalctl _TRANSPORT=kernel --grep`, every boot it still holds), so a crash stays
//! visible even if the machine has rebooted normally since, until someone acknowledges it with
//! `monolithd hardware-ack`, which records the boot in `$XDG_STATE_HOME/monolithd/
//! acknowledged-resets`.
//!
//! Error resets raise a **Fault** (every zone); a forced power-off (the power button held for
//! 4 s, usually because the machine hung) raises a **Warning**. Machine-check records logged
//! in the same boot (`mce: [Hardware Error]`) are counted into the reason.

use crate::controller;
use crate::reporter::Executor;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::time::{interval, MissedTickBehavior};

const REASON_MARK: &str = "Previous system reset reason";
const MCE_MARK: &str = "Hardware Error";
const FAULT_PREFIX: &str = "hardware:reset:";
const POLL: Duration = Duration::from_secs(60);
const REASSERT: Duration = Duration::from_secs(30);

/// Reset reasons that are ordinary (reboots, sleep transitions).
const BENIGN: &[&str] = &["software wrote", "software issued PCI reset", "ACPI power state transition", "keyboard reset pin"];
/// Someone held the power button: usually the machine had hung.
const FORCED: &str = "power button was pressed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Fault,
}

/// How a reset reason should be shown; `None` for an ordinary one. Unknown reasons count as
/// errors: the kernel only adds a reason when the firmware reports one.
pub fn classify(reason: &str) -> Option<Severity> {
    if BENIGN.iter().any(|benign| reason.contains(benign)) {
        None
    } else if reason.contains(FORCED) {
        Some(Severity::Warning)
    } else {
        Some(Severity::Fault)
    }
}

/// One reset reason found in the journal: the boot that logged it (the boot *after* the reset).
#[derive(Debug, Clone, PartialEq)]
pub struct Reset {
    pub boot: String,
    /// When that boot logged it, microseconds since the epoch.
    pub at_us: u64,
    pub reason: String,
}

/// Parse `journalctl -o json` lines carrying `MESSAGE`, `_BOOT_ID` and `__REALTIME_TIMESTAMP`.
fn entries(json_lines: &str) -> impl Iterator<Item = (String, u64, String)> + '_ {
    json_lines.lines().filter_map(|line| {
        let entry: Value = serde_json::from_str(line).ok()?;
        let boot = entry["_BOOT_ID"].as_str()?.to_owned();
        let at = entry["__REALTIME_TIMESTAMP"].as_str().and_then(|at| at.parse().ok()).unwrap_or(0);
        Some((boot, at, entry["MESSAGE"].as_str()?.to_owned()))
    })
}

pub fn parse_resets(json_lines: &str) -> Vec<Reset> {
    entries(json_lines)
        .filter_map(|(boot, at_us, message)| {
            let reason = message.split_once(REASON_MARK)?.1.split_once("]: ").map(|(_, text)| text.trim().to_owned())?;
            Some(Reset { boot, at_us, reason })
        })
        .collect()
}

/// Machine-check records per boot.
pub fn count_by_boot(json_lines: &str) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for (boot, _, _) in entries(json_lines) {
        *counts.entry(boot).or_insert(0) += 1;
    }
    counts
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub id: String,
    pub severity: Severity,
    pub reason: String,
    pub boot: String,
}

/// Local date and time, `YYYY-MM-DD HH:MM`.
fn local_time(at_us: u64) -> String {
    let seconds = (at_us / 1_000_000) as libc::time_t;
    // SAFETY: localtime_r only writes the tm we own.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return format!("@{seconds}");
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

/// Every unacknowledged reset that needs showing.
pub fn findings(resets: &[Reset], machine_checks: &BTreeMap<String, usize>, acknowledged: &BTreeSet<String>) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    for reset in resets {
        let Some(severity) = classify(&reset.reason) else { continue };
        if acknowledged.contains(&reset.boot) || out.iter().any(|finding| finding.boot == reset.boot) {
            continue;
        }
        let checks = machine_checks.get(&reset.boot).copied().unwrap_or(0);
        let what = match severity {
            Severity::Fault => "the machine reset itself after a hardware error",
            Severity::Warning => "the machine was forced off with the power button (it had probably hung)",
        };
        let reason = format!(
            "{what}: {} (found by the boot of {}; {checks} machine-check record(s)). Acknowledge with `monolithd hardware-ack`",
            reset.reason,
            local_time(reset.at_us)
        );
        out.push(Finding { id: format!("{FAULT_PREFIX}{}", &reset.boot[..reset.boot.len().min(8)]), severity, reason, boot: reset.boot.clone() });
    }
    out
}

async fn journal(pattern: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("journalctl")
        .args(["_TRANSPORT=kernel", "--grep", pattern, "-o", "json", "--output-fields=MESSAGE,_BOOT_ID", "--no-pager", "-q"])
        .output()
        .await
        .map_err(|error| format!("run journalctl: {error}"))?;
    // journalctl exits 1 when nothing matches.
    if !output.status.success() && !output.stdout.is_empty() {
        return Err(format!("journalctl exited with {}: {}", output.status, String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn ack_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("monolithd").join("acknowledged-resets")
}

fn acknowledged() -> BTreeSet<String> {
    std::fs::read_to_string(ack_path()).unwrap_or_default().lines().map(|line| line.trim().to_owned()).filter(|line| !line.is_empty()).collect()
}

/// What needs showing right now, read from the journal.
pub async fn current() -> Result<Vec<Finding>, String> {
    let resets = parse_resets(&journal(REASON_MARK).await?);
    let checks = count_by_boot(&journal(MCE_MARK).await?);
    Ok(findings(&resets, &checks, &acknowledged()))
}

/// Watch for as long as the reporters service runs.
pub async fn follow(dry_run: bool) -> Result<(), String> {
    let mut executor = Executor::new("hardware-reporter", dry_run);
    let mut raised: BTreeMap<String, (Severity, String)> = BTreeMap::new();
    if let Some(status) = executor.status().await {
        for fault in status["active_faults"].as_array().into_iter().flatten() {
            if let Some(id) = fault["id"].as_str().filter(|id| id.starts_with(FAULT_PREFIX)) {
                let severity = if fault["severity"] == "fault" { Severity::Fault } else { Severity::Warning };
                raised.insert(id.to_owned(), (severity, fault["reason"].as_str().unwrap_or("").to_owned()));
            }
        }
    }
    let mut tick = interval(POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_reassert = Instant::now();
    let mut last_error: Option<String> = None;
    loop {
        tick.tick().await;
        let want: BTreeMap<String, (Severity, String)> = match current().await {
            Ok(found) => {
                last_error = None;
                found.into_iter().map(|finding| (finding.id, (finding.severity, finding.reason))).collect()
            }
            Err(error) => {
                // Unreadable journal: keep what is raised rather than clearing it.
                if last_error.as_deref() != Some(error.as_str()) {
                    executor.log(format!("cannot read the journal: {error}"));
                    last_error = Some(error);
                }
                continue;
            }
        };
        for id in raised.keys().filter(|id| !want.contains_key(*id)).cloned().collect::<Vec<_>>() {
            executor.clear(&id).await;
        }
        let reassert = last_reassert.elapsed() >= REASSERT;
        for (id, (severity, reason)) in &want {
            if raised.get(id) != Some(&(*severity, reason.clone())) {
                match severity {
                    Severity::Fault => executor.fault(id, reason).await,
                    Severity::Warning => executor.warn(id, reason).await,
                }
            } else if reassert {
                executor.raise_as(id, if *severity == Severity::Fault { "fault" } else { "warning" }, reason).await;
            }
        }
        if reassert {
            last_reassert = Instant::now();
        }
        raised = want;
    }
}

/// `monolithd hardware-status`: print what the reporter would show, once.
pub async fn status() -> Result<(), String> {
    let found = current().await?;
    if found.is_empty() {
        println!("no unacknowledged hardware resets in the journal");
    }
    for finding in found {
        println!("{:?} {}: {}", finding.severity, finding.id, finding.reason);
    }
    Ok(())
}

/// `monolithd hardware-ack`: acknowledge every reset shown now, and clear its fault.
pub async fn acknowledge() -> Result<(), String> {
    let found = current().await?;
    if found.is_empty() {
        println!("nothing to acknowledge");
        return Ok(());
    }
    let path = ack_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    }
    let mut known = acknowledged();
    for finding in &found {
        known.insert(finding.boot.clone());
    }
    let text: String = known.iter().map(|boot| format!("{boot}\n")).collect();
    std::fs::write(&path, text).map_err(|error| format!("write {}: {error}", path.display()))?;
    for finding in &found {
        println!("acknowledged {}: {}", finding.id, finding.reason);
        if let Err(error) = controller::call(&json!({ "op": "fault.clear", "id": finding.id })).await {
            println!("  (the controller could not be told now: {error}; the reporter clears it within a minute)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real kernel lines from White Monolith, 2026-09-26 and 2026-09-27.
    const RESETS: &str = concat!(
        r#"{"_BOOT_ID":"b0f722d5d35b45d0a795e427faf0de65","__REALTIME_TIMESTAMP":"1790480395794319","MESSAGE":"x86/amd: Previous system reset reason [0x08000800]: an uncorrected error caused a data fabric sync flood event"}"#,
        "\n",
        r#"{"_BOOT_ID":"c8378a62cf6642a696af6d60cc387466","__REALTIME_TIMESTAMP":"1790514260770415","MESSAGE":"x86/amd: Previous system reset reason [0x00080800]: software wrote 0x6 to reset control register 0xCF9"}"#,
        "\n",
    );
    const MCES: &str = concat!(
        r#"{"_BOOT_ID":"b0f722d5d35b45d0a795e427faf0de65","MESSAGE":"mce: [Hardware Error]: CPU 8: Machine Check: 0 Bank 5: bea0000000000108"}"#,
        "\n",
        r#"{"_BOOT_ID":"b0f722d5d35b45d0a795e427faf0de65","MESSAGE":"mce: [Hardware Error]: CPU 10: Machine Check: 0 Bank 5: bea0000000000108"}"#,
        "\n",
    );

    #[test]
    fn a_sync_flood_is_a_fault_and_a_normal_reboot_is_nothing() {
        let found = findings(&parse_resets(RESETS), &count_by_boot(MCES), &BTreeSet::new());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].id.as_str(), found[0].severity), ("hardware:reset:b0f722d5", Severity::Fault));
        assert!(found[0].reason.contains("data fabric sync flood") && found[0].reason.contains("2 machine-check record(s)"), "{}", found[0].reason);
    }

    #[test]
    fn an_acknowledged_reset_is_not_shown_again() {
        let acknowledged: BTreeSet<String> = ["b0f722d5d35b45d0a795e427faf0de65".to_owned()].into();
        assert!(findings(&parse_resets(RESETS), &BTreeMap::new(), &acknowledged).is_empty());
    }

    #[test]
    fn reasons_are_classified() {
        assert_eq!(classify("an uncorrected error caused a data fabric sync flood event"), Some(Severity::Fault));
        assert_eq!(classify("thermal pin BP_THERMTRIP_L was tripped"), Some(Severity::Fault));
        assert_eq!(classify("hardware watchdog timer expired"), Some(Severity::Fault));
        assert_eq!(classify("power button was pressed for 4 seconds"), Some(Severity::Warning));
        assert_eq!(classify("software wrote 0x6 to reset control register 0xCF9"), None);
        assert_eq!(classify("ACPI power state transition occurred"), None);
        assert!(parse_resets("not json\n{\"MESSAGE\":\"no boot id\"}\n").is_empty());
    }
}
