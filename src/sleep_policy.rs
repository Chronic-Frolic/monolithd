//! Automatic suspend policy (phase 2: observe only; it never suspends and never changes LEDs).
//!
//! Owner decisions, 2026-09-25: White Monolith suspends after a tunable quiet period (default
//! two hours) whose clock restarts whenever any activity ends, so the machine can be used
//! several times without batching and changes state rarely (thermal cycling wears silicon).
//!
//! Activity, each restarting the clock when it ends: a controller job; keyboard and mouse
//! (KWin's input-idle notification); gamepads (evdev); local music (a playing audio stream
//! that does not belong to a running game, so an idle game on its menu music still sleeps);
//! a wake. Remote desktop (KRDP, TCP 3389) and Sunshine streaming sessions are informational
//! only (owner decision 2026-09-25): a connected but untouched session must not keep the
//! machine awake, and real remote use already arrives as input (RDP through KWin, Sunshine
//! through uinput). Hard
//! blocks: any `block`-mode sleep inhibitor other than the watchdog's own job block (the
//! manual suspend block is one). A source that cannot be read is unknown, and unknown counts
//! as busy: the machine stays awake rather than sleeping on a guess.
//!
//! The quiet clock uses `Instant`, which does not advance during suspend, so a missed wake
//! signal can never cause an immediate second suspend. The period comes from
//! `config/sleep.toml`, re-read every minute. The verdict is logged on change, summarised
//! every 15 minutes, and written to `$XDG_RUNTIME_DIR/monolith-events/sleep-policy.json`.

use crate::{controller, gamepad::Gamepads, paths, wayland_idle};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, MissedTickBehavior};

/// KWin reports idle after this long without input; the last input was that long before.
const INPUT_IDLE_MS: u32 = 60_000;
/// Jobs older than this are ignored, as the watchdog does, so a stuck one cannot pin the machine awake.
const JOB_CAP: Duration = Duration::from_secs(6 * 3600);
const SUMMARY_EVERY: Duration = Duration::from_secs(15 * 60);
const RDP_PORT: u16 = 3389;
const SUNSHINE_TCP: [u16; 3] = [47984, 47989, 48010];
const SUNSHINE_UDP: std::ops::RangeInclusive<u16> = 47998..=48010;
const WATCHDOG_WHO: &str = "Monolith-Event-Watchdog";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Jobs,
    Input,
    Gamepad,
    Audio,
    RemoteDesktop,
    Streaming,
}

impl Source {
    const ALL: [Source; 6] = [Source::Jobs, Source::Input, Source::Gamepad, Source::Audio, Source::RemoteDesktop, Source::Streaming];

    /// Whether this source can hold the machine awake or restart the quiet clock. Remote
    /// sessions are logged for evidence but count only through the input they produce.
    fn counts(self) -> bool {
        !matches!(self, Source::RemoteDesktop | Source::Streaming)
    }

    fn name(self) -> &'static str {
        match self {
            Source::Jobs => "jobs",
            Source::Input => "keyboard and mouse",
            Source::Gamepad => "gamepad",
            Source::Audio => "music",
            Source::RemoteDesktop => "remote desktop",
            Source::Streaming => "streaming",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Reading {
    Active(String),
    Idle,
    Unknown(String),
}

#[derive(Debug)]
struct Tracked {
    reading: Reading,
    last_active: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    /// Something is in use (or cannot be read): the clock is held.
    Busy(Vec<String>),
    /// Quiet, but a sleep inhibitor would refuse the suspend.
    Blocked(Vec<String>),
    /// Quiet: would suspend this long from now.
    Counting(Duration),
}

#[derive(Debug, Clone, PartialEq)]
struct Verdict {
    state: State,
    quiet_since: Instant,
    restarted_by: String,
}

/// The pure decision core: readings in, verdict out.
struct Policy {
    sources: BTreeMap<Source, Tracked>,
    /// Service start or the latest wake; the quiet clock never starts earlier.
    epoch: Instant,
    epoch_reason: String,
    blocks: Vec<String>,
}

impl Policy {
    fn new(now: Instant) -> Self {
        let sources = Source::ALL.into_iter().map(|source| (source, Tracked { reading: Reading::Unknown("not read yet".into()), last_active: None })).collect();
        Self { sources, epoch: now, epoch_reason: "service start".into(), blocks: Vec::new() }
    }

    fn wake(&mut self, now: Instant) {
        self.epoch = now;
        self.epoch_reason = "wake".into();
    }

    /// Record a reading; returns a log line when it changed.
    fn set(&mut self, source: Source, reading: Reading, now: Instant) -> Option<String> {
        let tracked = self.sources.get_mut(&source).expect("every source is tracked");
        if tracked.reading == reading {
            return None;
        }
        if matches!(tracked.reading, Reading::Active(_)) || matches!(reading, Reading::Active(_)) {
            tracked.last_active = Some(now);
        }
        let line = match &reading {
            Reading::Active(detail) => format!("{}: active ({detail})", source.name()),
            Reading::Idle => format!("{}: idle", source.name()),
            Reading::Unknown(reason) => format!("{}: unknown ({reason})", source.name()),
        };
        tracked.reading = reading;
        Some(line)
    }

    /// A momentary activity (a gamepad press) at `at`.
    fn activity(&mut self, source: Source, at: Instant) {
        let tracked = self.sources.get_mut(&source).expect("every source is tracked");
        tracked.last_active = Some(tracked.last_active.map_or(at, |last| last.max(at)));
    }

    fn verdict(&self, now: Instant, quiet: Duration) -> Verdict {
        let mut busy = Vec::new();
        let (mut quiet_since, mut restarted_by) = (self.epoch, self.epoch_reason.clone());
        for (source, tracked) in self.sources.iter().filter(|(source, _)| source.counts()) {
            let last = match &tracked.reading {
                Reading::Active(detail) => {
                    busy.push(format!("{} ({detail})", source.name()));
                    Some(now)
                }
                Reading::Unknown(reason) => {
                    busy.push(format!("{} unknown ({reason})", source.name()));
                    Some(now)
                }
                Reading::Idle => tracked.last_active,
            };
            if let Some(last) = last.filter(|last| *last > quiet_since) {
                quiet_since = last;
                restarted_by = source.name().to_owned();
            }
        }
        let state = if !busy.is_empty() {
            State::Busy(busy)
        } else if !self.blocks.is_empty() {
            State::Blocked(self.blocks.clone())
        } else {
            State::Counting((quiet_since + quiet).saturating_duration_since(now))
        };
        Verdict { state, quiet_since, restarted_by }
    }
}

// ---------------------------------------------------------------- reading the sources

/// Playing local streams that are not part of a running game.
fn audio_reading(pactl_json: &str, is_game: impl Fn(u32) -> bool) -> Result<Reading, String> {
    let streams: Value = serde_json::from_str(pactl_json).map_err(|error| format!("pactl output: {error}"))?;
    let playing: Vec<String> = streams
        .as_array()
        .ok_or("pactl output is not a list")?
        .iter()
        .filter(|stream| stream["corked"] == false && stream["mute"] == false)
        .filter(|stream| !stream["properties"]["application.process.id"].as_str().and_then(|pid| pid.parse().ok()).is_some_and(&is_game))
        .map(|stream| stream["properties"]["application.name"].as_str().unwrap_or("an app").to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(if playing.is_empty() { Reading::Idle } else { Reading::Active(playing.join(", ")) })
}

/// Is this `/proc/<pid>/cmdline` Steam's game launcher, `reaper SteamLaunch AppId=… -- …`?
/// Arguments are separated by NUL bytes there, not spaces.
fn is_steam_launch(cmdline: &[u8]) -> bool {
    let mut arguments = cmdline.split(|byte| *byte == 0);
    arguments.any(|argument| argument == b"SteamLaunch") && cmdline.split(|byte| *byte == 0).any(|argument| argument.starts_with(b"AppId="))
}

/// Does `pid` descend from a Steam game launcher (`reaper SteamLaunch AppId=…`)? Proton and
/// its wine processes do. Flatpak apps report PIDs from their own namespace; those resolve to
/// unrelated host processes (Firefox's `2` is kthreadd), which is simply "not a game".
fn is_game(pid: u32) -> bool {
    let mut current = pid;
    for _ in 0..64 {
        if current <= 1 {
            return false;
        }
        if std::fs::read(format!("/proc/{current}/cmdline")).is_ok_and(|cmdline| is_steam_launch(&cmdline)) {
            return true;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{current}/stat")) else { return false };
        let Some(parent) = stat.rsplit_once(") ").and_then(|(_, rest)| rest.split_whitespace().nth(1)).and_then(|parent| parent.parse().ok()) else { return false };
        current = parent;
    }
    false
}

/// Local TCP ports with established connections, from `/proc/net/tcp` and `tcp6`.
fn established_ports(table: &str) -> Vec<u16> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let port = u16::from_str_radix(fields.get(1)?.rsplit_once(':')?.1, 16).ok()?;
            (*fields.get(3)? == "01").then_some(port)
        })
        .collect()
}

/// Local UDP ports with a socket bound, from `/proc/net/udp` and `udp6`.
fn bound_udp_ports(table: &str) -> Vec<u16> {
    table.lines().skip(1).filter_map(|line| u16::from_str_radix(line.split_whitespace().nth(1)?.rsplit_once(':')?.1, 16).ok()).collect()
}

/// `block`-mode sleep inhibitors, except the watchdog's own job block.
fn blocks(list_inhibitors_json: &str) -> Result<Vec<String>, String> {
    let reply: Value = serde_json::from_str(list_inhibitors_json).map_err(|error| format!("inhibitor list: {error}"))?;
    let entries = reply["data"][0].as_array().ok_or("inhibitor list has no entries")?;
    Ok(entries
        .iter()
        .filter_map(|entry| {
            let (what, who, why, mode) = (entry[0].as_str()?, entry[1].as_str()?, entry[2].as_str()?, entry[3].as_str()?);
            (mode == "block" && what.split(':').any(|kind| kind == "sleep") && who != WATCHDOG_WHO).then(|| format!("{who}: {why}"))
        })
        .collect())
}

async fn output(program: &str, arguments: &[&str]) -> Result<String, String> {
    let result = Command::new(program).args(arguments).output().await.map_err(|error| format!("run {program}: {error}"))?;
    if !result.status.success() {
        return Err(format!("{program} failed: {}", String::from_utf8_lossy(&result.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&result.stdout).into_owned())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SleepConfig {
    version: u32,
    quiet_minutes: u64,
    cue_minutes: u64,
}

fn load_config() -> Result<SleepConfig, String> {
    let path = paths::config_dir().join("sleep.toml");
    let text = std::fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let config: SleepConfig = toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))?;
    if config.version != 1 {
        return Err(format!("{}: unsupported version {}", path.display(), config.version));
    }
    if config.quiet_minutes == 0 || config.cue_minutes >= config.quiet_minutes {
        return Err(format!("{}: need 0 < cue_minutes < quiet_minutes", path.display()));
    }
    Ok(config)
}

/// Monotonic time excludes suspend and boot time includes it; their gap grows across a suspend.
fn suspended_so_far() -> Duration {
    let read = |clock| {
        let mut spec = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime writes one timespec.
        unsafe { libc::clock_gettime(clock, &mut spec) };
        Duration::new(spec.tv_sec as u64, spec.tv_nsec as u32)
    };
    read(libc::CLOCK_BOOTTIME).saturating_sub(read(libc::CLOCK_MONOTONIC))
}

enum Update {
    Reading(Source, Reading),
    Activity(Source, Instant),
    Blocks(Vec<String>),
    Wake,
    Note(String),
}

async fn follow_input(updates: mpsc::Sender<Update>) {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/run/user/1000".into()));
    let mut last_problem = String::new();
    loop {
        let mut problem = "no Wayland compositor socket".to_owned();
        for socket in wayland_idle::sockets(&runtime) {
            match wayland_idle::IdleWatch::connect(&socket, INPUT_IDLE_MS).await {
                Ok(mut watch) => {
                    let name = socket.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    let _ = updates.send(Update::Note(format!("input idle: following {name} (ext_idle_notifier_v1 v{})", watch.version))).await;
                    // Until the compositor says otherwise, assume someone may be there.
                    let _ = updates.send(Update::Reading(Source::Input, Reading::Active("no idle report yet".into()))).await;
                    loop {
                        match watch.event().await {
                            Ok(wayland_idle::IdleEvent::Idled) => {
                                let _ = updates.send(Update::Reading(Source::Input, Reading::Idle)).await;
                                let at = Instant::now().checked_sub(Duration::from_millis(INPUT_IDLE_MS.into())).unwrap_or_else(Instant::now);
                                let _ = updates.send(Update::Activity(Source::Input, at)).await;
                            }
                            Ok(wayland_idle::IdleEvent::Resumed) => {
                                let _ = updates.send(Update::Reading(Source::Input, Reading::Active(name.clone()))).await;
                            }
                            Err(error) => {
                                problem = format!("{name}: {error}");
                                break;
                            }
                        }
                    }
                }
                Err(error) => problem = format!("{}: {error}", socket.file_name().unwrap_or_default().to_string_lossy()),
            }
        }
        let _ = updates.send(Update::Reading(Source::Input, Reading::Unknown(problem.clone()))).await;
        if problem != last_problem {
            let _ = updates.send(Update::Note(format!("input idle unavailable: {problem}"))).await;
            last_problem = problem;
        }
        sleep(Duration::from_secs(5)).await;
    }
}

async fn follow_gamepads(updates: mpsc::Sender<Update>) {
    let mut pads = Gamepads::default();
    let mut known: Vec<String> = Vec::new();
    let mut tick = interval(Duration::from_secs(1));
    let _ = updates.send(Update::Reading(Source::Gamepad, Reading::Idle)).await;
    for count in 0u64.. {
        tick.tick().await;
        if count % 10 == 0 {
            pads.rescan();
            let names = pads.names();
            if names != known {
                let _ = updates.send(Update::Note(format!("gamepads: {}", if names.is_empty() { "none".to_owned() } else { names.join(", ") }))).await;
                known = names;
            }
        }
        if pads.poll().is_some() {
            let _ = updates.send(Update::Activity(Source::Gamepad, Instant::now())).await;
        }
    }
}

async fn follow_polled(updates: mpsc::Sender<Update>) {
    let mut tick = interval(Duration::from_secs(10));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_suspended = suspended_so_far();
    let mut job_seen: BTreeMap<String, Instant> = BTreeMap::new();
    let mut last_udp = Vec::new();
    loop {
        tick.tick().await;
        let now = Instant::now();
        let suspended = suspended_so_far();
        if suspended > last_suspended + Duration::from_secs(3) {
            let _ = updates.send(Update::Wake).await;
        }
        last_suspended = suspended;

        let jobs = match controller::call(&json!({ "op": "status" })).await {
            Ok(status) => {
                let ids: Vec<String> = status["jobs"].as_array().into_iter().flatten().filter_map(|job| job["id"].as_str().map(str::to_owned)).collect();
                job_seen.retain(|id, _| ids.contains(id));
                for id in &ids {
                    job_seen.entry(id.clone()).or_insert(now);
                }
                let fresh: Vec<&String> = ids.iter().filter(|id| now.duration_since(job_seen[*id]) < JOB_CAP).collect();
                if fresh.is_empty() { Reading::Idle } else { Reading::Active(fresh.iter().map(|id| id.as_str()).collect::<Vec<_>>().join(", ")) }
            }
            Err(error) => Reading::Unknown(format!("controller: {error}")),
        };
        let _ = updates.send(Update::Reading(Source::Jobs, jobs)).await;

        let audio = match output("pactl", &["--format=json", "list", "sink-inputs"]).await {
            Ok(text) => audio_reading(&text, is_game).unwrap_or_else(Reading::Unknown),
            Err(error) => Reading::Unknown(error),
        };
        let _ = updates.send(Update::Reading(Source::Audio, audio)).await;

        let tcp: Vec<u16> = ["/proc/net/tcp", "/proc/net/tcp6"].iter().flat_map(|path| established_ports(&std::fs::read_to_string(path).unwrap_or_default())).collect();
        let count = |ports: &[u16]| tcp.iter().filter(|port| ports.contains(port)).count();
        let rdp = count(&[RDP_PORT]);
        let _ = updates.send(Update::Reading(Source::RemoteDesktop, if rdp > 0 { Reading::Active(format!("{rdp} connection(s)")) } else { Reading::Idle })).await;
        let sunshine = count(&SUNSHINE_TCP);
        let _ = updates.send(Update::Reading(Source::Streaming, if sunshine > 0 { Reading::Active(format!("{sunshine} Sunshine TCP connection(s)")) } else { Reading::Idle })).await;
        // Raw evidence for choosing the streaming detector after a real Moonlight session.
        let mut udp: Vec<u16> = ["/proc/net/udp", "/proc/net/udp6"].iter().flat_map(|path| bound_udp_ports(&std::fs::read_to_string(path).unwrap_or_default())).filter(|port| SUNSHINE_UDP.contains(port)).collect();
        udp.sort_unstable();
        udp.dedup();
        if udp != last_udp {
            let _ = updates.send(Update::Note(format!("Sunshine UDP ports bound: {udp:?}"))).await;
            last_udp = udp;
        }

        let blocked = match output("busctl", &["--json=short", "call", "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "ListInhibitors"]).await {
            Ok(text) => blocks(&text).unwrap_or_else(|error| vec![format!("inhibitors unreadable: {error}")]),
            Err(error) => vec![format!("inhibitors unreadable: {error}")],
        };
        let _ = updates.send(Update::Blocks(blocked)).await;
    }
}

/// Wall-clock seconds for an `Instant`, past or future, given the wall time at `now`.
fn unix_at_wall(instant: Instant, now: Instant, wall: u64) -> u64 {
    if instant >= now { wall + instant.duration_since(now).as_secs() } else { wall.saturating_sub(now.duration_since(instant).as_secs()) }
}

fn unix_at(instant: Instant, now: Instant) -> u64 {
    unix_at_wall(instant, now, SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs()))
}

fn clock(unix: u64) -> String {
    // SAFETY: an all-zero `tm` is a valid value for localtime_r to overwrite.
    let mut time: libc::tm = unsafe { std::mem::zeroed() };
    let seconds = unix as libc::time_t;
    // SAFETY: localtime_r fills `time` from `seconds`.
    unsafe { libc::localtime_r(&seconds, &mut time) };
    format!("{:02}:{:02}", time.tm_hour, time.tm_min)
}

fn describe(verdict: &Verdict, now: Instant) -> String {
    let since = clock(unix_at(verdict.quiet_since, now));
    match &verdict.state {
        State::Busy(busy) => format!("awake: busy with {}", busy.join("; ")),
        State::Blocked(blocks) => format!("quiet since {since} but sleep is blocked by {}", blocks.join("; ")),
        State::Counting(left) => format!("would suspend at {} (quiet since {since}, clock restarted by {})", clock(unix_at(now + *left, now)), verdict.restarted_by),
    }
}

fn status_file() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("monolith-events").join("sleep-policy.json"))
}

fn write_status(policy: &Policy, verdict: &Verdict, quiet: Duration, now: Instant) {
    let Some(path) = status_file() else { return };
    let (state, until) = match &verdict.state {
        State::Busy(reasons) => ("busy", json!(reasons)),
        State::Blocked(reasons) => ("blocked", json!(reasons)),
        State::Counting(left) => ("counting", json!(unix_at(now + *left, now))),
    };
    let sources: serde_json::Map<String, Value> = policy
        .sources
        .iter()
        .map(|(source, tracked)| {
            let reading = match &tracked.reading {
                Reading::Active(detail) => format!("active: {detail}"),
                Reading::Idle => "idle".into(),
                Reading::Unknown(reason) => format!("unknown: {reason}"),
            };
            (source.name().to_owned(), json!({ "reading": reading, "counts": source.counts(), "last_active_unix": tracked.last_active.map(|at| unix_at(at, now)) }))
        })
        .collect();
    let body = json!({
        "mode": "observe",
        "quiet_minutes": quiet.as_secs() / 60,
        "state": state,
        "detail": until,
        "summary": describe(verdict, now),
        "quiet_since_unix": unix_at(verdict.quiet_since, now),
        "restarted_by": verdict.restarted_by,
        "sources": sources,
        "updated_unix": unix_at(now, now),
    });
    let temporary = path.with_extension("json.tmp");
    if std::fs::write(&temporary, body.to_string()).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
}

/// `monolithd sleep-policy`: observe and report when White Monolith would suspend.
pub async fn run(arguments: Vec<String>) -> Result<(), String> {
    if !arguments.is_empty() {
        return Err("usage: monolithd sleep-policy".to_owned());
    }
    let mut config = load_config()?;
    eprintln!("monolithd sleep-policy: observe only; quiet period {} min; {}", config.quiet_minutes, paths::describe());
    let (sender, mut updates) = mpsc::channel(256);
    tokio::spawn(follow_input(sender.clone()));
    tokio::spawn(follow_gamepads(sender.clone()));
    tokio::spawn(follow_polled(sender));
    let mut policy = Policy::new(Instant::now());
    let mut evaluate = interval(Duration::from_secs(5));
    evaluate.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut reload = interval(Duration::from_secs(60));
    reload.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let (mut last_line, mut last_summary) = (String::new(), Instant::now());
    loop {
        tokio::select! {
            Some(update) = updates.recv() => {
                let now = Instant::now();
                match update {
                    Update::Reading(source, reading) => {
                        if let Some(line) = policy.set(source, reading, now) {
                            eprintln!("monolithd sleep-policy: {line}");
                        }
                    }
                    Update::Activity(source, at) => policy.activity(source, at),
                    Update::Blocks(blocks) => {
                        if blocks != policy.blocks {
                            eprintln!("monolithd sleep-policy: sleep blockers: {}", if blocks.is_empty() { "none".to_owned() } else { blocks.join("; ") });
                            policy.blocks = blocks;
                        }
                    }
                    Update::Wake => {
                        eprintln!("monolithd sleep-policy: woke from sleep; the quiet clock restarts");
                        policy.wake(now);
                    }
                    Update::Note(note) => eprintln!("monolithd sleep-policy: {note}"),
                }
            }
            _ = reload.tick() => match load_config() {
                Ok(fresh) => {
                    if fresh.quiet_minutes != config.quiet_minutes {
                        eprintln!("monolithd sleep-policy: quiet period now {} min", fresh.quiet_minutes);
                    }
                    config = fresh;
                }
                Err(error) => eprintln!("monolithd sleep-policy: keeping the previous settings: {error}"),
            },
            _ = evaluate.tick() => {
                let now = Instant::now();
                let quiet = Duration::from_secs(config.quiet_minutes * 60);
                let verdict = policy.verdict(now, quiet);
                let line = describe(&verdict, now);
                // Log when the meaning changes, not every time the countdown ticks.
                let key = match &verdict.state { State::Counting(_) => format!("counting since {:?}", verdict.quiet_since), _ => line.clone() };
                if key != last_line || now.duration_since(last_summary) >= SUMMARY_EVERY {
                    eprintln!("monolithd sleep-policy: {line}");
                    last_line = key;
                    last_summary = now;
                }
                write_status(&policy, &verdict, quiet, now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);
    const HOUR: Duration = Duration::from_secs(3600);

    fn quiet_policy(start: Instant) -> Policy {
        let mut policy = Policy::new(start);
        for source in Source::ALL {
            policy.set(source, Reading::Idle, start);
        }
        policy
    }

    #[test]
    fn unknown_sources_hold_the_machine_awake() {
        let start = Instant::now();
        let policy = Policy::new(start);
        assert!(matches!(policy.verdict(start + 3 * HOUR, 2 * HOUR).state, State::Busy(reasons) if reasons.len() == 4), "remote sessions never hold it");
    }

    #[test]
    fn a_connected_but_untouched_remote_session_does_not_keep_it_awake() {
        let start = Instant::now();
        let mut policy = quiet_policy(start);
        policy.set(Source::RemoteDesktop, Reading::Active("1 connection(s)".into()), start + 10 * S);
        policy.set(Source::Streaming, Reading::Unknown("no data".into()), start + 10 * S);
        let verdict = policy.verdict(start + HOUR, 2 * HOUR);
        assert_eq!(verdict.state, State::Counting(HOUR));
        assert_eq!(verdict.restarted_by, "service start");
    }

    #[test]
    fn the_clock_restarts_when_the_last_activity_ends() {
        let start = Instant::now();
        let mut policy = quiet_policy(start);
        assert_eq!(policy.verdict(start, 2 * HOUR).state, State::Counting(2 * HOUR));
        policy.set(Source::Jobs, Reading::Active("steam:292030".into()), start + 10 * S);
        assert!(matches!(policy.verdict(start + 20 * S, 2 * HOUR).state, State::Busy(_)));
        policy.set(Source::Jobs, Reading::Idle, start + 30 * S);
        policy.set(Source::Audio, Reading::Active("Firefox".into()), start + 40 * S);
        policy.set(Source::Audio, Reading::Idle, start + 100 * S);
        let verdict = policy.verdict(start + 100 * S, 2 * HOUR);
        assert_eq!(verdict.state, State::Counting(2 * HOUR));
        assert_eq!((verdict.quiet_since, verdict.restarted_by.as_str()), (start + 100 * S, "music"));
        assert_eq!(policy.verdict(start + HOUR, 2 * HOUR).state, State::Counting(HOUR + 100 * S));
    }

    #[test]
    fn momentary_activity_restarts_the_clock_without_holding_it() {
        let start = Instant::now();
        let mut policy = quiet_policy(start);
        policy.activity(Source::Gamepad, start + HOUR);
        let verdict = policy.verdict(start + HOUR + 10 * S, 2 * HOUR);
        assert_eq!(verdict.state, State::Counting(2 * HOUR - 10 * S));
        assert_eq!(verdict.restarted_by, "gamepad");
        policy.activity(Source::Gamepad, start + 10 * S);
        assert_eq!(policy.verdict(start + HOUR + 10 * S, 2 * HOUR).restarted_by, "gamepad", "an older event never moves the clock back");
    }

    #[test]
    fn a_wake_restarts_the_clock() {
        let start = Instant::now();
        let mut policy = quiet_policy(start);
        policy.wake(start + 5 * HOUR);
        let verdict = policy.verdict(start + 5 * HOUR, 2 * HOUR);
        assert_eq!((verdict.state, verdict.restarted_by.as_str()), (State::Counting(2 * HOUR), "wake"));
    }

    #[test]
    fn blocks_hold_sleep_but_not_the_clock() {
        let start = Instant::now();
        let mut policy = quiet_policy(start);
        policy.blocks = vec!["Monolith-Event-Controller: Manual remote suspend block".into()];
        assert!(matches!(policy.verdict(start + 3 * HOUR, 2 * HOUR).state, State::Blocked(_)));
        policy.blocks.clear();
        assert_eq!(policy.verdict(start + 3 * HOUR, 2 * HOUR).state, State::Counting(Duration::ZERO));
    }

    #[test]
    fn music_counts_but_a_game_on_its_menu_does_not() {
        let pactl = r#"[
          {"index":3669,"corked":false,"mute":false,"properties":{"application.name":"TrackmaniaUplay","application.process.id":"2809718"}},
          {"index":3674,"corked":true,"mute":false,"properties":{"application.name":"TrackmaniaUplay","application.process.id":"2809718"}},
          {"index":3689,"corked":false,"mute":false,"properties":{"application.name":"Firefox","application.process.id":"2"}},
          {"index":3700,"corked":false,"mute":true,"properties":{"application.name":"Muted","application.process.id":"77"}}
        ]"#;
        let game = |pid: u32| pid == 2809718;
        assert_eq!(audio_reading(pactl, game).unwrap(), Reading::Active("Firefox".into()));
        let only_game = r#"[{"corked":false,"mute":false,"properties":{"application.name":"TrackmaniaUplay","application.process.id":"2809718"}}]"#;
        assert_eq!(audio_reading(only_game, game).unwrap(), Reading::Idle, "an idle game's menu music is not music");
        assert_eq!(audio_reading("[]", game).unwrap(), Reading::Idle);
        assert!(audio_reading("not json", game).is_err());
    }

    #[test]
    fn recognises_the_steam_launcher_by_its_nul_separated_arguments() {
        let launcher = b"/var/home/u/.local/share/Steam/ubuntu12_32/reaper\0SteamLaunch\0AppId=2225070\0--\0/path/to/proton\0";
        assert!(is_steam_launch(launcher));
        assert!(!is_steam_launch(b"/usr/bin/firefox\0--new-window\0"));
        assert!(!is_steam_launch(b"grep\0SteamLaunch AppId=\0"), "one argument containing the words is not the launcher");
    }

    #[test]
    fn wall_times_work_both_ways() {
        let now = Instant::now();
        assert_eq!(unix_at_wall(now + 2 * HOUR, now, 1_000_000), 1_007_200, "a future suspend time");
        assert_eq!(unix_at_wall(now - 60 * S, now, 1_000_000), 999_940, "a past quiet-since");
    }

    #[test]
    fn reads_established_tcp_and_bound_udp_ports() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000:0D3D 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000 0 1 1\n   1: 0A00000F:0D3D 0A000021:C350 01 00000000:00000000 00:00000000 00000000  1000 0 2 1\n   2: 00000000:BB80 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000 0 3 1\n";
        assert_eq!(established_ports(tcp), vec![3389], "only the established 3389 connection, not the listeners");
        let udp = "  sl  local_address rem_address   st\n  10: 00000000:BB7E 00000000:0000 07 00000000:00000000\n";
        assert_eq!(bound_udp_ports(udp), vec![47998]);
    }

    #[test]
    fn only_other_programs_sleep_blocks_count() {
        let json = r#"{"type":"a(ssssuu)","data":[[["sleep","Monolith-Event-Watchdog","RGB suspend handoff","delay",1000,1],["sleep","Monolith-Event-Watchdog","jobs are running","block",1000,1],["handle-power-key:handle-suspend-key","PowerDevil","KDE handles power events","block",1000,2],["sleep","Monolith-Event-Controller","Manual remote suspend block","block",1000,3],["shutdown:sleep","Some App","Burning a disc","block",1000,4]]]}"#;
        assert_eq!(blocks(json).unwrap(), vec!["Monolith-Event-Controller: Manual remote suspend block".to_owned(), "Some App: Burning a disc".to_owned()]);
    }

    #[test]
    fn the_shipped_sleep_config_is_valid() {
        let config = load_config().unwrap();
        assert_eq!((config.version, config.quiet_minutes, config.cue_minutes), (1, 120, 5));
    }
}
