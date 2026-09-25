//! Steam download reporter: turns Steam's own update state into truthful controller jobs.
//!
//! State comes from Steam's `logs/content_log.txt`, which records every app's state change as
//! text ("Fully Installed,Update Queued,Update Running,") the moment it happens, plus the
//! update's byte totals when it starts. Progress comes from the app's `appmanifest_<appid>.acf`
//! counters, `BytesDownloaded + BytesStaged` of `BytesToDownload + BytesToStage`, reported in
//! KiB because job totals are u32. The manifest is trusted only if Steam rewrote it after the
//! current `update started` line and its totals match that line's; otherwise (a manifest left by
//! the previous or an interrupted attempt, which can have identical totals) the job reports 0
//! of the log total, understating rather than overstating. Whether Steam rewrites those counters during a download
//! or only at the end had not been measured when this was written (2026-09-24); if only at the
//! end, the bar holds at 0 until the update completes, which is coarse but still truthful.
//!
//! Policy (owner, 2026-09-24): only an update that is actually running counts, never a queued,
//! paused or suspended one, because a leased job blocks sleep; shader-cache updates are
//! ignored; an update becomes a job only after running for 10 s; bars show in Gaming Mode too.
//! A pause ends the job with `job-fail` (no LED effect, only an `event status` entry) and a
//! resume announces it again. Job IDs are `steam:<appid>`; a controller restart is answered by
//! re-announcing, and on its own start the reporter adopts or ends `steam:` jobs it left behind.

use crate::controller;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tokio::time::{interval, MissedTickBehavior};

const POLL: Duration = Duration::from_secs(2);
/// An update must run this long before it becomes a job, so short runtime and Proton updates
/// do not flash a bar and its 15 s completion hold.
const START_AFTER: Duration = Duration::from_secs(10);
/// Steam passes through `Update Queued` for an instant when an update ends or restarts, so a
/// running job is finished only once its app's state has been quiet this long.
const SETTLE: Duration = Duration::from_secs(3);
const JOB_PREFIX: &str = "steam:";

// ---------------------------------------------------------------- content_log parsing

#[derive(Debug, Clone, PartialEq)]
enum LogEvent {
    State { running: bool, queued: bool },
    Shader { running: bool },
    Started { total: u64 },
    Finished { result: String },
}

/// The `N` in `name done/N` from an `update started` line.
fn started_total(totals: &str, name: &str) -> u64 {
    totals
        .split(',')
        .find_map(|part| part.trim().strip_prefix(name)?.trim().split_once('/')?.1.trim().parse().ok())
        .unwrap_or(0)
}

fn parse_line(line: &str) -> Option<(u32, LogEvent)> {
    let rest = line.split_once("] AppID ")?.1;
    let (app, rest) = rest.split_once(' ')?;
    let app = app.parse().ok()?;
    let event = if let Some(flags) = rest.strip_prefix("state changed : ") {
        let flags: Vec<&str> = flags.split(',').map(str::trim).collect();
        LogEvent::State { running: flags.contains(&"Update Running"), queued: flags.contains(&"Update Queued") }
    } else if let Some(state) = rest.strip_prefix("Shader update changed : ") {
        LogEvent::Shader { running: state.trim() != "None" }
    } else if let Some(totals) = rest.strip_prefix("update started : ") {
        LogEvent::Started { total: started_total(totals, "download") + started_total(totals, "stage") }
    } else if let Some(finished) = rest.strip_prefix("scheduler finished : ") {
        LogEvent::Finished { result: finished.split_once("(result ")?.1.split(',').next()?.trim().to_owned() }
    } else {
        return None;
    };
    Some((app, event))
}

// ---------------------------------------------------------------- per-app state

#[derive(Debug, Default)]
struct App {
    running: bool,
    queued: bool,
    shader: bool,
    /// Download plus stage bytes from the latest `update started` line, and when it was seen.
    log_total: u64,
    started_at: Option<SystemTime>,
    result: Option<String>,
    changed: Option<Instant>,
    since: Option<Instant>,
    announced: bool,
}

impl App {
    fn active(&self, steam_alive: bool) -> bool {
        steam_alive && self.running && !self.shader
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    /// Announce if needed, then report progress.
    Report { app: u32 },
    Complete { app: u32 },
    Fail { app: u32, reason: String },
}

#[derive(Default)]
struct Tracker {
    apps: BTreeMap<u32, App>,
}

impl Tracker {
    fn apply(&mut self, app: u32, event: LogEvent, now: Instant) {
        let entry = self.apps.entry(app).or_default();
        entry.changed = Some(now);
        match event {
            LogEvent::State { running, queued } => {
                // A new update: forget the previous one's result and totals, so its finished
                // manifest cannot pass for this update's progress before `update started` arrives
                // (Steam preallocates first, which can outlast the 10 s gate).
                if running && !entry.running {
                    entry.result = None;
                    entry.log_total = 0;
                    entry.started_at = Some(SystemTime::now());
                }
                entry.running = running;
                entry.queued = queued;
            }
            LogEvent::Shader { running } => entry.shader = running,
            LogEvent::Started { total } => {
                entry.log_total = total;
                entry.started_at = Some(SystemTime::now());
            }
            LogEvent::Finished { result } => entry.result = Some(result),
        }
    }

    fn set_announced(&mut self, app: u32, announced: bool) {
        if let Some(entry) = self.apps.get_mut(&app) {
            entry.announced = announced;
        }
    }

    /// What to tell the controller now. An announced job whose update stopped is ended exactly
    /// once: completed if Steam left the queue cleanly, failed if it paused or reported an error.
    fn plan(&mut self, now: Instant, steam_alive: bool) -> Vec<Action> {
        let mut actions = Vec::new();
        for (&app, entry) in &mut self.apps {
            if entry.active(steam_alive) {
                let since = *entry.since.get_or_insert(now);
                if entry.announced || now.duration_since(since) >= START_AFTER {
                    actions.push(Action::Report { app });
                }
                continue;
            }
            entry.since = None;
            if !entry.announced {
                continue;
            }
            let settled = entry.changed.is_none_or(|changed| now.duration_since(changed) >= SETTLE);
            let action = if !steam_alive {
                Action::Fail { app, reason: "Steam is not running".to_owned() }
            } else if !settled {
                continue;
            } else if entry.queued {
                Action::Fail { app, reason: "paused by Steam".to_owned() }
            } else {
                match &entry.result {
                    Some(result) if result != "No Error" => Action::Fail { app, reason: format!("Steam reported {result}") },
                    _ => Action::Complete { app },
                }
            };
            entry.announced = false;
            actions.push(action);
        }
        actions
    }
}

// ---------------------------------------------------------------- Steam files

/// Top-level `"key" "value"` pairs of a Valve KeyValues file; nested sections are flattened
/// and the first occurrence of a key wins, which keeps `AppState`'s own fields.
fn parse_keyvalues(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('"').collect();
        if let [_, key, _, value, ..] = parts.as_slice() {
            values.entry((*key).to_owned()).or_insert_with(|| (*value).to_owned());
        }
    }
    values
}

/// Every library path in `libraryfolders.vdf`, plus the Steam root itself.
fn library_paths(root: &Path, vdf: &str) -> Vec<PathBuf> {
    let mut paths = vec![root.to_path_buf()];
    for line in vdf.lines() {
        let parts: Vec<&str> = line.split('"').collect();
        if let [_, "path", _, value, ..] = parts.as_slice() {
            let path = PathBuf::from(value);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

/// The app's manifest values and when Steam last wrote the file.
fn manifest(root: &Path, app: u32) -> Option<(BTreeMap<String, String>, SystemTime)> {
    let vdf = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")).unwrap_or_default();
    library_paths(root, &vdf).iter().find_map(|library| {
        let path = library.join(format!("steamapps/appmanifest_{app}.acf"));
        let modified = std::fs::metadata(&path).and_then(|metadata| metadata.modified()).ok()?;
        Some((parse_keyvalues(&std::fs::read_to_string(&path).ok()?), modified))
    })
}

fn kib_ceil(bytes: u64) -> u32 {
    u32::try_from(bytes.div_ceil(1024)).unwrap_or(u32::MAX).max(1)
}

/// `(completed, total)` in KiB, or `None` if nothing says how big the update is. `fresh` says
/// Steam wrote the manifest after the current update started.
fn progress(manifest: Option<&BTreeMap<String, String>>, fresh: bool, log_total: u64) -> Option<(u32, u32)> {
    let field = |key: &str| manifest.and_then(|values| values.get(key)).and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
    let manifest_total = field("BytesToDownload") + field("BytesToStage");
    let (completed, total) = if fresh && manifest_total > 0 && (log_total == 0 || manifest_total == log_total) {
        (field("BytesDownloaded") + field("BytesStaged"), manifest_total)
    } else if log_total > 0 {
        (0, log_total)
    } else {
        return None;
    };
    let total_kib = kib_ceil(total);
    // Completed rounds down and the total up, so finished bytes must map to the total exactly,
    // not to one KiB short of it.
    let completed_kib = if completed >= total { total_kib } else { u32::try_from(completed / 1024).unwrap_or(u32::MAX).min(total_kib) };
    Some((completed_kib, total_kib))
}

fn steam_alive() -> bool {
    std::fs::read_dir("/proc").map_or(false, |entries| {
        entries.flatten().any(|entry| std::fs::read_to_string(entry.path().join("comm")).is_ok_and(|comm| comm.trim_end() == "steam"))
    })
}

/// Reads whole new lines appended to a file, starting over if it shrinks (Steam rotates the
/// log to `content_log.previous.txt` when it starts).
struct LogFollower {
    path: PathBuf,
    offset: u64,
    partial: String,
}

impl LogFollower {
    fn read_new(&mut self) -> Vec<String> {
        let Ok(mut file) = std::fs::File::open(&self.path) else { return Vec::new() };
        let length = file.metadata().map_or(0, |metadata| metadata.len());
        if length < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(self.offset)).is_err() || file.read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        self.offset += bytes.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&bytes));
        let Some(end) = self.partial.rfind('\n') else { return Vec::new() };
        let complete: Vec<String> = self.partial[..end].lines().map(str::to_owned).collect();
        self.partial.drain(..=end);
        complete
    }
}

// ---------------------------------------------------------------- the reporter loop

fn job_id(app: u32) -> String {
    format!("{JOB_PREFIX}{app}")
}

struct Reporter {
    root: PathBuf,
    dry_run: bool,
    tracker: Tracker,
    /// Endings the controller has not yet acknowledged; retried every poll.
    pending: Vec<Action>,
    last_error: Option<String>,
}

enum Sent {
    Ok,
    Refused(String),
    Unreachable,
}

impl Reporter {
    async fn send(&mut self, request: Value) -> Sent {
        if self.dry_run {
            println!("monolithd steam-reporter (dry run): {request}");
            return Sent::Ok;
        }
        match controller::call(&request).await {
            Ok(reply) if reply["ok"] == Value::Bool(true) => {
                self.last_error = None;
                Sent::Ok
            }
            Ok(reply) => Sent::Refused(reply["code"].as_str().unwrap_or("error").to_owned()),
            Err(error) => {
                if self.last_error.as_deref() != Some(error.as_str()) {
                    eprintln!("monolithd steam-reporter: {error}");
                    self.last_error = Some(error.clone());
                }
                Sent::Unreachable
            }
        }
    }

    /// Jobs a previous reporter left in the controller: keep those still running, end the rest.
    async fn adopt(&mut self) {
        if self.dry_run {
            return;
        }
        let Ok(status) = controller::call(&json!({ "op": "status" })).await else { return };
        let ids: Vec<String> = status["jobs"].as_array().into_iter().flatten().filter_map(|job| job["id"].as_str().map(str::to_owned)).collect();
        for id in ids {
            let Some(app) = id.strip_prefix(JOB_PREFIX).and_then(|app| app.parse::<u32>().ok()) else { continue };
            if self.tracker.apps.get(&app).is_some_and(|entry| entry.active(steam_alive())) {
                self.tracker.set_announced(app, true);
                eprintln!("monolithd steam-reporter: adopted running job {id}");
            } else {
                let _ = self.send(json!({ "op": "job.fail", "id": id, "reason": "update not running when the reporter started" })).await;
                eprintln!("monolithd steam-reporter: ended stale job {id}");
            }
        }
    }

    async fn report(&mut self, app: u32) {
        let entry = &self.tracker.apps[&app];
        let (announced, log_total, started_at) = (entry.announced, entry.log_total, entry.started_at);
        let found = manifest(&self.root, app);
        let fresh = found.as_ref().is_some_and(|(_, modified)| started_at.is_some_and(|started| *modified > started));
        let values = found.map(|(values, _)| values);
        let Some((completed, total)) = progress(values.as_ref(), fresh, log_total) else { return };
        let id = job_id(app);
        if !announced {
            let label = values.as_ref().and_then(|values| values.get("name").cloned()).unwrap_or_else(|| format!("Steam app {app}"));
            match self.send(json!({ "op": "job.start", "id": id, "label": label, "total": total })).await {
                Sent::Ok => {
                    self.tracker.set_announced(app, true);
                    eprintln!("monolithd steam-reporter: announced {id} ({label}, {total} KiB)");
                }
                // Most often "already completing": the previous update of this app is still in
                // its completion hold. Try again next poll.
                Sent::Refused(_) | Sent::Unreachable => return,
            }
        }
        if let Sent::Refused(code) = self.send(json!({ "op": "job.progress", "id": id, "completed": completed, "total": total })).await {
            if code == "unknown_job" {
                eprintln!("monolithd steam-reporter: controller forgot {id}; announcing it again");
                self.tracker.set_announced(app, false);
            }
        }
    }

    /// Returns false while the controller cannot be reached, so the ending is retried.
    async fn finish(&mut self, action: &Action) -> bool {
        let (request, what) = match action {
            Action::Complete { app } => (json!({ "op": "job.complete", "id": job_id(*app) }), "completed".to_owned()),
            Action::Fail { app, reason } => (json!({ "op": "job.fail", "id": job_id(*app), "reason": reason }), format!("ended: {reason}")),
            Action::Report { .. } => return true,
        };
        match self.send(request).await {
            Sent::Unreachable => false,
            Sent::Ok | Sent::Refused(_) => {
                if let Action::Complete { app } | Action::Fail { app, .. } = action {
                    eprintln!("monolithd steam-reporter: {} {what}", job_id(*app));
                }
                true
            }
        }
    }

    async fn poll(&mut self, follower: &mut LogFollower) {
        let now = Instant::now();
        for line in follower.read_new() {
            if let Some((app, event)) = parse_line(&line) {
                self.tracker.apply(app, event, now);
            }
        }
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend(self.tracker.plan(now, steam_alive()));
        for action in pending {
            match action {
                Action::Report { app } => self.report(app).await,
                ending => {
                    if !self.finish(&ending).await {
                        self.pending.push(ending);
                    }
                }
            }
        }
    }
}

const USAGE: &str = "usage: monolithd steam-reporter [--dry-run] [--steam-root PATH]";

/// `monolithd steam-reporter`: follow Steam's content log and report running updates as jobs.
pub async fn run(arguments: Vec<String>) -> Result<(), String> {
    let mut dry_run = false;
    let mut root = None;
    let mut words = arguments.into_iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--dry-run" => dry_run = true,
            "--steam-root" => root = Some(PathBuf::from(words.next().ok_or(USAGE)?)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let root = match root {
        Some(root) => root,
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join(".local/share/Steam"),
    };
    let mut follower = LogFollower { path: root.join("logs/content_log.txt"), offset: 0, partial: String::new() };
    let mut reporter = Reporter { root, dry_run, tracker: Tracker::default(), pending: Vec::new(), last_error: None };
    // Replay the current log so updates already running are known before adopting old jobs.
    let now = Instant::now();
    for line in follower.read_new() {
        if let Some((app, event)) = parse_line(&line) {
            reporter.tracker.apply(app, event, now);
        }
    }
    reporter.adopt().await;
    eprintln!("monolithd steam-reporter: following {}{}", follower.path.display(), if dry_run { " (dry run)" } else { "" });
    let mut tick = interval(POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        reporter.poll(&mut follower).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn line(app: u32, text: &str) -> String {
        format!("[2026-09-23 14:23:59] AppID {app} {text}")
    }

    fn feed(tracker: &mut Tracker, now: Instant, lines: &[String]) {
        for text in lines {
            if let Some((app, event)) = parse_line(text) {
                tracker.apply(app, event, now);
            }
        }
    }

    #[test]
    fn parses_real_log_lines() {
        assert_eq!(parse_line(&line(2225070, "state changed : Fully Installed,Update Queued,Update Running,")), Some((2225070, LogEvent::State { running: true, queued: true })));
        assert_eq!(parse_line(&line(2225070, "state changed : Fully Installed,")), Some((2225070, LogEvent::State { running: false, queued: false })));
        assert_eq!(
            parse_line(&line(292030, "update started : download 0/23158016, store 0/0, reuse 0/23389998, delta 0/0, stage 0/52980852 ")),
            Some((292030, LogEvent::Started { total: 23158016 + 52980852 }))
        );
        assert_eq!(parse_line(&line(292030, "Shader update changed : Running Update,Downloading,Staging,")), Some((292030, LogEvent::Shader { running: true })));
        assert_eq!(parse_line(&line(292030, "Shader update changed : None")), Some((292030, LogEvent::Shader { running: false })));
        assert_eq!(parse_line(&line(292030, "scheduler finished : removed from schedule (result No Error, state 0xc) ")), Some((292030, LogEvent::Finished { result: "No Error".into() })));
        assert_eq!(parse_line(&line(292030, "preallocated 1 files (50 MB) ")), None);
        assert_eq!(parse_line("[2026-09-23 14:24:39] Current download rate: 0.000 Mbps"), None);
    }

    #[test]
    fn a_long_update_is_announced_after_ten_seconds_then_completes_after_settling() {
        let (mut tracker, t0) = (Tracker::default(), Instant::now());
        feed(&mut tracker, t0, &[line(7, "state changed : Fully Installed,Update Queued,Update Running,"), line(7, "update started : download 0/4096, store 0/0, stage 0/4096")]);
        assert!(tracker.plan(t0, true).is_empty());
        assert!(tracker.plan(t0 + 9 * S, true).is_empty());
        assert_eq!(tracker.plan(t0 + 10 * S, true), vec![Action::Report { app: 7 }]);
        tracker.set_announced(7, true);
        // Steam ends an update through a momentary `Update Queued`; that is not a pause.
        let end = t0 + 60 * S;
        feed(&mut tracker, end, &[line(7, "state changed : Fully Installed,Update Queued,")]);
        assert!(tracker.plan(end + S, true).is_empty(), "held while the state settles");
        feed(&mut tracker, end + S, &[line(7, "state changed : Fully Installed,"), line(7, "scheduler finished : removed from schedule (result No Error, state 0xc)")]);
        assert!(tracker.plan(end + 2 * S, true).is_empty());
        assert_eq!(tracker.plan(end + 4 * S, true), vec![Action::Complete { app: 7 }]);
        assert!(tracker.plan(end + 10 * S, true).is_empty(), "ended exactly once");
    }

    #[test]
    fn a_short_update_never_becomes_a_job() {
        let (mut tracker, t0) = (Tracker::default(), Instant::now());
        feed(&mut tracker, t0, &[line(7, "state changed : Fully Installed,Update Queued,Update Running,")]);
        assert!(tracker.plan(t0 + 5 * S, true).is_empty());
        feed(&mut tracker, t0 + 6 * S, &[line(7, "state changed : Fully Installed,")]);
        assert!(tracker.plan(t0 + 20 * S, true).is_empty());
    }

    #[test]
    fn shader_updates_are_ignored() {
        let (mut tracker, t0) = (Tracker::default(), Instant::now());
        feed(&mut tracker, t0, &[line(292030, "state changed : Fully Installed,Update Queued,Update Running,"), line(292030, "Shader update changed : Running Update,Downloading,Staging,")]);
        assert!(tracker.plan(t0 + 30 * S, true).is_empty());
    }

    #[test]
    fn a_pause_fails_the_job_and_a_resume_announces_it_again() {
        let (mut tracker, t0) = (Tracker::default(), Instant::now());
        feed(&mut tracker, t0, &[line(7, "state changed : Fully Installed,Update Queued,Update Running,")]);
        assert!(tracker.plan(t0, true).is_empty());
        assert_eq!(tracker.plan(t0 + 10 * S, true), vec![Action::Report { app: 7 }]);
        tracker.set_announced(7, true);
        // A game launch suspends the update: `Update Queued` stays.
        feed(&mut tracker, t0 + 20 * S, &[line(7, "state changed : Fully Installed,Update Queued,App Running,"), line(7, "scheduler finished : staying in schedule (result Suspended, state 0x200c)")]);
        assert_eq!(tracker.plan(t0 + 24 * S, true), vec![Action::Fail { app: 7, reason: "paused by Steam".into() }]);
        let resumed = t0 + 100 * S;
        feed(&mut tracker, resumed, &[line(7, "state changed : Fully Installed,Update Queued,Update Running,")]);
        assert!(tracker.plan(resumed, true).is_empty());
        assert_eq!(tracker.plan(resumed + 10 * S, true), vec![Action::Report { app: 7 }]);
    }

    #[test]
    fn a_steam_error_fails_the_job() {
        let (mut tracker, t0) = (Tracker::default(), Instant::now());
        feed(&mut tracker, t0, &[line(7, "state changed : Fully Installed,Update Queued,Update Running,")]);
        tracker.plan(t0, true);
        tracker.set_announced(7, true);
        feed(&mut tracker, t0 + 30 * S, &[line(7, "state changed : Update Required,"), line(7, "scheduler finished : removed from schedule (result Disk Write Failure, state 0x6)")]);
        assert_eq!(tracker.plan(t0 + 40 * S, true), vec![Action::Fail { app: 7, reason: "Steam reported Disk Write Failure".into() }]);
    }

    #[test]
    fn steam_exiting_ends_every_job_at_once() {
        let (mut tracker, t0) = (Tracker::default(), Instant::now());
        feed(&mut tracker, t0, &[line(7, "state changed : Fully Installed,Update Queued,Update Running,")]);
        tracker.plan(t0, true);
        tracker.set_announced(7, true);
        assert_eq!(tracker.plan(t0 + S, false), vec![Action::Fail { app: 7, reason: "Steam is not running".into() }]);
    }

    #[test]
    fn a_forgotten_job_is_reported_again_every_poll() {
        // After `unknown_job` the reporter clears `announced`; an active update keeps producing
        // `Report`, and `Reporter::report` announces again when it sees `announced` is false.
        let (mut tracker, t0) = (Tracker::default(), Instant::now());
        feed(&mut tracker, t0, &[line(7, "state changed : Fully Installed,Update Queued,Update Running,")]);
        tracker.plan(t0, true);
        tracker.set_announced(7, true);
        tracker.set_announced(7, false);
        assert_eq!(tracker.plan(t0 + 12 * S, true), vec![Action::Report { app: 7 }]);
    }

    #[test]
    fn a_new_update_forgets_the_previous_totals_before_its_own_start_line() {
        let (mut tracker, t0) = (Tracker::default(), Instant::now());
        feed(&mut tracker, t0, &[line(7, "state changed : Fully Installed,Update Queued,Update Running,"), line(7, "update started : download 0/4096, store 0/0, stage 0/4096")]);
        feed(&mut tracker, t0, &[line(7, "state changed : Fully Installed,")]);
        let first = tracker.apps[&7].started_at.unwrap();
        assert_eq!(tracker.apps[&7].log_total, 8192);
        std::thread::sleep(Duration::from_millis(5));
        feed(&mut tracker, t0 + 60 * S, &[line(7, "state changed : Fully Installed,Update Queued,Update Running,")]);
        assert_eq!(tracker.apps[&7].log_total, 0, "no total until this update's own start line");
        assert!(tracker.apps[&7].started_at.unwrap() > first);
    }

    #[test]
    fn keyvalues_and_libraries() {
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"2225070\"\n\t\"name\"\t\t\"Trackmania\"\n\t\"BytesToDownload\"\t\t\"204656928\"\n\t\"InstalledDepots\"\n\t{\n\t\t\"name\"\t\t\"nested\"\n\t}\n}\n";
        let values = parse_keyvalues(acf);
        assert_eq!(values["name"], "Trackmania");
        assert_eq!(values["BytesToDownload"], "204656928");
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/var/home/u/.local/share/Steam\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"/run/media/u/disk/SteamLibrary\"\n\t}\n}\n";
        assert_eq!(
            library_paths(Path::new("/var/home/u/.local/share/Steam"), vdf),
            vec![PathBuf::from("/var/home/u/.local/share/Steam"), PathBuf::from("/run/media/u/disk/SteamLibrary")]
        );
    }

    #[test]
    fn progress_trusts_the_manifest_only_when_it_describes_the_current_update() {
        let manifest = |downloaded: u64, to_download: u64, staged: u64, to_stage: u64| {
            BTreeMap::from([
                ("BytesDownloaded".to_owned(), downloaded.to_string()),
                ("BytesToDownload".to_owned(), to_download.to_string()),
                ("BytesStaged".to_owned(), staged.to_string()),
                ("BytesToStage".to_owned(), to_stage.to_string()),
            ])
        };
        // Current update, half downloaded.
        assert_eq!(progress(Some(&manifest(10240, 20480, 0, 20480)), true, 40960), Some((10, 40)));
        // The manifest still shows the previous, finished update: report 0 of the log's total.
        assert_eq!(progress(Some(&manifest(20480, 20480, 20480, 20480)), true, 1024 * 1024), Some((0, 1024)));
        // Same totals as the log, but written before this attempt started (a retried update).
        assert_eq!(progress(Some(&manifest(20480, 20480, 20480, 20480)), false, 40960), Some((0, 40)));
        // Every byte done reads as the whole total, even when the total is not a KiB multiple.
        assert_eq!(progress(Some(&manifest(6776222544, 6776222544, 7947804199, 7947804199)), true, 6776222544 + 7947804199), Some((14378933, 14378933)));
        // No log total yet: the manifest is all there is.
        assert_eq!(progress(Some(&manifest(0, 1500, 0, 0)), true, 0), Some((0, 2)));
        assert_eq!(progress(None, false, 0), None);
    }
}
