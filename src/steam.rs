//! Steam download reporter: turns Steam's own download state into truthful controller jobs.
//!
//! Source (owner decision 2026-09-25, no fallback): Steam's UI JavaScript API in its CEF page
//! `SharedJSContext`, reached through the DevTools port Decky Loader enables on 127.0.0.1:8080.
//! `SteamClient.Downloads.RegisterForDownloadOverview` reports the one app Steam is downloading
//! about once a second, with `progress[]` per phase (measured 2026-09-25: slot 1 preallocate,
//! 2 download, 3 stage, 6 install); `RegisterForDownloadItems` reports, on state changes, each
//! app's `completed`, `completed_time`, `update_result` and `update_error`. The subscription
//! script picks only those fields, filters to the local client, and lists any it can no longer
//! find, which is how an API change is detected.
//!
//! Jobs: only the app Steam is actively downloading (not a shader-cache update, not paused)
//! becomes job `steam:<appid>`, after running for 10 s; its bar is download bytes in KiB, full
//! during install. It completes when Steam marks the app completed after the job began, and
//! fails when Steam reports an error, or when the app stops downloading for 15 s (paused,
//! queued behind another app). Bars show in Gaming Mode too.
//!
//! Nothing here is silent (owner decision 2026-09-25):
//! - Steam running but its API unreachable for 60 s: warning `steam-reporter:blind`, bars released.
//! - Reachable but the API no longer looks as expected for 60 s: warning `steam-reporter:shape`.
//! - Steam reports a download error: the job fails and warning `steam-error:<appid>:<expiry>`
//!   names the game for 30 minutes.
//! - The reporter itself keeps failing: systemd's OnFailure raises fault `steam-reporter:down`.
//!
//! Warnings are re-asserted every 30 s because the controller forgets faults when it restarts,
//! and a reporter restart clears its own `steam-reporter:` faults, restores unexpired error
//! warnings from their IDs, and adopts or ends the `steam:` jobs it left behind.

use crate::reporter::{Ending, Executor};
use crate::ws::{self, WebSocket};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};

const DEVTOOLS: &str = "127.0.0.1:8080";
/// Distinct from the one-off recorder's names, so neither can unsubscribe the other.
const BINDING: &str = "monolithdSteamReport";
const POLL: Duration = Duration::from_secs(2);
/// A download must run this long before it becomes a job (owner policy 2a).
const START_AFTER: Duration = Duration::from_secs(10);
/// How long an app may stop downloading (a 6 s suspension was measured) before its job ends.
const PAUSE_GRACE: Duration = Duration::from_secs(15);
/// Rides out Steam's restart on every Desktop/Gaming switch before calling the reporter blind.
const BLIND_AFTER: Duration = Duration::from_secs(60);
const HEARTBEAT: Duration = Duration::from_secs(10);
const REASSERT: Duration = Duration::from_secs(30);
/// How long a download-error warning stays up (owner decision 2026-09-25).
const ERROR_WARNING_SECONDS: u64 = 30 * 60;
const JOB_PREFIX: &str = "steam:";
const OWN_FAULT_PREFIX: &str = "steam-reporter:";
const BLIND_ID: &str = "steam-reporter:blind";
const SHAPE_ID: &str = "steam-reporter:shape";
const ERROR_PREFIX: &str = "steam-error:";

// ---------------------------------------------------------------- what Steam reports

#[derive(Debug, Clone, PartialEq, Default)]
struct Overview {
    app: u32,
    state: String,
    shader: bool,
    paused: bool,
    /// Download phase bytes: done, total.
    download: (u64, u64),
    percent: u64,
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Item {
    app: u32,
    active: bool,
    completed: bool,
    completed_time: u64,
    result: i64,
    error: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Observation {
    Overview(Overview),
    Items(Vec<Item>),
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a Value, String> {
    value.get(name).ok_or_else(|| format!("missing {name}"))
}

fn number(value: &Value, name: &str) -> Result<u64, String> {
    field(value, name)?.as_u64().ok_or_else(|| format!("{name} is not a non-negative integer"))
}

fn flag(value: &Value, name: &str) -> Result<bool, String> {
    field(value, name)?.as_bool().ok_or_else(|| format!("{name} is not a boolean"))
}

/// One payload from the subscription script: `{"kind", "value", "missing"}`.
fn parse_event(text: &str) -> Result<Observation, String> {
    let event: Value = serde_json::from_str(text).map_err(|error| format!("unreadable event: {error}"))?;
    let kind = field(&event, "kind")?.as_str().unwrap_or("").to_owned();
    let missing: Vec<String> = event.get("missing").and_then(Value::as_array).into_iter().flatten().filter_map(|name| name.as_str().map(str::to_owned)).collect();
    if !missing.is_empty() {
        return Err(format!("Steam's download {kind} no longer has {}", missing.join(", ")));
    }
    let value = field(&event, "value")?;
    let shaped = |error: String| format!("Steam's download {kind}: {error}");
    match kind.as_str() {
        "overview" => {
            let progress = field(value, "progress").and_then(|p| p.as_array().ok_or_else(|| "progress is not a list".to_owned())).map_err(shaped)?;
            let download = progress.get(2).and_then(Value::as_array).filter(|pair| pair.len() == 2).ok_or_else(|| shaped(format!("progress has {} phases, expected the download in phase 2", progress.len())))?;
            let bytes = |index: usize| download[index].as_u64().ok_or_else(|| shaped("download bytes are not integers".to_owned()));
            Ok(Observation::Overview(Overview {
                app: u32::try_from(number(value, "update_appid").map_err(shaped)?).map_err(|_| shaped("update_appid out of range".to_owned()))?,
                state: field(value, "update_state").map_err(shaped)?.as_str().unwrap_or("").to_owned(),
                shader: flag(value, "update_is_shader").map_err(shaped)?,
                paused: flag(value, "paused").map_err(shaped)?,
                download: (bytes(0)?, bytes(1)?),
                percent: number(value, "overall_percent_complete").map_err(shaped)?,
                name: value.get("name").and_then(Value::as_str).map(str::to_owned),
            }))
        }
        "items" => value
            .as_array()
            .ok_or_else(|| shaped("items is not a list".to_owned()))?
            .iter()
            .map(|item| {
                Ok(Item {
                    app: u32::try_from(number(item, "appid")?).map_err(|_| "appid out of range".to_owned())?,
                    active: flag(item, "active")?,
                    completed: flag(item, "completed")?,
                    completed_time: number(item, "completed_time")?,
                    result: field(item, "update_result")?.as_i64().ok_or("update_result is not an integer")?,
                    error: field(item, "update_error")?.as_str().unwrap_or("").to_owned(),
                })
            })
            .collect::<Result<Vec<_>, String>>()
            .map(Observation::Items)
            .map_err(shaped),
        other => Err(format!("unknown event kind {other:?}")),
    }
}

fn kib_ceil(bytes: u64) -> u32 {
    u32::try_from(bytes.div_ceil(1024)).unwrap_or(u32::MAX).max(1)
}

/// `(completed, total)` for the bar: download KiB, full once Steam installs; Steam's own
/// percent when an update downloads nothing; `None` while nothing says how big it is.
fn progress_of(overview: &Overview) -> Option<(u32, u32)> {
    let (done, total) = overview.download;
    if total > 0 {
        let total_kib = kib_ceil(total);
        if matches!(overview.state.as_str(), "Installing" | "Finalizing") || done >= total {
            return Some((total_kib, total_kib));
        }
        return Some((u32::try_from(done / 1024).unwrap_or(u32::MAX).min(total_kib), total_kib));
    }
    if overview.state == "Starting" || overview.state.is_empty() {
        return None;
    }
    Some((overview.percent.min(100) as u32, 100))
}

// ---------------------------------------------------------------- the decision core

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Start { app: u32, label: String, total: u32 },
    Progress { app: u32, completed: u32, total: u32 },
    Complete { app: u32 },
    Fail { app: u32, reason: String },
    Warn { id: String, reason: String },
    Clear { id: String },
}

#[derive(Debug)]
struct Download {
    since: Instant,
    /// Wall-clock second the job began; Steam's `completed_time` must not be older.
    started_unix: u64,
    announced: bool,
    last: Option<(u32, u32)>,
    inactive_since: Option<Instant>,
    label: String,
}

#[derive(Default)]
struct Core {
    overview: Option<Overview>,
    items: BTreeMap<u32, Item>,
    downloads: BTreeMap<u32, Download>,
    /// Download-error warnings: app -> (expiry, reason).
    errors: BTreeMap<u32, (u64, String)>,
    connected: bool,
    blind_since: Option<Instant>,
    /// Why Steam's API does not look as expected, and since when.
    shape: Option<(String, Instant)>,
    /// Warnings currently raised, id -> reason.
    raised: BTreeMap<String, String>,
}

fn error_id(app: u32, expiry: u64) -> String {
    format!("{ERROR_PREFIX}{app}:{expiry}")
}

impl Core {
    fn observe(&mut self, observation: Observation) {
        match observation {
            Observation::Overview(overview) => self.overview = Some(overview),
            Observation::Items(items) => self.items = items.into_iter().map(|item| (item.app, item)).collect(),
        }
    }

    fn set_shape(&mut self, problem: Option<String>, now: Instant) {
        match problem {
            None => self.shape = None,
            Some(reason) => {
                let since = self.shape.as_ref().map_or(now, |(_, since)| *since);
                self.shape = Some((reason, since));
            }
        }
    }

    fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    fn set_announced(&mut self, app: u32, announced: bool) {
        if let Some(download) = self.downloads.get_mut(&app) {
            download.announced = announced;
        }
    }

    /// A `steam:` job a previous reporter left in the controller. It continues if Steam is
    /// still downloading that app; otherwise the pause grace ends it.
    fn adopt(&mut self, app: u32, now: Instant, now_unix: u64) {
        let since = now.checked_sub(START_AFTER).unwrap_or(now);
        self.downloads.insert(app, Download { since, started_unix: now_unix.saturating_sub(3600), announced: true, last: None, inactive_since: None, label: format!("Steam app {app}") });
    }

    fn release_all(&mut self, reason: &str, actions: &mut Vec<Action>) {
        for (app, download) in std::mem::take(&mut self.downloads) {
            if download.announced {
                actions.push(Action::Fail { app, reason: reason.to_owned() });
            }
        }
    }

    fn tick(&mut self, now: Instant, now_unix: u64, steam_alive: bool) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut want: BTreeMap<String, String> = BTreeMap::new();
        if !steam_alive {
            self.blind_since = None;
            self.release_all("Steam is not running", &mut actions);
        } else if !self.connected {
            let since = *self.blind_since.get_or_insert(now);
            if now.duration_since(since) >= BLIND_AFTER {
                want.insert(BLIND_ID.to_owned(), "Steam is running but its download API on 127.0.0.1:8080 cannot be reached".to_owned());
                self.release_all("the reporter cannot see Steam's downloads", &mut actions);
            }
        } else {
            self.blind_since = None;
        }
        let shape = self.shape.clone().filter(|_| steam_alive);
        if let Some((reason, since)) = &shape {
            if now.duration_since(*since) >= BLIND_AFTER {
                want.insert(SHAPE_ID.to_owned(), reason.clone());
                self.release_all("Steam's download API no longer looks as expected", &mut actions);
            }
        }
        let shape_blocking = shape.is_some();
        if steam_alive && self.connected && !shape_blocking {
            self.track(now, now_unix, &mut actions);
        }
        self.errors.retain(|_, (expiry, _)| *expiry > now_unix);
        for (app, (expiry, reason)) in &self.errors {
            want.insert(error_id(*app, *expiry), reason.clone());
        }
        for id in self.raised.keys() {
            if !want.contains_key(id) {
                actions.push(Action::Clear { id: id.clone() });
            }
        }
        for (id, reason) in &want {
            if self.raised.get(id) != Some(reason) {
                actions.push(Action::Warn { id: id.clone(), reason: reason.clone() });
            }
        }
        self.raised = want;
        actions
    }

    fn track(&mut self, now: Instant, now_unix: u64, actions: &mut Vec<Action>) {
        let active = self.overview.as_ref().filter(|overview| overview.app != 0 && !overview.shader && !overview.paused).cloned();
        if let Some(overview) = &active {
            let download = self.downloads.entry(overview.app).or_insert_with(|| Download {
                since: now,
                started_unix: now_unix,
                announced: false,
                last: None,
                inactive_since: None,
                label: format!("Steam app {}", overview.app),
            });
            download.inactive_since = None;
            if let Some(name) = overview.name.as_ref().filter(|name| !name.is_empty()) {
                download.label = name.clone();
            }
            if let Some(progress) = progress_of(overview) {
                download.last = Some(progress);
            }
        }
        let active_app = active.map(|overview| overview.app);
        let mut ended = Vec::new();
        for (&app, download) in &mut self.downloads {
            if Some(app) != active_app {
                download.inactive_since.get_or_insert(now);
            }
            if let Some(item) = self.items.get(&app).filter(|item| !item.active) {
                if item.result != 0 || !item.error.is_empty() {
                    let detail = if item.error.is_empty() { format!("update result {}", item.result) } else { item.error.clone() };
                    if download.announced {
                        actions.push(Action::Fail { app, reason: format!("Steam reported an error: {detail}") });
                    }
                    self.errors.insert(app, (now_unix + ERROR_WARNING_SECONDS, format!("Steam could not download {}: {detail}", download.label)));
                    ended.push(app);
                    continue;
                }
                if item.completed && item.completed_time + 5 >= download.started_unix {
                    if download.announced {
                        actions.push(Action::Complete { app });
                    }
                    ended.push(app);
                    continue;
                }
            }
            if let Some(since) = download.inactive_since {
                if now.duration_since(since) >= PAUSE_GRACE {
                    if download.announced {
                        actions.push(Action::Fail { app, reason: "Steam stopped downloading it (paused, queued or cancelled)".to_owned() });
                    }
                    ended.push(app);
                }
                continue;
            }
            match (download.announced, download.last) {
                (false, Some((completed, total))) if now.duration_since(download.since) >= START_AFTER => {
                    actions.push(Action::Start { app, label: download.label.clone(), total });
                    actions.push(Action::Progress { app, completed, total });
                    download.announced = true;
                }
                (true, Some((completed, total))) => actions.push(Action::Progress { app, completed, total }),
                _ => {}
            }
        }
        for app in ended {
            self.downloads.remove(&app);
        }
    }
}

// ---------------------------------------------------------------- the link to Steam

/// Subscribes to both download callbacks. It first drops any earlier subscription of its
/// own, never throws into Steam, and unsubscribes itself once the binding is gone.
const SUBSCRIBE: &str = r#"(() => {
  const KEY = "__monolithdSteam";
  const unsub = () => { try { (window[KEY] || []).forEach(r => r && r.unregister()); } catch (e) {} window[KEY] = []; };
  unsub();
  const pick = (source, names) => {
    const value = {}, missing = [];
    for (const name of names) { if (source && name in source) value[name] = source[name]; else missing.push(name); }
    return [value, missing];
  };
  const emit = (kind, value, missing) => {
    try {
      if (typeof window.monolithdSteamReport !== "function") { unsub(); return; }
      window.monolithdSteamReport(JSON.stringify({ kind, value, missing }));
    } catch (e) { unsub(); }
  };
  const local = source => !source || !("remote_client_id" in source) || String(source.remote_client_id) === "0";
  const d = SteamClient.Downloads;
  window[KEY] = [
    d.RegisterForDownloadOverview(o => {
      if (!local(o)) return;
      const [value, missing] = pick(o, ["remote_client_id", "update_appid", "update_state", "update_is_shader", "paused", "progress", "overall_percent_complete"]);
      if (Array.isArray(value.progress)) value.progress = value.progress.map(p => [p && p.bytes_in_progress, p && p.bytes_total]);
      try { value.name = window.appStore.GetAppOverviewByAppID(o.update_appid).display_name; } catch (e) {}
      emit("overview", value, missing);
    }),
    d.RegisterForDownloadItems((isDownloading, groups) => {
      const items = [], missing = new Set();
      for (const group of groups || []) {
        if (!("remote_client_id" in group)) missing.add("remote_client_id");
        if (!local(group)) continue;
        for (const item of group.item_data || []) {
          const [value, gone] = pick(item, ["appid", "active", "paused", "completed", "completed_time", "update_result", "update_error"]);
          gone.forEach(name => missing.add(name));
          items.push(value);
        }
      }
      emit("items", items, [...missing]);
    }),
  ];
  return "subscribed";
})()"#;

/// Is our subscription still in place? False after SharedJSContext reloaded under us.
const CHECK: &str = r#"(() => { const r = window.__monolithdSteam; return Array.isArray(r) && r.length === 2 && typeof window.monolithdSteamReport === "function"; })()"#;

enum Link {
    Up,
    Down(String),
    Data(Result<Observation, String>),
    Shape(Option<String>),
}

async fn find_page() -> Result<(String, String), String> {
    let body = timeout(Duration::from_secs(3), ws::http_get(DEVTOOLS, "/json")).await.map_err(|_| "DevTools did not answer".to_owned())??;
    let targets: Value = serde_json::from_str(&body).map_err(|error| format!("DevTools target list: {error}"))?;
    let url = targets
        .as_array()
        .into_iter()
        .flatten()
        .find(|target| target["title"] == "SharedJSContext")
        .and_then(|target| target["webSocketDebuggerUrl"].as_str())
        .ok_or("Steam's SharedJSContext page is not open")?;
    let rest = url.strip_prefix("ws://").ok_or_else(|| format!("unexpected DevTools URL {url}"))?;
    let (host, path) = rest.split_once('/').ok_or_else(|| format!("unexpected DevTools URL {url}"))?;
    Ok((host.to_owned(), format!("/{path}")))
}

fn evaluate(id: u64, expression: &str) -> String {
    json!({ "id": id, "method": "Runtime.evaluate", "params": { "expression": expression, "returnByValue": true } }).to_string()
}

/// Add the binding and run the subscription script; returns the script's request id.
async fn subscribe(socket: &mut WebSocket, next: &mut u64) -> Result<u64, String> {
    let binding = json!({ "id": *next, "method": "Runtime.addBinding", "params": { "name": BINDING } }).to_string();
    let id = *next + 1;
    *next += 2;
    socket.send_text(&binding).await?;
    socket.send_text(&evaluate(id, SUBSCRIBE)).await?;
    Ok(id)
}

/// One DevTools session: subscribe, forward every report, and check the subscription every
/// 10 s. Returns when the connection fails; the caller reconnects.
async fn session(links: &mpsc::Sender<Link>) -> Result<(), String> {
    let (host, path) = find_page().await?;
    let mut socket = timeout(Duration::from_secs(3), WebSocket::connect(&host, &path)).await.map_err(|_| "DevTools WebSocket did not answer".to_owned())??;
    let mut next = 1u64;
    let mut subscribing = Some(subscribe(&mut socket, &mut next).await?);
    let mut checking: Option<(u64, Instant)> = None;
    let _ = links.send(Link::Up).await;
    let mut heartbeat = interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    heartbeat.tick().await;
    loop {
        tokio::select! {
            message = socket.read_text() => {
                let message: Value = serde_json::from_str(&message?).map_err(|error| format!("DevTools sent invalid JSON: {error}"))?;
                if message["method"] == "Runtime.bindingCalled" && message["params"]["name"] == BINDING {
                    let payload = message["params"]["payload"].as_str().unwrap_or("");
                    let _ = links.send(Link::Data(parse_event(payload))).await;
                    continue;
                }
                let Some(id) = message["id"].as_u64() else { continue };
                let value = &message["result"]["result"]["value"];
                if subscribing == Some(id) {
                    subscribing = None;
                    let problem = if value.as_str() == Some("subscribed") {
                        None
                    } else {
                        let detail = message["result"]["exceptionDetails"]["exception"]["description"].as_str().or(message["error"]["message"].as_str()).unwrap_or("no result");
                        Some(format!("subscribing to Steam's download API failed: {}", detail.lines().next().unwrap_or(detail)))
                    };
                    let _ = links.send(Link::Shape(problem)).await;
                } else if checking.is_some_and(|(pending, _)| pending == id) {
                    checking = None;
                    if value.as_bool() != Some(true) && subscribing.is_none() {
                        subscribing = Some(subscribe(&mut socket, &mut next).await?);
                    }
                }
            }
            _ = heartbeat.tick() => {
                if checking.is_some_and(|(_, sent)| sent.elapsed() >= HEARTBEAT) {
                    return Err("DevTools stopped answering".to_owned());
                }
                if checking.is_none() {
                    socket.send_text(&evaluate(next, CHECK)).await?;
                    checking = Some((next, Instant::now()));
                    next += 1;
                }
            }
        }
    }
}

async fn follow(links: mpsc::Sender<Link>) {
    loop {
        let reason = match session(&links).await {
            Ok(()) => "session ended".to_owned(),
            Err(error) => error,
        };
        if links.send(Link::Down(reason)).await.is_err() {
            return;
        }
        sleep(POLL).await;
    }
}

fn steam_alive() -> bool {
    std::fs::read_dir("/proc").is_ok_and(|entries| {
        entries.flatten().any(|entry| std::fs::read_to_string(entry.path().join("comm")).is_ok_and(|comm| comm.trim_end() == "steam"))
    })
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

// ---------------------------------------------------------------- the controller side

fn job_id(app: u32) -> String {
    format!("{JOB_PREFIX}{app}")
}

struct Reporter {
    executor: Executor,
    core: Core,
}

impl Reporter {
    /// Clear this reporter's own stale faults, restore unexpired error warnings, and adopt
    /// the jobs a previous reporter left behind.
    async fn recover(&mut self) {
        let Some(status) = self.executor.status().await else { return };
        let (now, now_unix) = (Instant::now(), unix_now());
        for fault in status["active_faults"].as_array().into_iter().flatten() {
            let (Some(id), reason) = (fault["id"].as_str(), fault["reason"].as_str().unwrap_or("")) else { continue };
            if id.starts_with(OWN_FAULT_PREFIX) {
                self.executor.clear(id).await;
            } else if let Some(rest) = id.strip_prefix(ERROR_PREFIX) {
                match rest.split_once(':').and_then(|(app, expiry)| Some((app.parse::<u32>().ok()?, expiry.parse::<u64>().ok()?))) {
                    Some((app, expiry)) if expiry > now_unix => {
                        self.core.errors.insert(app, (expiry, reason.to_owned()));
                        self.core.raised.insert(id.to_owned(), reason.to_owned());
                    }
                    _ => self.executor.clear(id).await,
                }
            }
        }
        for job in status["jobs"].as_array().into_iter().flatten() {
            if let Some(app) = job["id"].as_str().and_then(|id| id.strip_prefix(JOB_PREFIX)).and_then(|app| app.parse().ok()) {
                self.core.adopt(app, now, now_unix);
                self.executor.log(format!("adopted job {}", job_id(app)));
            }
        }
    }

    async fn perform(&mut self, action: Action) {
        match action {
            Action::Start { app, label, total } => {
                if !self.executor.start(&job_id(app), &label, total).await {
                    self.core.set_announced(app, false);
                }
            }
            Action::Progress { app, completed, total } => {
                if !self.executor.progress(&job_id(app), completed, total).await {
                    self.core.set_announced(app, false);
                }
            }
            Action::Complete { app } => self.executor.end(job_id(app), Ending::Complete).await,
            Action::Fail { app, reason } => self.executor.end(job_id(app), Ending::Fail(reason)).await,
            Action::Warn { id, reason } => self.executor.warn(&id, &reason).await,
            Action::Clear { id } => self.executor.clear(&id).await,
        }
    }

    async fn step(&mut self) {
        self.executor.retry().await;
        for action in self.core.tick(Instant::now(), unix_now(), steam_alive()) {
            self.perform(action).await;
        }
    }

    /// Re-raise every warning that should be up; the controller forgets them on restart.
    async fn reassert(&mut self) {
        for (id, reason) in self.core.raised.clone() {
            self.executor.raise(&id, &reason).await;
        }
    }
}

const USAGE: &str = "usage: monolithd steam-reporter [--dry-run]";

/// `monolithd steam-reporter`: report Steam's downloads as controller jobs.
pub async fn run(arguments: Vec<String>) -> Result<(), String> {
    let dry_run = match arguments.as_slice() {
        [] => false,
        [flag] if flag == "--dry-run" => true,
        _ => return Err(USAGE.to_owned()),
    };
    let mut reporter = Reporter { executor: Executor::new("steam-reporter", dry_run), core: Core::default() };
    reporter.recover().await;
    eprintln!("monolithd steam-reporter: following Steam's download API on {DEVTOOLS}{}", if dry_run { " (dry run)" } else { "" });
    let (sender, mut links) = mpsc::channel(64);
    tokio::spawn(follow(sender));
    let mut tick = interval(POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut reassert = interval(REASSERT);
    reassert.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_down: Option<String> = None;
    loop {
        tokio::select! {
            Some(link) = links.recv() => {
                let now = Instant::now();
                match link {
                    Link::Up => {
                        reporter.core.set_connected(true);
                        last_down = None;
                        eprintln!("monolithd steam-reporter: connected to Steam");
                    }
                    Link::Down(reason) => {
                        reporter.core.set_connected(false);
                        if last_down.as_deref() != Some(reason.as_str()) {
                            eprintln!("monolithd steam-reporter: not connected: {reason}");
                            last_down = Some(reason);
                        }
                    }
                    Link::Data(Ok(observation)) => {
                        reporter.core.set_shape(None, now);
                        reporter.core.observe(observation);
                    }
                    Link::Data(Err(problem)) | Link::Shape(Some(problem)) => {
                        eprintln!("monolithd steam-reporter: {problem}");
                        reporter.core.set_shape(Some(problem), now);
                    }
                    Link::Shape(None) => reporter.core.set_shape(None, now),
                }
                reporter.step().await;
            }
            _ = tick.tick() => reporter.step().await,
            _ = reassert.tick() => reporter.reassert().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);
    const FIXTURE: &str = include_str!("../tests/fixtures/steam-downloads-2026-09-25.jsonl");

    fn overview(app: u32, state: &str, shader: bool, paused: bool, done: u64, total: u64) -> Observation {
        Observation::Overview(Overview { app, state: state.into(), shader, paused, download: (done, total), percent: 0, name: Some(format!("Game {app}")) })
    }

    fn item(app: u32, active: bool, completed: bool, completed_time: u64, result: i64, error: &str) -> Observation {
        Observation::Items(vec![Item { app, active, completed, completed_time, result, error: error.into() }])
    }

    /// Feed observations at the given second offsets, ticking once a second, and return
    /// every action with the second it happened.
    fn run(core: &mut Core, base_unix: u64, until: u64, events: Vec<(u64, Observation)>, steam_alive: impl Fn(u64) -> bool) -> Vec<(u64, Action)> {
        let base = Instant::now();
        let mut events = events.into_iter().peekable();
        let mut out = Vec::new();
        for second in 0..=until {
            while events.peek().is_some_and(|(at, _)| *at <= second) {
                core.observe(events.next().unwrap().1);
            }
            for action in core.tick(base + S * second as u32, base_unix + second, steam_alive(second)) {
                out.push((second, action));
            }
        }
        out
    }

    fn connected() -> Core {
        Core { connected: true, ..Core::default() }
    }

    #[test]
    fn parses_the_subscription_payloads() {
        let text = r#"{"kind":"overview","value":{"remote_client_id":"0","update_appid":292030,"update_state":"Downloading","update_is_shader":false,"paused":false,"progress":[[0,0],[5,5],[100,2048],[1,1],[0,0],[0,0],[0,5]],"overall_percent_complete":4,"name":"The Witcher 3"},"missing":[]}"#;
        assert_eq!(
            parse_event(text).unwrap(),
            Observation::Overview(Overview { app: 292030, state: "Downloading".into(), shader: false, paused: false, download: (100, 2048), percent: 4, name: Some("The Witcher 3".into()) })
        );
        let text = r#"{"kind":"items","value":[{"appid":7,"active":false,"paused":false,"completed":true,"completed_time":1790343452,"update_result":0,"update_error":""}],"missing":[]}"#;
        assert_eq!(parse_event(text).unwrap(), item(7, false, true, 1790343452, 0, ""));
    }

    #[test]
    fn a_changed_api_is_reported_not_guessed() {
        assert!(parse_event(r#"{"kind":"overview","value":{},"missing":["progress","update_appid"]}"#).unwrap_err().contains("no longer has progress, update_appid"));
        let short = r#"{"kind":"overview","value":{"update_appid":1,"update_state":"Downloading","update_is_shader":false,"paused":false,"progress":[[0,0],[1,1]],"overall_percent_complete":1},"missing":[]}"#;
        assert!(parse_event(short).unwrap_err().contains("expected the download in phase 2"));
        assert!(parse_event(r#"{"kind":"items","value":[{"appid":"x"}],"missing":[]}"#).is_err());
    }

    #[test]
    fn progress_is_download_bytes_and_full_during_install() {
        let o = |state: &str, done, total, percent| Overview { app: 1, state: state.into(), download: (done, total), percent, ..Overview::default() };
        assert_eq!(progress_of(&o("Downloading", 10240, 20480, 0)), Some((10, 20)));
        assert_eq!(progress_of(&o("Installing", 10240, 20480, 0)), Some((20, 20)));
        assert_eq!(progress_of(&o("Downloading", 20480, 20480, 0)), Some((20, 20)));
        assert_eq!(progress_of(&o("Starting", 0, 0, 0)), None);
        assert_eq!(progress_of(&o("Updating", 0, 0, 37)), Some((37, 100)), "an update that downloads nothing uses Steam's percent");
    }

    /// Today's real recording: one job for the Witcher, none for the Redistributables that
    /// Steam had already finished, no failures, no warnings.
    #[test]
    fn replays_the_recorded_witcher_download() {
        let mut lines = FIXTURE.lines().filter(|line| !line.starts_with('#'));
        let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        let base_unix = header["t0_unix"].as_u64().unwrap();
        let events: Vec<(u64, Observation)> = lines
            .map(|line| {
                let at = serde_json::from_str::<Value>(line).unwrap()["t"].as_f64().unwrap();
                (at.ceil() as u64, parse_event(line).unwrap())
            })
            .collect();
        let until = events.last().unwrap().0 + 60;
        let actions = run(&mut connected(), base_unix, until, events, |_| true);
        let kinds: Vec<&Action> = actions.iter().map(|(_, action)| action).filter(|action| !matches!(action, Action::Progress { .. })).collect();
        assert_eq!(kinds, vec![&Action::Start { app: 292030, label: "The Witcher 3: Wild Hunt - Complete Edition".into(), total: kib_ceil(55536635376) }, &Action::Complete { app: 292030 }]);
        let (start, _) = actions.iter().find(|(_, action)| matches!(action, Action::Start { .. })).unwrap();
        assert!((106..=108).contains(start), "announced 10 s after Steam started the download at t=96, got t={start}");
        let bars: Vec<u32> = actions.iter().filter_map(|(_, action)| if let Action::Progress { completed, .. } = action { Some(*completed) } else { None }).collect();
        assert!(bars.windows(2).all(|pair| pair[0] <= pair[1]), "the bar never moves backwards");
        assert_eq!(*bars.last().unwrap(), kib_ceil(55536635376), "full before completion");
        assert!(bars.len() > 100, "the bar was reported live, {} times", bars.len());
    }

    #[test]
    fn a_short_download_never_becomes_a_job() {
        let events = vec![(0, overview(7, "Downloading", false, false, 1, 100)), (5, overview(0, "None", false, false, 0, 0)), (5, item(7, false, true, 1_000_005, 0, ""))];
        assert!(run(&mut connected(), 1_000_000, 40, events, |_| true).is_empty());
    }

    #[test]
    fn a_brief_suspension_or_shader_interlude_does_not_end_the_job() {
        let events = vec![
            (0, overview(7, "Downloading", false, false, 1024, 1_048_576)),
            (20, overview(7, "Downloading", true, false, 0, 0)),
            (26, overview(9, "Downloading", false, false, 0, 10)),
            (32, overview(7, "Downloading", false, false, 4096, 1_048_576)),
        ];
        let actions = run(&mut connected(), 1_000_000, 40, events, |_| true);
        assert!(actions.iter().all(|(_, action)| !matches!(action, Action::Fail { .. })), "{actions:?}");
        let held: Vec<u32> = actions.iter().filter_map(|(at, action)| if let (20..=31, Action::Progress { completed, .. }) = (at, action) { Some(*completed) } else { None }).collect();
        assert!(held.is_empty(), "no reports while it is not downloading, the bar holds");
    }

    #[test]
    fn a_pause_longer_than_the_grace_ends_the_job() {
        let events = vec![(0, overview(7, "Downloading", false, false, 1024, 1_048_576)), (20, overview(7, "Downloading", false, true, 1024, 1_048_576))];
        let fails: Vec<u64> = run(&mut connected(), 1_000_000, 60, events, |_| true).into_iter().filter_map(|(at, action)| matches!(action, Action::Fail { app: 7, .. }).then_some(at)).collect();
        assert_eq!(fails, vec![35]);
    }

    #[test]
    fn completion_is_the_edge_after_the_job_began_not_an_old_flag() {
        // Steam still says "completed" from an old download; that must not end this one.
        let events = vec![(0, item(7, false, true, 999_000, 0, "")), (0, overview(7, "Downloading", false, false, 1024, 1_048_576))];
        let actions = run(&mut connected(), 1_000_000, 30, events, |_| true);
        assert!(actions.iter().all(|(_, action)| !matches!(action, Action::Complete { .. })));
        assert!(actions.iter().any(|(_, action)| matches!(action, Action::Start { app: 7, .. })));
    }

    #[test]
    fn a_steam_error_fails_the_job_and_warns_for_thirty_minutes() {
        let events = vec![
            (0, overview(7, "Downloading", false, false, 1024, 1_048_576)),
            (20, overview(0, "None", false, false, 0, 0)),
            (20, item(7, false, false, 0, 8, "Disk write failure")),
        ];
        let actions = run(&mut connected(), 1_000_000, 20 + 1800 + 2, events, |_| true);
        let id = error_id(7, 1_000_020 + 1800);
        assert!(actions.contains(&(20, Action::Fail { app: 7, reason: "Steam reported an error: Disk write failure".into() })));
        assert!(actions.contains(&(20, Action::Warn { id: id.clone(), reason: "Steam could not download Game 7: Disk write failure".into() })));
        assert!(actions.contains(&(1820, Action::Clear { id })));
    }

    #[test]
    fn a_blind_reporter_warns_after_a_minute_and_releases_the_bar() {
        let mut core = connected();
        let actions = run(&mut core, 1_000_000, 20, vec![(0, overview(7, "Downloading", false, false, 1024, 1_048_576))], |_| true);
        assert!(actions.iter().any(|(_, action)| matches!(action, Action::Start { .. })));
        core.set_connected(false);
        let actions = run(&mut core, 1_000_020, 61, vec![], |_| true);
        let blind: Vec<(u64, &Action)> = actions.iter().filter(|(_, action)| !matches!(action, Action::Progress { .. })).map(|(at, action)| (*at, action)).collect();
        assert_eq!(blind.len(), 2, "{blind:?}");
        assert!(blind.iter().any(|entry| matches!(entry, (60, Action::Warn { id, .. }) if id == BLIND_ID)), "{blind:?}");
        assert!(blind.iter().any(|entry| matches!(entry, (60, Action::Fail { app: 7, .. }))), "{blind:?}");
        core.set_connected(true);
        assert_eq!(core.tick(Instant::now(), 1_000_090, true), vec![Action::Clear { id: BLIND_ID.into() }]);
    }

    #[test]
    fn steam_exiting_ends_the_job_at_once_and_raises_nothing() {
        let actions = run(&mut connected(), 1_000_000, 30, vec![(0, overview(7, "Downloading", false, false, 1024, 1_048_576))], |second| second < 20);
        let rest: Vec<&(u64, Action)> = actions.iter().filter(|(_, action)| !matches!(action, Action::Progress { .. } | Action::Start { .. })).collect();
        assert_eq!(rest, vec![&(20, Action::Fail { app: 7, reason: "Steam is not running".into() })]);
    }

    #[test]
    fn a_changed_api_warns_after_a_minute() {
        let mut core = connected();
        let start = Instant::now();
        core.set_shape(Some("Steam's download overview no longer has progress".into()), start);
        assert!(core.tick(start + S * 59, 1_000_059, true).is_empty());
        assert_eq!(core.tick(start + S * 60, 1_000_060, true), vec![Action::Warn { id: SHAPE_ID.into(), reason: "Steam's download overview no longer has progress".into() }]);
        core.set_shape(None, start + S * 70);
        assert_eq!(core.tick(start + S * 70, 1_000_070, true), vec![Action::Clear { id: SHAPE_ID.into() }]);
    }

    #[test]
    fn an_adopted_job_continues_or_ends() {
        let mut core = connected();
        let now = Instant::now();
        core.adopt(7, now, 1_000_000);
        core.adopt(8, now, 1_000_000);
        let actions = run(&mut core, 1_000_000, 20, vec![(0, overview(7, "Downloading", false, false, 1024, 1_048_576))], |_| true);
        assert!(actions.iter().all(|(_, action)| !matches!(action, Action::Start { .. })), "adopted jobs are not announced again");
        assert!(actions.iter().any(|(_, action)| matches!(action, Action::Progress { app: 7, .. })));
        assert!(actions.contains(&(15, Action::Fail { app: 8, reason: "Steam stopped downloading it (paused, queued or cancelled)".into() })));
    }
}
