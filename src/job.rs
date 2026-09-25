//! `monolithd job rsync …`: run a copy as a controller job with a truthful progress bar.
//!
//! Made for filesystem day's manual copies and restore tests (planned 2026-09-25). rsync gets
//! `--info=progress2` (one overall line, carriage-return separated) and `--no-inc-recursive`
//! (the file list is complete before copying, so the total, and with it the bar, never moves
//! backwards). The bar is rsync's own overall percentage. rsync's output still reaches the
//! terminal, and the wrapper exits with rsync's exit code.
//!
//! A copy shorter than 5 s never becomes a job. Exit 0 completes the job; anything else, or
//! Ctrl-C / SIGTERM (passed on to rsync), fails it. The job ID carries this process's PID,
//! `rsync:<pid>`, so the reporters service can end a job whose wrapper was killed outright.

use crate::reporter::{Ending, Executor};
use serde_json::Value;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::{interval, MissedTickBehavior};

pub const JOB_PREFIX: &str = "rsync:";
const START_AFTER: Duration = Duration::from_secs(5);
const USAGE: &str = "usage: monolithd job [--dry-run] [--label TEXT] rsync RSYNC-ARGUMENTS...";

/// The overall percentage from one `--info=progress2` line, for example
/// `  1,234,567  45%   12.34MB/s    0:00:12 (xfr#3, to-chk=10/20)`.
fn percent(line: &str) -> Option<u32> {
    line.split_whitespace().find_map(|field| field.strip_suffix('%')?.parse::<u32>().ok()).map(|value| value.min(100))
}

/// Complete lines from a buffer of rsync output, splitting on both `\r` and `\n`; the
/// unfinished remainder stays in the buffer.
fn take_lines(buffer: &mut String) -> Vec<String> {
    let Some(end) = buffer.rfind(['\r', '\n']) else { return Vec::new() };
    let lines = buffer[..end].split(['\r', '\n']).filter(|line| !line.trim().is_empty()).map(str::to_owned).collect();
    buffer.drain(..=end);
    lines
}

struct Arguments {
    dry_run: bool,
    label: Option<String>,
    rsync: Vec<String>,
}

fn parse_arguments(arguments: Vec<String>) -> Result<Arguments, String> {
    let mut words = arguments.into_iter();
    let (mut dry_run, mut label) = (false, None);
    loop {
        match words.next().as_deref() {
            Some("--dry-run") => dry_run = true,
            Some("--label") => label = Some(words.next().ok_or(USAGE)?),
            Some("rsync") => break,
            _ => return Err(USAGE.to_owned()),
        }
    }
    let rsync: Vec<String> = words.collect();
    if rsync.is_empty() {
        return Err(USAGE.to_owned());
    }
    Ok(Arguments { dry_run, label, rsync })
}

/// `monolithd job …`
pub async fn run(arguments: Vec<String>) -> Result<(), String> {
    let arguments = parse_arguments(arguments)?;
    let id = format!("{JOB_PREFIX}{}", std::process::id());
    let label = arguments.label.clone().unwrap_or_else(|| format!("Copy to {}", arguments.rsync.last().map(String::as_str).unwrap_or("?")));
    let mut child = Command::new("rsync")
        .arg("--info=progress2")
        .arg("--no-inc-recursive")
        .args(&arguments.rsync)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start rsync: {error}"))?;
    let pid = child.id().ok_or("rsync exited at once")?;
    let mut stdout = child.stdout.take().ok_or("no rsync output")?;
    let mut terminal = tokio::io::stdout();
    let mut executor = Executor::new("job", arguments.dry_run);
    let (mut interrupt, mut terminate) = (signal(SignalKind::interrupt()).map_err(|error| error.to_string())?, signal(SignalKind::terminate()).map_err(|error| error.to_string())?);
    let (started, mut tick) = (Instant::now(), interval(Duration::from_secs(2)));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let (mut buffer, mut chunk) = (String::new(), [0u8; 8192]);
    let (mut current, mut reported, mut announced, mut interrupted, mut output_open) = (None::<u32>, None::<u32>, false, None::<&str>, true);
    let status = loop {
        tokio::select! {
            read = stdout.read(&mut chunk), if output_open => match read {
                Ok(0) | Err(_) => output_open = false,
                Ok(count) => {
                    let _ = terminal.write_all(&chunk[..count]).await;
                    let _ = terminal.flush().await;
                    buffer.push_str(&String::from_utf8_lossy(&chunk[..count]));
                    if let Some(value) = take_lines(&mut buffer).iter().filter_map(|line| percent(line)).last() {
                        current = Some(value);
                    }
                }
            },
            _ = tick.tick() => {
                let Some(value) = current else { continue };
                if !announced && started.elapsed() >= START_AFTER {
                    announced = executor.start(&id, &label, 100).await;
                    reported = None;
                }
                if announced && reported != Some(value) {
                    if executor.progress(&id, value, 100).await {
                        reported = Some(value);
                    } else {
                        announced = false;
                    }
                }
            }
            _ = interrupt.recv() => {
                // SAFETY: signalling our own child process.
                unsafe { libc::kill(pid as i32, libc::SIGINT) };
                interrupted = Some("interrupted");
            }
            _ = terminate.recv() => {
                // SAFETY: signalling our own child process.
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                interrupted = Some("terminated");
            }
            status = child.wait() => break status.map_err(|error| format!("wait for rsync: {error}"))?,
        }
    };
    let code = status.code().unwrap_or(1);
    if announced {
        let ending = match (interrupted, status.success()) {
            (Some(how), _) => Ending::Fail(format!("copy {how}")),
            (None, true) => Ending::Complete,
            (None, false) => Ending::Fail(format!("rsync exited with code {code}")),
        };
        executor.end(id.clone(), ending).await;
        executor.retry().await;
    }
    std::process::exit(code);
}

/// Is `pid` still a `monolithd job` wrapper? (Its cmdline arguments are NUL-separated.)
fn is_wrapper(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
        let arguments: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
        arguments.first().is_some_and(|program| program.ends_with(b"monolithd")) && arguments.get(1) == Some(&&b"job"[..])
    })
}

/// `rsync:<pid>` jobs whose wrapper is gone (killed outright, or its SSH session dropped).
fn orphans(status: &Value, alive: impl Fn(u32) -> bool) -> Vec<String> {
    status["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|job| job["id"].as_str())
        .filter(|id| id.strip_prefix(JOB_PREFIX).and_then(|pid| pid.parse().ok()).is_some_and(|pid| !alive(pid)))
        .map(str::to_owned)
        .collect()
}

/// End orphaned copy jobs every 30 s, so a killed wrapper cannot hold a bar (and block
/// sleep) until the watchdog's 6 h cap.
pub async fn sweep(dry_run: bool) -> Result<(), String> {
    let mut executor = Executor::new("job-sweep", dry_run);
    let mut tick = interval(Duration::from_secs(30));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        executor.retry().await;
        let Some(status) = executor.status().await else { continue };
        for id in orphans(&status, is_wrapper) {
            executor.end(id, Ending::Fail("the copy's wrapper process is gone".to_owned())).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_overall_percentage() {
        assert_eq!(percent("      1,234,567  45%   12.34MB/s    0:00:12 (xfr#3, to-chk=10/20)"), Some(45));
        assert_eq!(percent("  3,221,225,472 100%  101.10MB/s    0:00:30 (xfr#1, to-chk=0/1)"), Some(100));
        assert_eq!(percent("sending incremental file list"), None);
        assert_eq!(percent("sent 3,221,983,839 bytes  received 35 bytes"), None);
    }

    #[test]
    fn splits_carriage_return_progress_and_keeps_the_remainder() {
        let mut buffer = String::from("          0   0%    0.00kB/s    0:00:00\r    512,000  12%  1.00MB/s    0:00:03\r    1,024,0");
        assert_eq!(take_lines(&mut buffer).iter().filter_map(|line| percent(line)).collect::<Vec<_>>(), vec![0, 12]);
        assert_eq!(buffer, "    1,024,0", "the unfinished line waits for more output");
        buffer.push_str("00  25%  1.00MB/s    0:00:02\n");
        assert_eq!(take_lines(&mut buffer).iter().filter_map(|line| percent(line)).collect::<Vec<_>>(), vec![25]);
    }

    #[test]
    fn only_rsync_jobs_whose_wrapper_is_gone_are_orphans() {
        let status = serde_json::json!({ "jobs": [{ "id": "rsync:100" }, { "id": "rsync:200" }, { "id": "steam:292030" }, { "id": "rsync:x" }] });
        assert_eq!(orphans(&status, |pid| pid == 100), vec!["rsync:200".to_owned()]);
    }

    #[test]
    fn parses_the_command_line() {
        let parsed = parse_arguments(["--dry-run", "--label", "Photos", "rsync", "-a", "src/", "dst/"].map(String::from).to_vec()).unwrap();
        assert!(parsed.dry_run);
        assert_eq!(parsed.label.as_deref(), Some("Photos"));
        assert_eq!(parsed.rsync, vec!["-a", "src/", "dst/"]);
        assert!(parse_arguments(vec!["rsync".into()]).is_err(), "rsync needs arguments");
        assert!(parse_arguments(vec!["cp".into(), "a".into()]).is_err(), "only rsync is wrapped");
    }
}
