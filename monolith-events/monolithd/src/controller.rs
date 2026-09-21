//! The Monolith Event Controller: the setpoint layer for the lighting plant.
//!
//! It holds what each zone should show (an ambient set, plus a progress lease per
//! truthful job), reads the plant's state through the scene gateway, and drives the
//! difference to zero. Reconciliation is its only act: the same loop brings the lights
//! up at boot, re-asserts them after a stack restart, and moves a zone between ambient
//! and a progress bar. It never renders anything itself and never touches QLC+ or OpenRGB.

use crate::allocator::{JobState, Planner, ZoneTarget};
use crate::gateway;
use crate::registry::{self, Registry};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, timeout, MissedTickBehavior};

const MAX_REQUEST_BYTES: usize = 4096;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(20);
const RECENT_ACTIONS: usize = 12;

// ---------------------------------------------------------------- configuration

/// Controller policy (`controller.toml`). What plays is data in the registry; this says
/// which set is the default and how progress zones are allocated.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerConfig {
    pub version: u32,
    pub default_ambient: String,
    /// Progress-capable zones in allocation order.
    pub progress_zones: Vec<String>,
    /// How long a completed job holds its bar at 100% before the zone is released.
    pub complete_hold_seconds: u64,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u64,
}

fn default_poll_interval() -> u64 {
    1000
}

pub fn load_config(path: &Path) -> Result<ControllerConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))
}

/// Every disagreement between the policy and the registry.
pub fn check_config(config: &ControllerConfig, registry: &Registry) -> Vec<String> {
    let mut problems = Vec::new();
    if config.version != 1 {
        problems.push(format!("controller.toml version {} is unsupported (expected 1)", config.version));
    }
    match registry.ambient_set(&config.default_ambient) {
        None => problems.push(format!("default_ambient {} is not an ambient set in the registry", config.default_ambient)),
        Some(_) => {}
    }
    if config.progress_zones.is_empty() {
        problems.push("progress_zones is empty".to_owned());
    }
    let mut seen = BTreeSet::new();
    for zone in &config.progress_zones {
        if !seen.insert(zone) {
            problems.push(format!("progress zone {zone} is listed twice"));
        }
        if registry.progress_for_zone(zone).is_none() {
            problems.push(format!("progress zone {zone} has no progress family in the registry"));
        }
    }
    if config.complete_hold_seconds > 3600 {
        problems.push("complete_hold_seconds must be at most 3600".to_owned());
    }
    if !(100..=60_000).contains(&config.poll_interval_ms) {
        problems.push("poll_interval_ms must be between 100 and 60000".to_owned());
    }
    problems
}

// ---------------------------------------------------------------- planning

/// What one managed zone should show.
#[derive(Debug, Clone, PartialEq)]
enum Want {
    Ambient { function: String },
    Progress { zone: String, family: String, step: u32 },
}

impl Want {
    /// The owner name the gateway reports when the zone shows this.
    fn owner_name(&self) -> String {
        match self {
            Self::Ambient { function } => function.clone(),
            Self::Progress { family, step, .. } => format!("{family}:{step}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Stop(String),
    StartSet(Vec<String>),
    Rejoin(String),
    Progress { zone: String, step: u32 },
}

impl Action {
    fn request(&self) -> Value {
        match self {
            Self::Stop(function) => json!({ "op": "stop", "function": function }),
            Self::StartSet(functions) => json!({ "op": "start_set", "functions": functions }),
            Self::Rejoin(function) => json!({ "op": "rejoin", "function": function }),
            Self::Progress { zone, step } => json!({ "op": "progress", "zone": zone, "completed": step }),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Stop(function) => format!("stop {function}"),
            Self::StartSet(functions) => format!("start set [{}]", functions.join(", ")),
            Self::Rejoin(function) => format!("rejoin {function}"),
            Self::Progress { zone, step } => format!("progress {zone} {step}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ZoneView {
    owner: Option<String>,
    uncertain: bool,
}

/// The part of the gateway's `status` the controller acts on.
#[derive(Debug, Clone)]
struct GatewayView {
    mode: String,
    qlc_reachable: bool,
    output_state: Option<String>,
    calibration_state: Option<String>,
    zones: BTreeMap<String, ZoneView>,
}

impl GatewayView {
    fn from_status(status: &Value) -> Result<Self, String> {
        if status["ok"] != Value::Bool(true) {
            return Err(format!("gateway status failed: {}", status["error"].as_str().unwrap_or("unknown error")));
        }
        let zones = status["zones"]
            .as_object()
            .ok_or("gateway status has no zones")?
            .iter()
            .map(|(name, zone)| (name.clone(), ZoneView { owner: zone["owner"].as_str().map(str::to_owned), uncertain: zone["uncertain"] == Value::Bool(true) }))
            .collect();
        Ok(Self {
            mode: status["mode"].as_str().unwrap_or("unknown").to_owned(),
            qlc_reachable: status["qlc_reachable"] == Value::Bool(true),
            output_state: status["output"]["state"].as_str().map(str::to_owned),
            calibration_state: status["calibration"]["state"].as_str().map(str::to_owned),
            zones,
        })
    }
}

struct Plan {
    actions: Vec<Action>,
    blocked: Option<String>,
}

enum Owner<'a> {
    Free,
    Ambient,
    Progress,
    Foreign(&'a str),
}

fn classify<'a>(owner: Option<&'a String>, registry: &Registry) -> Owner<'a> {
    let Some(name) = owner else { return Owner::Free };
    if registry.ambient_set_of(name).is_some() {
        Owner::Ambient
    } else if name.split_once(':').is_some_and(|(family, _)| registry.progress.iter().any(|entry| entry.name == family)) {
        Owner::Progress
    } else {
        Owner::Foreign(name)
    }
}

/// Turn desired state and gateway state into the actions that close the gap.
fn plan(view: &GatewayView, wants: &BTreeMap<String, Want>, registry: &Registry) -> Plan {
    let blocked = |reason: String| Plan { actions: Vec::new(), blocked: Some(reason) };
    if view.mode != "control" {
        return blocked(format!("the gateway is {}, not in control mode", view.mode));
    }
    if !view.qlc_reachable {
        return blocked("QLC+ is not reachable".to_owned());
    }
    if let Some(state) = view.output_state.as_deref().filter(|state| *state != "ok") {
        return blocked(format!("the OpenRGB output is {state}"));
    }
    for zone in wants.keys() {
        match view.zones.get(zone) {
            None => return blocked(format!("the gateway does not know zone {zone}")),
            Some(entry) if entry.uncertain => return blocked(format!("zone {zone} is in an unconfirmed state")),
            Some(_) => {}
        }
    }

    let mut stops: Vec<String> = Vec::new();
    let mut free_for_ambient: Vec<String> = Vec::new();
    let mut rejoins: Vec<String> = Vec::new();
    let mut progress: Vec<Action> = Vec::new();
    let mut ambient_running = false;

    for (zone, want) in wants {
        let owner = view.zones[zone].owner.as_ref();
        if owner.is_some_and(|name| *name == want.owner_name()) {
            ambient_running |= matches!(want, Want::Ambient { .. });
            continue;
        }
        // A non-composable owner (the bench scene, say) has to be stopped before anything replaces it.
        let stop_foreign = |name: &str, stops: &mut Vec<String>| -> bool {
            let composable = registry.function(name).is_some_and(|entry| entry.composable);
            if !composable && !stops.iter().any(|known| known == name) {
                stops.push(name.to_owned());
            }
            !composable
        };
        match (want, classify(owner, registry)) {
            (Want::Ambient { function }, Owner::Free) => free_for_ambient.push(function.clone()),
            (Want::Ambient { function }, Owner::Ambient | Owner::Progress) => rejoins.push(function.clone()),
            (Want::Ambient { function }, Owner::Foreign(name)) => {
                if stop_foreign(name, &mut stops) {
                    free_for_ambient.push(function.clone());
                } else {
                    rejoins.push(function.clone());
                }
            }
            (Want::Progress { zone, step, .. }, foreign_or_other) => {
                if let Owner::Foreign(name) = foreign_or_other {
                    stop_foreign(name, &mut stops);
                }
                progress.push(Action::Progress { zone: zone.clone(), step: *step });
            }
        }
    }

    let mut actions: Vec<Action> = stops.into_iter().map(Action::Stop).collect();
    if !free_for_ambient.is_empty() {
        if ambient_running {
            actions.extend(free_for_ambient.into_iter().map(Action::Rejoin));
        } else {
            actions.push(Action::StartSet(free_for_ambient));
        }
    }
    actions.extend(rejoins.into_iter().map(Action::Rejoin));
    actions.extend(progress);
    Plan { actions, blocked: None }
}

// ---------------------------------------------------------------- the controller

/// How the controller talks to the gateway. A trait so tests can substitute a fake.
trait Link {
    fn call(&self, request: Value) -> impl Future<Output = Result<Value, String>>;
}

struct GatewayLink;

impl Link for GatewayLink {
    fn call(&self, request: Value) -> impl Future<Output = Result<Value, String>> {
        async move { gateway::call(&request).await }
    }
}

#[derive(Default)]
struct Report {
    blocked: Option<String>,
    gateway: Value,
    owners: BTreeMap<String, Option<String>>,
    wants: BTreeMap<String, String>,
    recent: VecDeque<String>,
    reconciles: u64,
}

struct Controller<L: Link> {
    link: L,
    registry: Registry,
    config: ControllerConfig,
    planner: Planner,
    ambient: String,
    paused: bool,
    dirty: bool,
    report: Report,
}

fn failure(code: &str, message: impl Into<String>) -> Value {
    json!({ "ok": false, "code": code, "error": message.into() })
}

impl<L: Link> Controller<L> {
    fn new(link: L, registry: Registry, config: ControllerConfig) -> Self {
        let zones = config
            .progress_zones
            .iter()
            .map(|zone| (zone.clone(), registry.progress_for_zone(zone).map_or(0, |entry| entry.total)))
            .collect();
        let planner = Planner::new(zones, Duration::from_secs(config.complete_hold_seconds));
        let ambient = config.default_ambient.clone();
        Self { link, registry, config, planner, ambient, paused: false, dirty: true, report: Report::default() }
    }

    fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// The zones the controller manages and what each should show.
    fn wants(&self) -> BTreeMap<String, Want> {
        let mut wants = BTreeMap::new();
        if let Some(set) = self.registry.ambient_set(&self.ambient) {
            for function in &set.functions {
                for zone in self.registry.function(function).map(|entry| entry.zones.clone()).unwrap_or_default() {
                    wants.insert(zone, Want::Ambient { function: function.clone() });
                }
            }
        }
        for zone in &self.config.progress_zones {
            if let (ZoneTarget::Progress { step }, Some(family)) = (self.planner.target(zone), self.registry.progress_for_zone(zone)) {
                wants.insert(zone.clone(), Want::Progress { zone: zone.clone(), family: family.name.clone(), step });
            }
        }
        wants
    }

    fn remember(&mut self, line: String) {
        eprintln!("monolithd controller: {line}");
        self.report.recent.push_back(line);
        while self.report.recent.len() > RECENT_ACTIONS {
            self.report.recent.pop_front();
        }
    }

    fn block(&mut self, reason: Option<String>) {
        if self.report.blocked != reason {
            match &reason {
                Some(reason) => eprintln!("monolithd controller: waiting, {reason}"),
                None if self.report.blocked.is_some() => eprintln!("monolithd controller: gateway ready, resuming"),
                None => {}
            }
            self.report.blocked = reason;
        }
    }

    /// One pass of the loop: read the plant, compare, and send what closes the gap.
    async fn reconcile(&mut self, now: Instant) {
        self.planner.tick(now);
        self.report.reconciles += 1;
        if self.paused {
            self.block(Some("paused".to_owned()));
            return;
        }
        let status = match self.link.call(json!({ "op": "status" })).await {
            Ok(status) => status,
            Err(error) => return self.block(Some(format!("the gateway is unreachable ({error})"))),
        };
        let view = match GatewayView::from_status(&status) {
            Ok(view) => view,
            Err(error) => return self.block(Some(error)),
        };
        self.report.gateway = json!({ "mode": view.mode, "qlc_reachable": view.qlc_reachable, "output": view.output_state, "calibration": view.calibration_state });
        self.report.owners = view.zones.iter().map(|(zone, entry)| (zone.clone(), entry.owner.clone())).collect();
        let wants = self.wants();
        self.report.wants = wants.iter().map(|(zone, want)| (zone.clone(), want.owner_name())).collect();

        let plan = plan(&view, &wants, &self.registry);
        self.block(plan.blocked);
        for action in plan.actions {
            let reply = match self.link.call(action.request()).await {
                Ok(reply) => reply,
                Err(error) => {
                    self.remember(format!("{} failed: {error}", action.describe()));
                    return;
                }
            };
            if reply["ok"] != Value::Bool(true) {
                let reason = format!("{}: {}", reply["code"].as_str().unwrap_or("error"), reply["error"].as_str().unwrap_or("request failed"));
                self.remember(format!("{} refused, {reason}", action.describe()));
                return;
            }
            self.remember(action.describe());
        }
    }

    fn job_json(&self, id: &str) -> Value {
        let Some(job) = self.planner.jobs().iter().find(|job| job.id == id) else { return Value::Null };
        job_json(job)
    }

    fn handle(&mut self, request: ControllerRequest, now: Instant) -> Value {
        let outcome = match request {
            ControllerRequest::Status => return self.status_json(),
            ControllerRequest::AmbientSelect { set } => match self.registry.ambient_set(&set) {
                None => Err(("unknown_set", format!("{set} is not an ambient set"))),
                Some(_) => {
                    self.ambient = set.clone();
                    Ok(json!({ "ambient": set }))
                }
            },
            ControllerRequest::JobStart { id, label, total, priority } => {
                self.planner.start(&id, &label, total, priority).map(|()| json!({ "job": self.job_json(&id) })).map_err(|error| ("bad_request", error))
            }
            ControllerRequest::JobProgress { id, completed, total } => {
                self.planner.progress(&id, completed, total).map(|()| json!({ "job": self.job_json(&id) })).map_err(|error| ("unknown_job", error))
            }
            ControllerRequest::JobComplete { id } => self.planner.complete(&id, now).map(|()| json!({ "job": self.job_json(&id) })).map_err(|error| ("unknown_job", error)),
            ControllerRequest::JobFail { id, reason } => self.planner.fail(&id, &reason).map(|()| json!({})).map_err(|error| ("unknown_job", error)),
            ControllerRequest::Pause => {
                self.paused = true;
                Ok(json!({ "paused": true }))
            }
            ControllerRequest::Resume => {
                self.paused = false;
                Ok(json!({ "paused": false }))
            }
        };
        match outcome {
            Ok(mut reply) => {
                self.dirty = true;
                reply["ok"] = Value::Bool(true);
                reply
            }
            Err((code, error)) => failure(code, error),
        }
    }

    fn status_json(&self) -> Value {
        let mut zones = Map::new();
        for (zone, want) in &self.report.wants {
            zones.insert(zone.clone(), json!({ "want": want, "owner": self.report.owners.get(zone).cloned().flatten() }));
        }
        let failures: Vec<Value> = self.planner.failures().iter().map(|entry| json!({ "id": entry.id, "label": entry.label, "reason": entry.reason })).collect();
        json!({
            "ok": true,
            "paused": self.paused,
            "ambient": self.ambient,
            "complete_hold_seconds": self.config.complete_hold_seconds,
            "waiting": self.report.blocked,
            "gateway": self.report.gateway,
            "zones": zones,
            "jobs": self.planner.jobs().iter().map(job_json).collect::<Vec<_>>(),
            "failures": failures,
            "recent_actions": self.report.recent,
            "reconciles": self.report.reconciles,
        })
    }
}

fn job_json(job: &crate::allocator::Job) -> Value {
    let (state, zone) = match &job.state {
        JobState::Queued => ("queued", Value::Null),
        JobState::Leased(zone) => ("leased", json!(zone)),
        JobState::Completing { zone, .. } => ("completing", json!(zone)),
    };
    json!({ "id": job.id, "label": job.label, "completed": job.completed, "total": job.total, "priority": job.priority, "state": state, "zone": zone })
}

// ---------------------------------------------------------------- the control socket

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "op", deny_unknown_fields)]
pub enum ControllerRequest {
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "ambient.select")]
    AmbientSelect { set: String },
    #[serde(rename = "job.start")]
    JobStart {
        id: String,
        label: String,
        total: u32,
        #[serde(default)]
        priority: i32,
    },
    #[serde(rename = "job.progress")]
    JobProgress { id: String, completed: u32, total: Option<u32> },
    #[serde(rename = "job.complete")]
    JobComplete { id: String },
    #[serde(rename = "job.fail")]
    JobFail { id: String, reason: String },
    #[serde(rename = "pause")]
    Pause,
    #[serde(rename = "resume")]
    Resume,
}

type Envelope = (ControllerRequest, oneshot::Sender<Value>);

pub fn socket_path() -> Result<PathBuf, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(runtime).join("monolith-events").join("controller.sock"))
}

async fn serve(listener: UnixListener, owner_uid: u32, requests: mpsc::Sender<Envelope>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let requests = requests.clone();
        tokio::spawn(async move {
            if let Err(error) = connection(stream, owner_uid, requests).await {
                eprintln!("monolithd controller: connection dropped: {error}");
            }
        });
    }
}

async fn connection(stream: UnixStream, owner_uid: u32, requests: mpsc::Sender<Envelope>) -> Result<(), String> {
    let peer = stream.peer_cred().map_err(|error| format!("peer credentials: {error}"))?;
    if peer.uid() != owner_uid {
        return Err(format!("rejected peer with uid {}", peer.uid()));
    }
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = Vec::new();
        match timeout(IDLE_TIMEOUT, (&mut reader).take(MAX_REQUEST_BYTES as u64).read_until(b'\n', &mut line)).await {
            Err(_) | Ok(Ok(0)) => return Ok(()),
            Ok(Err(error)) => return Err(format!("read request: {error}")),
            Ok(Ok(_)) => {}
        }
        if line.last() != Some(&b'\n') {
            let reply = failure("request_too_large", format!("requests are limited to {MAX_REQUEST_BYTES} bytes and end with a newline"));
            let _ = writer.write_all(format!("{reply}\n").as_bytes()).await;
            return Ok(());
        }
        let reply = match serde_json::from_slice::<ControllerRequest>(&line) {
            Ok(request) => {
                let (send, receive) = oneshot::channel();
                requests.send((request, send)).await.map_err(|_| "controller is shutting down".to_owned())?;
                receive.await.map_err(|_| "controller dropped the request".to_owned())?
            }
            Err(error) => failure("bad_request", error.to_string()),
        };
        writer.write_all(format!("{reply}\n").as_bytes()).await.map_err(|error| format!("write reply: {error}"))?;
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// `monolithd controller`: run the controller until it is stopped.
pub async fn run() -> Result<(), String> {
    let root = root();
    let layout = crate::config::load_layout(&root.join("scene-layout.toml"))?;
    let (registry, problems) = registry::load_and_validate(&root.join("qlc-functions.toml"), &layout)?;
    if let Some(first) = problems.first() {
        return Err(format!("the registry does not match the QLC+ workspace ({} problem(s)); first: {first}", problems.len()));
    }
    let config = load_config(&root.join("controller.toml"))?;
    let problems = check_config(&config, &registry);
    if let Some(first) = problems.first() {
        return Err(format!("controller.toml is not valid ({} problem(s)); first: {first}", problems.len()));
    }

    let path = socket_path()?;
    let listener = gateway::prepare_socket(&path)?;
    let uid = std::fs::metadata(&path).map_err(|error| format!("stat {}: {error}", path.display()))?.uid();
    let poll = Duration::from_millis(config.poll_interval_ms);
    let mut controller = Controller::new(GatewayLink, registry, config);
    eprintln!("monolithd controller: ambient {}, listening on {}", controller.ambient, path.display());

    let (requests, mut incoming) = mpsc::channel::<Envelope>(32);
    tokio::spawn(serve(listener, uid, requests));
    let mut poll = interval(poll);
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut soon = interval(Duration::from_millis(100));
    soon.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            Some((request, reply)) = incoming.recv() => {
                let value = controller.handle(request, Instant::now());
                let _ = reply.send(value);
            }
            _ = poll.tick() => controller.reconcile(Instant::now()).await,
            _ = soon.tick() => {
                if controller.take_dirty() {
                    controller.reconcile(Instant::now()).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------- the client

const USAGE: &str = "usage: monolithd event <status | ambient SET | job-start ID LABEL TOTAL [PRIORITY] | job-progress ID COMPLETED [TOTAL] | job-complete ID | job-fail ID REASON... | pause | resume>";

fn client_request(arguments: &[String]) -> Result<Value, String> {
    let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let number = |text: &str| text.parse::<u32>().map_err(|_| format!("{text:?} is not a non-negative integer"));
    Ok(match words.as_slice() {
        ["status"] => json!({ "op": "status" }),
        ["ambient", set] => json!({ "op": "ambient.select", "set": set }),
        ["job-start", id, label, total] => json!({ "op": "job.start", "id": id, "label": label, "total": number(total)? }),
        ["job-start", id, label, total, priority] => {
            let priority = priority.parse::<i32>().map_err(|_| format!("{priority:?} is not an integer"))?;
            json!({ "op": "job.start", "id": id, "label": label, "total": number(total)?, "priority": priority })
        }
        ["job-progress", id, completed] => json!({ "op": "job.progress", "id": id, "completed": number(completed)? }),
        ["job-progress", id, completed, total] => json!({ "op": "job.progress", "id": id, "completed": number(completed)?, "total": number(total)? }),
        ["job-complete", id] => json!({ "op": "job.complete", "id": id }),
        ["job-fail", id, reason @ ..] if !reason.is_empty() => json!({ "op": "job.fail", "id": id, "reason": reason.join(" ") }),
        ["pause"] => json!({ "op": "pause" }),
        ["resume"] => json!({ "op": "resume" }),
        _ => return Err(USAGE.to_owned()),
    })
}

/// `monolithd event ...`: a one-shot client for the controller socket.
pub async fn client(arguments: Vec<String>) -> Result<(), String> {
    let request = client_request(&arguments)?;
    let path = socket_path()?;
    let exchange = async {
        let mut stream = UnixStream::connect(&path).await.map_err(|error| format!("connect {} (is the controller running?): {error}", path.display()))?;
        stream.write_all(format!("{request}\n").as_bytes()).await.map_err(|error| format!("send request: {error}"))?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).await.map_err(|error| format!("read reply: {error}"))?;
        Ok::<String, String>(reply)
    };
    let reply = timeout(CLIENT_TIMEOUT, exchange).await.map_err(|_| "controller did not answer in time".to_owned())??;
    let reply: Value = serde_json::from_str(&reply).map_err(|error| format!("controller sent invalid JSON: {error}"))?;
    println!("{}", serde_json::to_string_pretty(&reply).unwrap_or_else(|_| reply.to_string()));
    if reply["ok"] == Value::Bool(true) {
        Ok(())
    } else {
        Err(format!("{}: {}", reply["code"].as_str().unwrap_or("error"), reply["error"].as_str().unwrap_or("request failed")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const REGISTRY: &str = r#"
        version = 1
        workspace = "unused.qxw"
        [zones.ram]
        regions = [{ universe = 0, address = 0, channels = 96 }]
        [zones.rog_eye]
        regions = [{ universe = 4, address = 0, channels = 9 }]
        [zones.strip]
        regions = [{ universe = 4, address = 15, channels = 210 }]
        [[functions]]
        name = "boot_proof"
        kind = "chaser"
        id = 106
        children = [104, 105]
        zones = ["ram", "rog_eye", "strip"]
        composable = false
        [[functions]]
        name = "ambient_ram"
        kind = "chaser"
        id = 109
        children = [107, 108]
        zones = ["ram"]
        composable = true
        [[functions]]
        name = "ambient_eye"
        kind = "chaser"
        id = 112
        children = [110, 111]
        zones = ["rog_eye"]
        composable = true
        [[functions]]
        name = "ambient_strip"
        kind = "chaser"
        id = 115
        children = [113, 114]
        zones = ["strip"]
        composable = true
        [[functions]]
        name = "reference_white_eye"
        kind = "scene"
        id = 116
        children = []
        zones = ["rog_eye"]
        composable = true
        [[ambient_sets]]
        name = "deep_violet"
        functions = ["ambient_ram", "ambient_eye", "ambient_strip"]
        [[progress]]
        name = "progress_ram"
        zone = "ram"
        label = "RAM"
        first_id = 0
        total = 32
        [[progress]]
        name = "progress_strip"
        zone = "strip"
        label = "Strip"
        first_id = 33
        total = 70
    "#;

    fn registry() -> Registry {
        toml::from_str(REGISTRY).unwrap()
    }

    fn config() -> ControllerConfig {
        toml::from_str("version = 1\ndefault_ambient = \"deep_violet\"\nprogress_zones = [\"ram\", \"strip\"]\ncomplete_hold_seconds = 15\n").unwrap()
    }

    /// A gateway status with the given owner per zone.
    fn status(mode: &str, output: &str, owners: [Option<&str>; 3]) -> Value {
        let zone = |owner: Option<&str>| json!({ "owner": owner, "uncertain": false });
        json!({
            "ok": true, "mode": mode, "qlc_reachable": true,
            "output": { "state": output }, "calibration": { "state": "loaded" },
            "zones": { "ram": zone(owners[0]), "rog_eye": zone(owners[1]), "strip": zone(owners[2]) },
        })
    }

    fn healthy(owners: [Option<&str>; 3]) -> Value {
        status("control", "ok", owners)
    }

    const AMBIENT: [Option<&str>; 3] = [Some("ambient_ram"), Some("ambient_eye"), Some("ambient_strip")];

    fn wants_for(controller: &Controller<FakeLink>) -> BTreeMap<String, Want> {
        controller.wants()
    }

    fn plan_for(status: &Value, controller: &Controller<FakeLink>) -> Plan {
        plan(&GatewayView::from_status(status).unwrap(), &wants_for(controller), &controller.registry)
    }

    #[derive(Clone, Default)]
    struct FakeLink {
        status: Arc<Mutex<Value>>,
        calls: Arc<Mutex<Vec<Value>>>,
        refuse: Arc<Mutex<Option<String>>>,
    }

    impl Link for FakeLink {
        fn call(&self, request: Value) -> impl Future<Output = Result<Value, String>> {
            let link = self.clone();
            async move {
                link.calls.lock().unwrap().push(request.clone());
                let op = request["op"].as_str().unwrap_or("").to_owned();
                if op == "status" {
                    return Ok(link.status.lock().unwrap().clone());
                }
                if link.refuse.lock().unwrap().as_deref() == Some(op.as_str()) {
                    return Ok(json!({ "ok": false, "code": "zone_busy", "error": "nope" }));
                }
                Ok(json!({ "ok": true }))
            }
        }
    }

    fn controller_with(link: &FakeLink) -> Controller<FakeLink> {
        Controller::new(link.clone(), registry(), config())
    }

    fn ops(link: &FakeLink) -> Vec<String> {
        link.calls.lock().unwrap().iter().filter(|call| call["op"] != "status").map(|call| call.to_string()).collect()
    }

    // ------------------------------------------------------------ the planner

    #[test]
    fn startup_stops_the_boot_scene_then_starts_the_whole_set_together() {
        let controller = controller_with(&FakeLink::default());
        let plan = plan_for(&healthy([Some("boot_proof"); 3]), &controller);
        assert_eq!(plan.blocked, None);
        assert_eq!(plan.actions, vec![Action::Stop("boot_proof".to_owned()), Action::StartSet(vec!["ambient_ram".to_owned(), "ambient_eye".to_owned(), "ambient_strip".to_owned()])]);
    }

    #[test]
    fn a_dark_machine_gets_the_set_in_one_batch() {
        let controller = controller_with(&FakeLink::default());
        let plan = plan_for(&healthy([None; 3]), &controller);
        assert_eq!(plan.actions, vec![Action::StartSet(vec!["ambient_ram".to_owned(), "ambient_eye".to_owned(), "ambient_strip".to_owned()])]);
    }

    #[test]
    fn nothing_to_do_when_the_plant_already_matches() {
        let controller = controller_with(&FakeLink::default());
        assert_eq!(plan_for(&healthy(AMBIENT), &controller).actions, vec![]);
    }

    #[test]
    fn free_zones_rejoin_a_set_that_is_already_running() {
        let controller = controller_with(&FakeLink::default());
        let plan = plan_for(&healthy([None, Some("ambient_eye"), None]), &controller);
        assert_eq!(plan.actions, vec![Action::Rejoin("ambient_ram".to_owned()), Action::Rejoin("ambient_strip".to_owned())]);
    }

    #[test]
    fn a_leased_zone_shows_its_progress_step_and_the_others_stay_ambient() {
        let link = FakeLink::default();
        let mut controller = controller_with(&link);
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 10, priority: 0 }, Instant::now());
        controller.handle(ControllerRequest::JobProgress { id: "a".into(), completed: 5, total: None }, Instant::now());
        let plan = plan_for(&healthy(AMBIENT), &controller);
        assert_eq!(plan.actions, vec![Action::Progress { zone: "ram".to_owned(), step: 16 }]);
        assert_eq!(plan_for(&healthy([Some("progress_ram:16"), Some("ambient_eye"), Some("ambient_strip")]), &controller).actions, vec![], "already showing it");
        assert_eq!(
            plan_for(&healthy([Some("progress_ram:12"), Some("ambient_eye"), Some("ambient_strip")]), &controller).actions,
            vec![Action::Progress { zone: "ram".to_owned(), step: 16 }],
            "a stale step is replaced"
        );
    }

    #[test]
    fn a_zone_whose_lease_ended_rejoins_the_ambient_in_phase() {
        let controller = controller_with(&FakeLink::default());
        let plan = plan_for(&healthy([Some("progress_ram:32"), Some("ambient_eye"), Some("ambient_strip")]), &controller);
        assert_eq!(plan.actions, vec![Action::Rejoin("ambient_ram".to_owned())]);
    }

    #[test]
    fn startup_with_a_job_already_running_sets_up_everything_at_once() {
        let mut controller = controller_with(&FakeLink::default());
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 4, priority: 0 }, Instant::now());
        let plan = plan_for(&healthy([Some("boot_proof"); 3]), &controller);
        assert_eq!(
            plan.actions,
            vec![Action::Stop("boot_proof".to_owned()), Action::StartSet(vec!["ambient_eye".to_owned(), "ambient_strip".to_owned()]), Action::Progress { zone: "ram".to_owned(), step: 0 }]
        );
    }

    #[test]
    fn a_composable_foreign_owner_is_replaced_and_a_bench_scene_is_stopped_first() {
        let controller = controller_with(&FakeLink::default());
        let plan = plan_for(&healthy([Some("ambient_ram"), Some("reference_white_eye"), Some("ambient_strip")]), &controller);
        assert_eq!(plan.actions, vec![Action::Rejoin("ambient_eye".to_owned())], "no explicit stop needed");
    }

    #[test]
    fn the_planner_waits_instead_of_acting_when_the_plant_is_not_ready() {
        let controller = controller_with(&FakeLink::default());
        for (status, needle) in [
            (status("read_only", "ok", AMBIENT), "not in control mode"),
            (status("control", "recovering", AMBIENT), "output is recovering"),
            (status("control", "reconnecting", AMBIENT), "output is reconnecting"),
            (status("control", "starting", AMBIENT), "output is starting"),
        ] {
            let plan = plan_for(&status, &controller);
            assert!(plan.actions.is_empty(), "{needle}");
            assert!(plan.blocked.unwrap().contains(needle));
        }
        let mut dead = healthy(AMBIENT);
        dead["qlc_reachable"] = json!(false);
        assert!(plan_for(&dead, &controller).blocked.unwrap().contains("QLC+ is not reachable"));
        let mut unsure = healthy(AMBIENT);
        unsure["zones"]["ram"]["uncertain"] = json!(true);
        assert!(plan_for(&unsure, &controller).blocked.unwrap().contains("unconfirmed"));
    }

    // ------------------------------------------------------------ the loop

    #[tokio::test]
    async fn reconciliation_brings_the_lights_up_from_the_boot_scene() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy([Some("boot_proof"); 3]);
        let mut controller = controller_with(&link);
        controller.reconcile(Instant::now()).await;
        assert_eq!(
            ops(&link),
            vec![r#"{"function":"boot_proof","op":"stop"}"#, r#"{"functions":["ambient_ram","ambient_eye","ambient_strip"],"op":"start_set"}"#]
        );
    }

    #[tokio::test]
    async fn a_job_runs_from_lease_to_completion_hold_to_ambient() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy(AMBIENT);
        let mut controller = controller_with(&link);
        let start = Instant::now();

        controller.handle(ControllerRequest::JobStart { id: "backup".into(), label: "Backup".into(), total: 32, priority: 0 }, start);
        controller.handle(ControllerRequest::JobProgress { id: "backup".into(), completed: 8, total: None }, start);
        controller.reconcile(start).await;
        assert_eq!(ops(&link), vec![r#"{"completed":8,"op":"progress","zone":"ram"}"#]);

        // The plant now shows it; a completed job holds at 100%.
        *link.status.lock().unwrap() = healthy([Some("progress_ram:8"), Some("ambient_eye"), Some("ambient_strip")]);
        controller.handle(ControllerRequest::JobComplete { id: "backup".into() }, start);
        controller.reconcile(start + Duration::from_secs(1)).await;
        assert_eq!(ops(&link).last().unwrap(), r#"{"completed":32,"op":"progress","zone":"ram"}"#);

        // After the 15 s hold the zone returns to the ambient set.
        *link.status.lock().unwrap() = healthy([Some("progress_ram:32"), Some("ambient_eye"), Some("ambient_strip")]);
        controller.reconcile(start + Duration::from_secs(16)).await;
        assert_eq!(ops(&link).last().unwrap(), r#"{"function":"ambient_ram","op":"rejoin"}"#);
        assert!(controller.planner.jobs().is_empty());
    }

    #[tokio::test]
    async fn the_controller_re_asserts_state_after_a_stack_restart() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy(AMBIENT);
        let mut controller = controller_with(&link);
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 10, priority: 0 }, Instant::now());
        controller.handle(ControllerRequest::JobProgress { id: "a".into(), completed: 5, total: None }, Instant::now());
        // The stack restarts: the gateway comes back with nothing running.
        *link.status.lock().unwrap() = healthy([None; 3]);
        controller.reconcile(Instant::now()).await;
        assert_eq!(
            ops(&link),
            vec![r#"{"functions":["ambient_eye","ambient_strip"],"op":"start_set"}"#, r#"{"completed":16,"op":"progress","zone":"ram"}"#]
        );
    }

    #[tokio::test]
    async fn a_paused_controller_leaves_the_plant_alone() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy([None; 3]);
        let mut controller = controller_with(&link);
        controller.handle(ControllerRequest::Pause, Instant::now());
        controller.reconcile(Instant::now()).await;
        assert!(link.calls.lock().unwrap().is_empty(), "not even a status read");
        controller.handle(ControllerRequest::Resume, Instant::now());
        controller.reconcile(Instant::now()).await;
        assert_eq!(ops(&link).len(), 1);
    }

    #[tokio::test]
    async fn a_refused_action_stops_the_batch_and_is_retried_next_time() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy([Some("boot_proof"); 3]);
        *link.refuse.lock().unwrap() = Some("stop".to_owned());
        let mut controller = controller_with(&link);
        controller.reconcile(Instant::now()).await;
        assert_eq!(ops(&link).len(), 1, "the start_set must not be attempted after a refused stop");
        assert!(controller.report.recent.back().unwrap().contains("refused"));
        *link.refuse.lock().unwrap() = None;
        controller.reconcile(Instant::now()).await;
        assert_eq!(ops(&link).len(), 3, "the retry sends the stop and the start_set");
    }

    #[tokio::test]
    async fn an_unreachable_gateway_is_reported_and_not_fatal() {
        struct Down;
        impl Link for Down {
            fn call(&self, _: Value) -> impl Future<Output = Result<Value, String>> {
                async { Err("connect: refused".to_owned()) }
            }
        }
        let mut controller = Controller::new(Down, registry(), config());
        controller.reconcile(Instant::now()).await;
        assert!(controller.report.blocked.as_deref().unwrap().contains("unreachable"));
        assert_eq!(controller.status_json()["waiting"].as_str().unwrap().contains("unreachable"), true);
    }

    // ------------------------------------------------------------ requests

    #[test]
    fn requests_report_the_lease_and_reject_nonsense() {
        let mut controller = controller_with(&FakeLink::default());
        let now = Instant::now();
        let reply = controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "Backup".into(), total: 10, priority: 0 }, now);
        assert_eq!((reply["ok"].clone(), reply["job"]["state"].clone(), reply["job"]["zone"].clone()), (json!(true), json!("leased"), json!("ram")));
        controller.handle(ControllerRequest::JobStart { id: "b".into(), label: "x".into(), total: 10, priority: 0 }, now);
        let third = controller.handle(ControllerRequest::JobStart { id: "c".into(), label: "x".into(), total: 10, priority: 0 }, now);
        assert_eq!(third["job"]["state"], "queued");

        let unknown = controller.handle(ControllerRequest::JobProgress { id: "zzz".into(), completed: 1, total: None }, now);
        assert_eq!((unknown["ok"].clone(), unknown["code"].clone()), (json!(false), json!("unknown_job")));
        assert_eq!(controller.handle(ControllerRequest::JobStart { id: "z".into(), label: "x".into(), total: 0, priority: 0 }, now)["code"], "bad_request");
        assert_eq!(controller.handle(ControllerRequest::AmbientSelect { set: "nope".into() }, now)["code"], "unknown_set");
        assert_eq!(controller.handle(ControllerRequest::AmbientSelect { set: "deep_violet".into() }, now)["ok"], true);

        let failed = controller.handle(ControllerRequest::JobFail { id: "a".into(), reason: "disk full".into() }, now);
        assert_eq!(failed["ok"], true);
        let status = controller.status_json();
        assert_eq!(status["failures"][0]["reason"], "disk full");
        assert_eq!(status["jobs"].as_array().unwrap().len(), 2, "b and c remain; c has moved up");
    }

    #[test]
    fn requests_are_parsed_strictly() {
        let parse = |text: &str| serde_json::from_str::<ControllerRequest>(text);
        assert_eq!(parse(r#"{"op":"status"}"#).unwrap(), ControllerRequest::Status);
        assert_eq!(
            parse(r#"{"op":"job.start","id":"a","label":"x","total":3}"#).unwrap(),
            ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 3, priority: 0 }
        );
        assert_eq!(parse(r#"{"op":"ambient.select","set":"deep_violet"}"#).unwrap(), ControllerRequest::AmbientSelect { set: "deep_violet".into() });
        assert!(parse(r#"{"op":"job.start","id":"a","label":"x"}"#).is_err(), "a job needs a total");
        assert!(parse(r#"{"op":"job.progress","id":"a","completed":-1}"#).is_err());
        // Requests that change anything reject unknown fields (unit variants such as status do not: a serde quirk).
        assert!(parse(r#"{"op":"job.complete","id":"a","extra":1}"#).is_err());
        assert!(parse(r#"{"op":"ambient.select","set":"a","extra":1}"#).is_err());
        assert!(parse(r#"{"op":"suspend"}"#).is_err());
    }

    #[test]
    fn builds_client_requests_from_arguments() {
        let words = |text: &str| text.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(client_request(&words("job-start a Backup 10 2")).unwrap(), json!({ "op": "job.start", "id": "a", "label": "Backup", "total": 10, "priority": 2 }));
        assert_eq!(client_request(&words("job-progress a 4 8")).unwrap(), json!({ "op": "job.progress", "id": "a", "completed": 4, "total": 8 }));
        assert_eq!(client_request(&words("job-fail a disk is full")).unwrap(), json!({ "op": "job.fail", "id": "a", "reason": "disk is full" }));
        assert_eq!(client_request(&words("ambient deep_violet")).unwrap(), json!({ "op": "ambient.select", "set": "deep_violet" }));
        assert!(client_request(&words("job-start a Backup")).is_err());
        assert!(client_request(&words("job-fail a")).is_err());
        assert!(client_request(&words("job-progress a many")).is_err());
    }

    // ------------------------------------------------------------ configuration

    #[test]
    fn the_shipped_controller_policy_is_valid_against_the_real_registry() {
        let root = root();
        let layout = crate::config::load_layout(&root.join("scene-layout.toml")).unwrap();
        let (registry, problems) = registry::load_and_validate(&root.join("qlc-functions.toml"), &layout).unwrap();
        assert_eq!(problems, Vec::<String>::new());
        let config = load_config(&root.join("controller.toml")).unwrap();
        assert_eq!(check_config(&config, &registry), Vec::<String>::new());
        assert_eq!((config.default_ambient.as_str(), config.complete_hold_seconds), ("deep_violet", 15));
    }

    #[test]
    fn bad_policy_is_reported() {
        let registry = registry();
        let bad = |text: &str| check_config(&toml::from_str::<ControllerConfig>(text).unwrap(), &registry).join("; ");
        let base = "version = 1\ndefault_ambient = \"deep_violet\"\nprogress_zones = [\"ram\"]\ncomplete_hold_seconds = 15\n";
        assert_eq!(bad(base), "");
        assert!(bad(&base.replace("deep_violet", "nope")).contains("not an ambient set"));
        assert!(bad(&base.replace("[\"ram\"]", "[\"rog_eye\"]")).contains("no progress family"));
        assert!(bad(&base.replace("[\"ram\"]", "[]")).contains("empty"));
        assert!(bad(&base.replace("[\"ram\"]", "[\"ram\", \"ram\"]")).contains("twice"));
        assert!(bad(&base.replace("= 15", "= 99999")).contains("at most 3600"));
        assert!(bad(&base.replace("version = 1", "version = 2")).contains("unsupported"));
        assert!(toml::from_str::<ControllerConfig>(&format!("{base}surprise = 1\n")).is_err());
    }

    #[test]
    fn the_control_socket_serves_json_lines_for_the_owner_only() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let directory = std::env::temp_dir().join(format!("monolithd-controller-test-{}", std::process::id()));
            let path = directory.join("monolith-events").join("controller.sock");
            let listener = gateway::prepare_socket(&path).unwrap();
            let uid = std::fs::metadata(&path).unwrap().uid();
            let (requests, mut incoming) = mpsc::channel::<Envelope>(4);
            tokio::spawn(serve(listener, uid, requests));
            tokio::spawn(async move {
                while let Some((request, reply)) = incoming.recv().await {
                    let _ = reply.send(json!({ "ok": true, "saw": format!("{request:?}") }));
                }
            });
            let mut stream = UnixStream::connect(&path).await.unwrap();
            stream.write_all(b"{\"op\":\"pause\"}\nnot json\n{\"op\":\"status\"}\n").await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            let first: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let second: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let third: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(first["saw"], "Pause");
            assert_eq!(second["code"], "bad_request");
            assert_eq!(third["saw"], "Status");
            let _ = std::fs::remove_dir_all(&directory);
        });
    }
}
