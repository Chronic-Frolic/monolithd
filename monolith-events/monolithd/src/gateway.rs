//! Scene gateway: the only component that drives QLC+ Functions.
//!
//! It runs inside the lighting stack's private network namespace and exposes a
//! narrow, owner-only Unix socket so a host-side controller can request scenes
//! by semantic name. The gateway tracks one owner per zone, refuses conflicting
//! requests, and updates its table only from QLC+'s own confirmed status.
//!
//! Requests on different zones never wait for each other: every zone has its
//! own lock, and an operation takes the locks of the zones it touches in a
//! fixed (alphabetical) order.

use crate::config::Layout;
use crate::qlc::{self, FunctionStatus};
use crate::registry::{self, Registry};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::{sleep, timeout};

const MAX_REQUEST_BYTES: usize = 4096;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(20);
/// QLC+ applies commands within a few milliseconds, so confirmation polls fast.
const CONFIRM_POLLS: usize = 400;
const CONFIRM_INTERVAL: Duration = Duration::from_millis(5);
const RESTART_DELAY: Duration = Duration::from_secs(2);
const DEFAULT_OVERLAP: Duration = Duration::from_millis(40);
const MAX_OVERLAP: Duration = Duration::from_millis(1000);

/// One start (`true`) or stop (`false`) of a QLC+ Function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    pub id: u32,
    pub running: bool,
}

/// The QLC+ operations the gateway needs. A trait so tests can substitute a fake.
pub trait Qlc: Send + Sync {
    /// Deliver every command together, in order.
    fn send(&self, commands: &[Command]) -> impl Future<Output = Result<(), String>> + Send;
    fn status(&self, id: u32) -> impl Future<Output = Result<FunctionStatus, String>> + Send;
}

impl Qlc for qlc::Client {
    async fn send(&self, commands: &[Command]) -> Result<(), String> {
        let commands: Vec<(u32, bool)> = commands.iter().map(|command| (command.id, command.running)).collect();
        self.send_batch(&commands).await
    }
    async fn status(&self, id: u32) -> Result<FunctionStatus, String> { qlc::Client::status(self, id).await }
}

/// How an owner is replaced by its successor on the same zone.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    /// Stop, confirm, then start and confirm. The old behaviour, with fast polling.
    Sequential,
    /// Send stop and start together in one batch, then confirm both.
    #[default]
    Pipelined,
    /// Start the successor first, keep both running briefly, then stop the
    /// old owner: QLC+ merges the overlap instead of showing a black gap.
    Overlap,
}

impl Transition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Pipelined => "pipelined",
            Self::Overlap => "overlap",
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Status,
    Start { function: String },
    /// Start several functions on free, disjoint zones so they stay in phase.
    StartSet { functions: Vec<String> },
    Replace {
        function: String,
        #[serde(default)]
        transition: Transition,
        overlap_ms: Option<u64>,
    },
    Stop { function: String },
    Progress {
        zone: String,
        completed: u32,
        #[serde(default)]
        transition: Transition,
        overlap_ms: Option<u64>,
    },
}

/// A registered Function (or one progress step) and the zones it drives.
#[derive(Clone, Debug, PartialEq)]
struct Target {
    name: String,
    id: u32,
    zones: Vec<String>,
    composable: bool,
}

#[derive(Default)]
struct ZoneState {
    owner: Option<Target>,
    /// QLC+ did not confirm the last change; commands on this zone are refused
    /// until `status` re-syncs it.
    uncertain: bool,
}

type Held<'a> = BTreeMap<String, MutexGuard<'a, ZoneState>>;

struct Failure {
    code: &'static str,
    message: String,
}

fn fail<T>(code: &'static str, message: impl Into<String>) -> Result<T, Failure> {
    Err(Failure { code, message: message.into() })
}

pub struct Gateway<Q> {
    registry: Option<Registry>,
    problems: Vec<String>,
    qlc: Q,
    zones: BTreeMap<String, Mutex<ZoneState>>,
    confirm_polls: usize,
    confirm_interval: Duration,
}

impl<Q: Qlc> Gateway<Q> {
    /// `problems` non-empty (or no registry) puts the gateway in read-only mode.
    pub fn new(registry: Option<Registry>, problems: Vec<String>, qlc: Q) -> Self {
        let zones = registry
            .iter()
            .flat_map(|registry| registry.zones.keys())
            .map(|name| (name.clone(), Mutex::new(ZoneState::default())))
            .collect();
        Self { registry, problems, qlc, zones, confirm_polls: CONFIRM_POLLS, confirm_interval: CONFIRM_INTERVAL }
    }

    #[cfg(test)]
    fn with_confirmation(mut self, polls: usize, interval: Duration) -> Self {
        self.confirm_polls = polls;
        self.confirm_interval = interval;
        self
    }

    /// Lock the named zones in alphabetical order (deadlock-free by construction).
    async fn lock_zones<'a>(&'a self, names: &[String]) -> Result<Held<'a>, Failure> {
        let ordered: BTreeSet<&String> = names.iter().collect();
        let mut held = Held::new();
        for name in ordered {
            let Some(zone) = self.zones.get(name) else { return fail("unknown_zone", format!("unknown zone {name}")) };
            held.insert(name.clone(), zone.lock().await);
        }
        Ok(held)
    }

    /// Record that the stack started `function_id` itself at boot.
    pub async fn seed(&self, function_id: u32) {
        let Some(registry) = &self.registry else { return };
        let Some(target) = target_for_id(registry, function_id) else {
            eprintln!("monolithd gateway: boot Function {function_id} is not in the registry; zone table starts empty");
            return;
        };
        if let Ok(mut held) = self.lock_zones(&target.zones).await {
            for zone in held.values_mut() {
                zone.owner = Some(target.clone());
            }
        }
    }

    pub async fn handle(&self, request: Request) -> Value {
        let outcome = match request {
            Request::Status => return self.status().await,
            other => self.mutate(other).await,
        };
        match outcome {
            Ok(value) => value,
            Err(Failure { code, message }) => json!({ "ok": false, "code": code, "error": message }),
        }
    }

    fn control(&self) -> Result<&Registry, Failure> {
        match (&self.registry, self.problems.is_empty()) {
            (Some(registry), true) => Ok(registry),
            _ => fail(
                "read_only",
                format!(
                    "gateway is read-only because the registry could not be trusted: {}",
                    self.problems.first().map(String::as_str).unwrap_or("registry unavailable")
                ),
            ),
        }
    }

    async fn mutate(&self, request: Request) -> Result<Value, Failure> {
        let registry = self.control()?;
        match request {
            Request::Start { function } => {
                let target = resolve_function(registry, &function)?;
                self.activate(&target, false, Transition::default(), DEFAULT_OVERLAP).await
            }
            Request::StartSet { functions } => {
                let targets = functions.iter().map(|name| resolve_function(registry, name)).collect::<Result<Vec<_>, _>>()?;
                self.start_set(&targets).await
            }
            Request::Replace { function, transition, overlap_ms } => {
                let target = resolve_function(registry, &function)?;
                self.activate(&target, true, transition, overlap(overlap_ms)?).await
            }
            Request::Progress { zone, completed, transition, overlap_ms } => {
                let target = resolve_progress(registry, &zone, completed)?;
                self.activate(&target, true, transition, overlap(overlap_ms)?).await
            }
            Request::Stop { function } => {
                let target = resolve_function(registry, &function)?;
                self.stop(&target).await
            }
            Request::Status => unreachable!("status is handled before mutation"),
        }
    }

    /// Wait until QLC+ reports every commanded state, polling all of them each round.
    async fn confirm(&self, commands: &[Command]) -> Result<(), String> {
        let mut pending: Vec<Command> = commands.to_vec();
        let mut last = String::from("no status reply");
        for attempt in 0..self.confirm_polls {
            if attempt > 0 {
                sleep(self.confirm_interval).await;
            }
            let mut still = Vec::new();
            for command in &pending {
                let want = if command.running { FunctionStatus::Running } else { FunctionStatus::Stopped };
                match self.qlc.status(command.id).await {
                    Ok(status) if status == want => {}
                    Ok(status) => {
                        last = format!("Function {} reports {}", command.id, status.as_str());
                        still.push(*command);
                    }
                    Err(error) => {
                        last = error;
                        still.push(*command);
                    }
                }
            }
            if still.is_empty() {
                return Ok(());
            }
            pending = still;
        }
        Err(format!("QLC+ did not confirm the change: {last}"))
    }

    async fn send_confirmed(&self, commands: &[Command]) -> Result<(), String> {
        self.qlc.send(commands).await?;
        self.confirm(commands).await
    }

    fn mark_uncertain(held: &mut Held<'_>, zones: &[String]) {
        for zone in zones {
            if let Some(zone) = held.get_mut(zone) {
                zone.uncertain = true;
                zone.owner = None;
            }
        }
    }

    fn check_free_of_uncertainty(held: &Held<'_>) -> Result<(), Failure> {
        match held.iter().find(|(_, zone)| zone.uncertain) {
            Some((name, _)) => fail("zone_uncertain", format!("zone {name} is in an unconfirmed state; run `status` to re-sync it")),
            None => Ok(()),
        }
    }

    async fn activate(&self, target: &Target, replace: bool, transition: Transition, overlap: Duration) -> Result<Value, Failure> {
        let mut held = self.lock_zones(&target.zones).await?;
        Self::check_free_of_uncertainty(&held)?;

        let mut owners: Vec<Target> = Vec::new();
        for zone in held.values() {
            if let Some(owner) = &zone.owner {
                if owner.id != target.id && !owners.iter().any(|known| known.id == owner.id) {
                    owners.push(owner.clone());
                }
            }
        }
        let already = held.values().all(|zone| zone.owner.as_ref().is_some_and(|owner| owner.id == target.id));
        if already && owners.is_empty() {
            return Ok(json!({ "ok": true, "changed": false, "function": target.name, "id": target.id, "zones": target.zones }));
        }

        if !owners.is_empty() {
            if !replace {
                let names: Vec<&str> = owners.iter().map(|owner| owner.name.as_str()).collect();
                return fail("zone_busy", format!("zones of {} are owned by {}; use replace, or stop the owner", target.name, names.join(", ")));
            }
            if let Some(owner) = owners.iter().find(|owner| !owner.composable) {
                return fail(
                    "explicit_stop_required",
                    format!("{} is not composable and drives {:?}; stop it explicitly first", owner.name, owner.zones),
                );
            }
        }

        let stops: Vec<Command> = owners.iter().map(|owner| Command { id: owner.id, running: false }).collect();
        let start = Command { id: target.id, running: true };
        let outcome = if stops.is_empty() {
            self.send_confirmed(&[start]).await
        } else {
            match transition {
                Transition::Sequential => match self.send_confirmed(&stops).await {
                    Ok(()) => self.send_confirmed(&[start]).await,
                    Err(error) => Err(error),
                },
                Transition::Pipelined => {
                    let all: Vec<Command> = stops.iter().copied().chain([start]).collect();
                    self.send_confirmed(&all).await
                }
                Transition::Overlap => match self.qlc.send(&[start]).await {
                    Ok(()) => {
                        sleep(overlap).await;
                        let all: Vec<Command> = stops.iter().copied().chain([start]).collect();
                        match self.qlc.send(&stops).await {
                            Ok(()) => self.confirm(&all).await,
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                },
            }
        };
        if let Err(error) = outcome {
            Self::mark_uncertain(&mut held, &target.zones);
            return fail("qlc_unconfirmed", format!("switching to {}: {error}", target.name));
        }
        for zone in held.values_mut() {
            zone.owner = Some(target.clone());
        }
        let released: Vec<&str> = owners.iter().map(|owner| owner.name.as_str()).collect();
        Ok(json!({
            "ok": true, "changed": true, "function": target.name, "id": target.id, "zones": target.zones,
            "released": released, "transition": if owners.is_empty() { Value::Null } else { json!(transition.as_str()) },
        }))
    }

    /// Start several functions together: every command goes out in one batch,
    /// so their chasers begin in the same QLC+ pass and stay in phase.
    async fn start_set(&self, targets: &[Target]) -> Result<Value, Failure> {
        if targets.is_empty() {
            return fail("bad_request", "start_set needs at least one function");
        }
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for target in targets {
            for zone in &target.zones {
                if let Some(other) = seen.insert(zone.as_str(), target.name.as_str()) {
                    return fail("overlapping_set", format!("{} and {} both drive zone {zone}", other, target.name));
                }
            }
        }
        let names: Vec<String> = targets.iter().flat_map(|target| target.zones.iter().cloned()).collect();
        let mut held = self.lock_zones(&names).await?;
        Self::check_free_of_uncertainty(&held)?;

        let mut to_start = Vec::new();
        for target in targets {
            let owned_by_target = target.zones.iter().all(|zone| held[zone].owner.as_ref().is_some_and(|owner| owner.id == target.id));
            if owned_by_target {
                continue;
            }
            if let Some(owner) = target.zones.iter().find_map(|zone| held[zone].owner.as_ref()) {
                return fail("zone_busy", format!("zones of {} are owned by {}", target.name, owner.name));
            }
            to_start.push(target);
        }
        if !to_start.is_empty() {
            let commands: Vec<Command> = to_start.iter().map(|target| Command { id: target.id, running: true }).collect();
            if let Err(error) = self.send_confirmed(&commands).await {
                for target in &to_start {
                    Self::mark_uncertain(&mut held, &target.zones);
                }
                return fail("qlc_unconfirmed", format!("starting the set: {error}"));
            }
            for target in &to_start {
                for zone in &target.zones {
                    if let Some(entry) = held.get_mut(zone) {
                        entry.owner = Some((*target).clone());
                    }
                }
            }
        }
        let started: Vec<&str> = to_start.iter().map(|target| target.name.as_str()).collect();
        Ok(json!({ "ok": true, "changed": !to_start.is_empty(), "started": started }))
    }

    async fn stop(&self, target: &Target) -> Result<Value, Failure> {
        let mut held = self.lock_zones(&target.zones).await?;
        Self::check_free_of_uncertainty(&held)?;
        if let Err(error) = self.send_confirmed(&[Command { id: target.id, running: false }]).await {
            Self::mark_uncertain(&mut held, &target.zones);
            return fail("qlc_unconfirmed", format!("stopping {}: {error}", target.name));
        }
        let mut changed = false;
        for zone in held.values_mut() {
            if zone.owner.as_ref().is_some_and(|owner| owner.id == target.id) {
                zone.owner = None;
                changed = true;
            }
        }
        Ok(json!({ "ok": true, "changed": changed, "function": target.name, "id": target.id }))
    }

    /// Re-derive an uncertain zone's owner from what QLC+ says is running.
    async fn resync(&self, registry: &Registry, held: &mut Held<'_>, zone: &str) {
        let mut candidates: Vec<u32> = registry
            .functions
            .iter()
            .filter(|entry| entry.zones.iter().any(|z| z == zone))
            .map(|entry| entry.id)
            .collect();
        if let Some(progress) = registry.progress_for_zone(zone) {
            candidates.extend(progress.first_id..=progress.first_id + progress.total);
        }
        let mut running = Vec::new();
        for id in candidates {
            match self.qlc.status(id).await {
                Ok(FunctionStatus::Running) => running.push(id),
                Ok(_) => {}
                Err(_) => return, // cannot tell; stay uncertain
            }
        }
        match running.as_slice() {
            [] => {
                if let Some(entry) = held.get_mut(zone) {
                    entry.uncertain = false;
                    entry.owner = None;
                }
            }
            [id] => {
                if let Some(target) = target_for_id(registry, *id) {
                    for z in &target.zones {
                        if let Some(entry) = held.get_mut(z) {
                            entry.uncertain = false;
                            entry.owner = Some(target.clone());
                        }
                    }
                }
            }
            _ => {} // several candidates running: leave it flagged for a human
        }
    }

    async fn status(&self) -> Value {
        let all: Vec<String> = self.zones.keys().cloned().collect();
        let mut held = self.lock_zones(&all).await.unwrap_or_default();
        let mode = if self.control().is_ok() { "control" } else { "read_only" };
        if let (Some(registry), "control") = (&self.registry, mode) {
            let uncertain: Vec<String> = held.iter().filter(|(_, zone)| zone.uncertain).map(|(name, _)| name.clone()).collect();
            for zone in uncertain {
                self.resync(registry, &mut held, &zone).await;
            }
        }
        let mut reachable = true;
        let mut zones = Map::new();
        for (name, zone) in &held {
            let (owner, id, qlc) = match &zone.owner {
                Some(owner) => {
                    let report = match self.qlc.status(owner.id).await {
                        Ok(status) => status.as_str().to_owned(),
                        Err(error) => {
                            reachable = false;
                            format!("unreachable: {error}")
                        }
                    };
                    (Value::from(owner.name.clone()), Value::from(owner.id), Value::from(report))
                }
                None => (Value::Null, Value::Null, Value::Null),
            };
            zones.insert(name.clone(), json!({ "owner": owner, "function_id": id, "qlc": qlc, "uncertain": zone.uncertain }));
        }
        let problems: Vec<&String> = self.problems.iter().take(10).collect();
        json!({
            "ok": true, "mode": mode, "qlc_reachable": reachable, "default_transition": Transition::default().as_str(),
            "problem_count": self.problems.len(), "problems": problems, "zones": zones,
        })
    }
}

fn overlap(requested: Option<u64>) -> Result<Duration, Failure> {
    match requested.map(Duration::from_millis) {
        None => Ok(DEFAULT_OVERLAP),
        Some(duration) if duration <= MAX_OVERLAP => Ok(duration),
        Some(_) => fail("out_of_range", format!("overlap_ms must be at most {}", MAX_OVERLAP.as_millis())),
    }
}

fn resolve_function(registry: &Registry, name: &str) -> Result<Target, Failure> {
    match registry.function(name) {
        Some(entry) => Ok(Target { name: entry.name.clone(), id: entry.id, zones: entry.zones.clone(), composable: entry.composable }),
        None => fail("unknown_function", format!("{name} is not a registered function")),
    }
}

fn resolve_progress(registry: &Registry, zone: &str, completed: u32) -> Result<Target, Failure> {
    let Some(progress) = registry.progress_for_zone(zone) else {
        return fail("unknown_zone", format!("zone {zone} has no progress family"));
    };
    if completed > progress.total {
        return fail("out_of_range", format!("completed must be 0 through {}, not {completed}", progress.total));
    }
    Ok(Target {
        name: format!("{}:{completed}", progress.name),
        id: progress.first_id + completed,
        zones: vec![zone.to_owned()],
        composable: true,
    })
}

/// Describe whatever the registry knows under a Function ID.
fn target_for_id(registry: &Registry, id: u32) -> Option<Target> {
    if let Some(entry) = registry.function_by_id(id) {
        return Some(Target { name: entry.name.clone(), id, zones: entry.zones.clone(), composable: entry.composable });
    }
    registry
        .progress
        .iter()
        .find(|progress| id >= progress.first_id && id <= progress.first_id + progress.total)
        .map(|progress| Target { name: format!("{}:{}", progress.name, id - progress.first_id), id, zones: vec![progress.zone.clone()], composable: true })
}

pub fn socket_path() -> Result<PathBuf, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(runtime).join("monolith-events").join("gateway.sock"))
}

/// Create the socket directory (0700) if needed, clear a dead socket, bind, and lock to 0600.
fn prepare_socket(path: &Path) -> Result<UnixListener, String> {
    let directory = path.parent().ok_or("gateway socket path has no parent")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .map_err(|error| format!("create {}: {error}", directory.display()))?;
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(format!("another gateway is already listening on {}", path.display()));
        }
        std::fs::remove_file(path).map_err(|error| format!("remove stale {}: {error}", path.display()))?;
    }
    let listener = UnixListener::bind(path).map_err(|error| format!("bind {}: {error}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod {}: {error}", path.display()))?;
    Ok(listener)
}

async fn serve<Q: Qlc + 'static>(gateway: Arc<Gateway<Q>>, listener: UnixListener, owner_uid: u32) -> Result<(), String> {
    loop {
        let (stream, _) = listener.accept().await.map_err(|error| format!("accept: {error}"))?;
        let gateway = gateway.clone();
        tokio::spawn(async move {
            if let Err(error) = connection(gateway, stream, owner_uid).await {
                eprintln!("monolithd gateway: connection dropped: {error}");
            }
        });
    }
}

async fn connection<Q: Qlc>(gateway: Arc<Gateway<Q>>, stream: UnixStream, owner_uid: u32) -> Result<(), String> {
    let peer = stream.peer_cred().map_err(|error| format!("peer credentials: {error}"))?;
    if peer.uid() != owner_uid {
        return Err(format!("rejected peer with uid {}", peer.uid()));
    }
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = Vec::new();
        let read = timeout(IDLE_TIMEOUT, (&mut reader).take(MAX_REQUEST_BYTES as u64).read_until(b'\n', &mut line)).await;
        match read {
            Err(_) | Ok(Ok(0)) => return Ok(()),
            Ok(Err(error)) => return Err(format!("read request: {error}")),
            Ok(Ok(_)) => {}
        }
        if line.last() != Some(&b'\n') {
            let reply = json!({ "ok": false, "code": "request_too_large", "error": format!("requests are limited to {MAX_REQUEST_BYTES} bytes and end with a newline") });
            let _ = writer.write_all(format!("{reply}\n").as_bytes()).await;
            return Ok(());
        }
        let reply = match serde_json::from_slice::<Request>(&line) {
            Ok(request) => gateway.handle(request).await,
            Err(error) => json!({ "ok": false, "code": "bad_request", "error": error.to_string() }),
        };
        writer
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .map_err(|error| format!("write reply: {error}"))?;
    }
}

/// Serve forever. A gateway fault is logged and retried; it never affects rendering.
pub async fn run<Q: Qlc + 'static>(gateway: Arc<Gateway<Q>>) {
    loop {
        let result: Result<(), String> = async {
            let path = socket_path()?;
            let listener = prepare_socket(&path)?;
            let uid = std::fs::metadata(&path).map_err(|error| format!("stat {}: {error}", path.display()))?.uid();
            eprintln!("monolithd gateway: listening on {}", path.display());
            serve(gateway.clone(), listener, uid).await
        }
        .await;
        if let Err(error) = result {
            eprintln!("monolithd gateway: {error}; retrying in {} s", RESTART_DELAY.as_secs());
        }
        sleep(RESTART_DELAY).await;
    }
}

/// Load and check the registry, then serve the gateway for the production stack.
pub async fn spawn_for_stack(layout: &Layout, registry_path: &Path, boot_function: u32) {
    let (registry, problems) = match registry::load_and_validate(registry_path, layout) {
        Ok((registry, problems)) => (Some(registry), problems),
        Err(error) => (None, vec![error]),
    };
    if problems.is_empty() {
        eprintln!("monolithd gateway: registry verified against the QLC+ workspace; control mode");
    } else {
        eprintln!("monolithd gateway: READ-ONLY, {} registry problem(s); first: {}", problems.len(), problems[0]);
    }
    let gateway = Arc::new(Gateway::new(registry, problems, qlc::Client::new(&layout.qlc_e131.web_listener)));
    gateway.seed(boot_function).await;
    tokio::spawn(run(gateway));
}

const USAGE: &str = "usage: monolithd scene <status | start NAME | start-set NAME... | stop NAME | replace NAME [FLAGS] | progress ZONE COMPLETED [FLAGS]>\n  FLAGS: --transition sequential|pipelined|overlap   --overlap-ms N";

/// Build the JSON request for `monolithd scene ...` from its arguments.
fn client_request(arguments: &[String]) -> Result<Value, String> {
    let mut positional: Vec<&str> = Vec::new();
    let mut transition: Option<&str> = None;
    let mut overlap_ms: Option<u64> = None;
    let mut rest = arguments.iter().map(String::as_str);
    while let Some(word) = rest.next() {
        match word {
            "--transition" => transition = Some(rest.next().ok_or("--transition needs a value")?),
            "--overlap-ms" => {
                let value = rest.next().ok_or("--overlap-ms needs a value")?;
                overlap_ms = Some(value.parse().map_err(|_| format!("--overlap-ms must be an integer, not {value:?}"))?);
            }
            other => positional.push(other),
        }
    }
    let mut request = match positional.as_slice() {
        ["status"] => json!({ "op": "status" }),
        ["start", name] => json!({ "op": "start", "function": name }),
        ["start-set", names @ ..] if !names.is_empty() => json!({ "op": "start_set", "functions": names }),
        ["stop", name] => json!({ "op": "stop", "function": name }),
        ["replace", name] => json!({ "op": "replace", "function": name }),
        ["progress", zone, completed] => {
            let completed: u32 = completed.parse().map_err(|_| format!("COMPLETED must be an integer, not {completed:?}"))?;
            json!({ "op": "progress", "zone": zone, "completed": completed })
        }
        _ => return Err(USAGE.to_owned()),
    };
    if (transition.is_some() || overlap_ms.is_some()) && !matches!(request["op"].as_str(), Some("replace" | "progress")) {
        return Err("--transition and --overlap-ms apply only to replace and progress".to_owned());
    }
    if let Some(transition) = transition {
        request["transition"] = json!(transition);
    }
    if let Some(overlap_ms) = overlap_ms {
        request["overlap_ms"] = json!(overlap_ms);
    }
    Ok(request)
}

/// `monolithd scene ...`: a one-shot client for the gateway socket.
pub async fn client(arguments: Vec<String>) -> Result<(), String> {
    let request = client_request(&arguments)?;
    let path = socket_path()?;
    let exchange = async {
        let mut stream = UnixStream::connect(&path).await.map_err(|error| format!("connect {}: {error}", path.display()))?;
        stream.write_all(format!("{request}\n").as_bytes()).await.map_err(|error| format!("send request: {error}"))?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).await.map_err(|error| format!("read reply: {error}"))?;
        Ok::<String, String>(reply)
    };
    let reply = timeout(CLIENT_TIMEOUT, exchange).await.map_err(|_| "gateway did not answer in time".to_owned())??;
    let reply: Value = serde_json::from_str(&reply).map_err(|error| format!("gateway sent invalid JSON: {error}"))?;
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
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

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

    #[derive(Clone, Default)]
    struct Fake {
        running: Arc<StdMutex<std::collections::BTreeSet<u32>>>,
        /// Every command, in the order QLC+ would apply it.
        calls: Arc<StdMutex<Vec<String>>>,
        /// The same commands grouped as they were delivered.
        batches: Arc<StdMutex<Vec<Vec<String>>>>,
        /// Functions whose start QLC+ silently ignores.
        stuck: Arc<StdMutex<std::collections::BTreeSet<u32>>>,
        /// Extra latency before commands touching a Function are applied.
        delays: Arc<StdMutex<BTreeMap<u32, Duration>>>,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> { self.calls.lock().unwrap().clone() }
        fn batches(&self) -> Vec<Vec<String>> { self.batches.lock().unwrap().clone() }
        fn running(&self) -> Vec<u32> { self.running.lock().unwrap().iter().copied().collect() }
    }

    fn label(command: &Command) -> String {
        format!("{} {}", if command.running { "start" } else { "stop" }, command.id)
    }

    impl Qlc for Fake {
        async fn send(&self, commands: &[Command]) -> Result<(), String> {
            let delay = {
                let delays = self.delays.lock().unwrap();
                commands.iter().filter_map(|command| delays.get(&command.id).copied()).max()
            };
            if let Some(delay) = delay {
                sleep(delay).await;
            }
            self.batches.lock().unwrap().push(commands.iter().map(label).collect());
            for command in commands {
                self.calls.lock().unwrap().push(label(command));
                if command.running {
                    if !self.stuck.lock().unwrap().contains(&command.id) {
                        self.running.lock().unwrap().insert(command.id);
                    }
                } else {
                    self.running.lock().unwrap().remove(&command.id);
                }
            }
            Ok(())
        }
        async fn status(&self, id: u32) -> Result<FunctionStatus, String> {
            Ok(if self.running.lock().unwrap().contains(&id) { FunctionStatus::Running } else { FunctionStatus::Stopped })
        }
    }

    fn gateway(fake: &Fake) -> Gateway<Fake> {
        let registry: Registry = toml::from_str(REGISTRY).unwrap();
        Gateway::new(Some(registry), Vec::new(), fake.clone()).with_confirmation(3, Duration::from_millis(1))
    }

    fn start(function: &str) -> Request { Request::Start { function: function.to_owned() } }
    fn start_set(functions: &[&str]) -> Request { Request::StartSet { functions: functions.iter().map(|f| (*f).to_owned()).collect() } }
    fn replace(function: &str) -> Request { Request::Replace { function: function.to_owned(), transition: Transition::default(), overlap_ms: None } }
    fn stop(function: &str) -> Request { Request::Stop { function: function.to_owned() } }
    fn progress(zone: &str, completed: u32) -> Request { Request::Progress { zone: zone.to_owned(), completed, transition: Transition::default(), overlap_ms: None } }
    fn progress_with(zone: &str, completed: u32, transition: Transition, overlap_ms: Option<u64>) -> Request {
        Request::Progress { zone: zone.to_owned(), completed, transition, overlap_ms }
    }
    fn code(reply: &Value) -> &str { reply["code"].as_str().unwrap_or("") }

    #[tokio::test]
    async fn starts_a_function_on_free_zones_and_records_the_owner() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        let reply = gateway.handle(start("ambient_ram")).await;
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(fake.calls(), vec!["start 109"]);
        let status = gateway.handle(Request::Status).await;
        assert_eq!(status["zones"]["ram"]["owner"], "ambient_ram");
        assert_eq!(status["zones"]["ram"]["qlc"], "Running");
        assert_eq!(status["zones"]["rog_eye"]["owner"], Value::Null);
        assert_eq!(status["mode"], "control");
        assert_eq!(status["default_transition"], "pipelined");
    }

    #[tokio::test]
    async fn starting_the_current_owner_again_is_a_no_op() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        gateway.handle(start("ambient_ram")).await;
        let reply = gateway.handle(start("ambient_ram")).await;
        assert_eq!((reply["ok"].clone(), reply["changed"].clone()), (Value::Bool(true), Value::Bool(false)));
        assert_eq!(fake.calls(), vec!["start 109"]);
    }

    #[tokio::test]
    async fn start_refuses_zones_owned_by_something_else() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        gateway.seed(106).await;
        let reply = gateway.handle(start("ambient_ram")).await;
        assert_eq!(code(&reply), "zone_busy", "{reply}");
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn replace_refuses_to_stop_a_non_composable_owner() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        gateway.seed(106).await;
        let reply = gateway.handle(replace("ambient_ram")).await;
        assert_eq!(code(&reply), "explicit_stop_required", "{reply}");
        assert!(fake.calls().is_empty());
        let reply = gateway.handle(progress("ram", 16)).await;
        assert_eq!(code(&reply), "explicit_stop_required", "{reply}");
    }

    #[tokio::test]
    async fn explicit_stop_releases_every_zone_of_the_owner() {
        let fake = Fake::default();
        fake.running.lock().unwrap().insert(106);
        let gateway = gateway(&fake);
        gateway.seed(106).await;
        let reply = gateway.handle(stop("boot_proof")).await;
        assert_eq!((reply["ok"].clone(), reply["changed"].clone()), (Value::Bool(true), Value::Bool(true)), "{reply}");
        let status = gateway.handle(Request::Status).await;
        for zone in ["ram", "rog_eye", "strip"] {
            assert_eq!(status["zones"][zone]["owner"], Value::Null, "{zone}");
        }
        assert!(fake.running().is_empty());
    }

    #[tokio::test]
    async fn the_default_transition_sends_stop_and_start_together() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        gateway.handle(start("ambient_ram")).await;
        let reply = gateway.handle(progress("ram", 16)).await;
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["id"], 16);
        assert_eq!(reply["released"], json!(["ambient_ram"]));
        assert_eq!(reply["transition"], "pipelined");
        assert_eq!(fake.calls(), vec!["start 109", "stop 109", "start 16"]);
        assert_eq!(fake.batches(), vec![vec!["start 109"], vec!["stop 109", "start 16"]]);
        // The next step replaces the previous step, not the (already gone) ambient.
        gateway.handle(progress("ram", 17)).await;
        assert_eq!(fake.calls()[3..], ["stop 16", "start 17"]);
        assert_eq!(fake.running(), vec![17]);
    }

    #[tokio::test]
    async fn sequential_transition_confirms_the_stop_before_starting() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        gateway.handle(start("ambient_ram")).await;
        gateway.handle(progress_with("ram", 16, Transition::Sequential, None)).await;
        assert_eq!(fake.batches(), vec![vec!["start 109"], vec!["stop 109"], vec!["start 16"]]);
    }

    #[tokio::test]
    async fn overlap_transition_starts_the_successor_before_stopping_the_owner() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        gateway.handle(start("ambient_ram")).await;
        let started = Instant::now();
        let reply = gateway.handle(progress_with("ram", 16, Transition::Overlap, Some(60))).await;
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["transition"], "overlap");
        assert!(started.elapsed() >= Duration::from_millis(60), "the overlap must be held");
        assert_eq!(fake.batches(), vec![vec!["start 109"], vec!["start 16"], vec!["stop 109"]]);
        assert_eq!(fake.running(), vec![16]);
    }

    #[tokio::test]
    async fn overlap_length_is_bounded() {
        let gateway = gateway(&Fake::default());
        let reply = gateway.handle(progress_with("ram", 1, Transition::Overlap, Some(5000))).await;
        assert_eq!(code(&reply), "out_of_range", "{reply}");
    }

    #[tokio::test]
    async fn progress_on_one_zone_leaves_other_zones_running() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        gateway.handle(start("ambient_ram")).await;
        gateway.handle(start("ambient_eye")).await;
        gateway.handle(progress("ram", 16)).await;
        assert_eq!(fake.running(), vec![16, 112]);
    }

    #[tokio::test]
    async fn progress_id_arithmetic_and_bounds() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        assert_eq!(gateway.handle(progress("strip", 35)).await["id"], 68);
        assert_eq!(gateway.handle(progress("strip", 70)).await["id"], 103);
        assert_eq!(code(&gateway.handle(progress("strip", 71)).await), "out_of_range");
        assert_eq!(code(&gateway.handle(progress("rog_eye", 1)).await), "unknown_zone");
        assert_eq!(code(&gateway.handle(start("nonsense")).await), "unknown_function");
    }

    #[tokio::test]
    async fn start_set_sends_every_start_in_one_batch() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        let reply = gateway.handle(start_set(&["ambient_ram", "ambient_eye", "ambient_strip"])).await;
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(fake.batches(), vec![vec!["start 109", "start 112", "start 115"]]);
        let status = gateway.handle(Request::Status).await;
        for (zone, owner) in [("ram", "ambient_ram"), ("rog_eye", "ambient_eye"), ("strip", "ambient_strip")] {
            assert_eq!(status["zones"][zone]["owner"], owner);
        }
    }

    #[tokio::test]
    async fn start_set_skips_members_that_are_already_running() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        gateway.handle(start("ambient_ram")).await;
        let reply = gateway.handle(start_set(&["ambient_ram", "ambient_eye"])).await;
        assert_eq!(reply["started"], json!(["ambient_eye"]), "{reply}");
        assert_eq!(fake.batches()[1], vec!["start 112"]);
    }

    #[tokio::test]
    async fn start_set_rejects_overlapping_or_busy_sets() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        assert_eq!(code(&gateway.handle(start_set(&["boot_proof", "ambient_ram"])).await), "overlapping_set");
        assert_eq!(code(&gateway.handle(start_set(&[])).await), "bad_request");
        gateway.seed(106).await;
        assert_eq!(code(&gateway.handle(start_set(&["ambient_eye"])).await), "zone_busy");
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn requests_on_different_zones_do_not_wait_for_each_other() {
        let fake = Fake::default();
        fake.delays.lock().unwrap().insert(109, Duration::from_millis(300));
        let gateway = Arc::new(gateway(&fake));
        let begun = Instant::now();
        let slow = {
            let gateway = gateway.clone();
            tokio::spawn(async move { gateway.handle(start("ambient_ram")).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let quick = gateway.handle(start("ambient_eye")).await;
        let quick_done = begun.elapsed();
        assert_eq!(quick["ok"], true, "{quick}");
        assert!(quick_done < Duration::from_millis(200), "the eye waited for RAM: {quick_done:?}");
        assert_eq!(slow.await.unwrap()["ok"], true);
        assert!(begun.elapsed() >= Duration::from_millis(300));
    }

    #[tokio::test]
    async fn requests_on_the_same_zone_are_serialized() {
        let fake = Fake::default();
        fake.delays.lock().unwrap().insert(109, Duration::from_millis(100));
        let gateway = Arc::new(gateway(&fake));
        let first = {
            let gateway = gateway.clone();
            tokio::spawn(async move { gateway.handle(start("ambient_ram")).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Queued behind the first, then finds RAM already owned: a no-op, not a second start.
        let second = gateway.handle(start("ambient_ram")).await;
        assert_eq!(first.await.unwrap()["changed"], true);
        assert_eq!(second["changed"], false, "{second}");
        assert_eq!(fake.calls(), vec!["start 109"]);
    }

    #[tokio::test]
    async fn an_unconfirmed_start_leaves_the_zone_uncertain_until_status_resyncs() {
        let fake = Fake::default();
        fake.stuck.lock().unwrap().insert(109);
        let gateway = gateway(&fake);
        let reply = gateway.handle(start("ambient_ram")).await;
        assert_eq!(code(&reply), "qlc_unconfirmed", "{reply}");
        // Refused while uncertain, and no further QLC traffic for it.
        let calls = fake.calls().len();
        assert_eq!(code(&gateway.handle(start("ambient_ram")).await), "zone_uncertain");
        assert_eq!(fake.calls().len(), calls);
        // QLC+ later turns out to be running it; status re-syncs the table.
        fake.running.lock().unwrap().insert(109);
        let status = gateway.handle(Request::Status).await;
        assert_eq!(status["zones"]["ram"]["owner"], "ambient_ram", "{status}");
        assert_eq!(status["zones"]["ram"]["uncertain"], false);
    }

    #[tokio::test]
    async fn an_unconfirmed_pipelined_switch_marks_the_zone_uncertain() {
        let fake = Fake::default();
        let gateway = gateway(&fake);
        gateway.handle(start("ambient_ram")).await;
        fake.stuck.lock().unwrap().insert(16);
        let reply = gateway.handle(progress("ram", 16)).await;
        assert_eq!(code(&reply), "qlc_unconfirmed", "{reply}");
        let status = gateway.handle(Request::Status).await;
        assert_eq!(status["zones"]["ram"]["uncertain"], false, "status should have re-synced: {status}");
        assert_eq!(status["zones"]["ram"]["owner"], Value::Null, "nothing is running, so nothing owns RAM: {status}");
    }

    #[tokio::test]
    async fn resync_with_nothing_running_frees_the_zone() {
        let fake = Fake::default();
        fake.stuck.lock().unwrap().insert(109);
        let gateway = gateway(&fake);
        gateway.handle(start("ambient_ram")).await;
        let status = gateway.handle(Request::Status).await;
        assert_eq!(status["zones"]["ram"]["uncertain"], false);
        assert_eq!(status["zones"]["ram"]["owner"], Value::Null);
    }

    #[tokio::test]
    async fn degraded_registry_serves_status_only() {
        let fake = Fake::default();
        let registry: Registry = toml::from_str(REGISTRY).unwrap();
        let gateway = Gateway::new(Some(registry), vec!["ambient_ram: function 109 is not in the workspace".to_owned()], fake.clone());
        for request in [start("ambient_ram"), start_set(&["ambient_ram"]), replace("ambient_ram"), stop("ambient_ram"), progress("ram", 1)] {
            assert_eq!(code(&gateway.handle(request).await), "read_only");
        }
        assert!(fake.calls().is_empty());
        let status = gateway.handle(Request::Status).await;
        assert_eq!((status["mode"].clone(), status["problem_count"].clone()), (json!("read_only"), json!(1)));
    }

    #[tokio::test]
    async fn a_missing_registry_is_read_only_too() {
        let gateway = Gateway::new(None, vec!["read qlc-functions.toml: not found".to_owned()], Fake::default());
        assert_eq!(code(&gateway.handle(start("ambient_ram")).await), "read_only");
        assert_eq!(gateway.handle(Request::Status).await["mode"], "read_only");
    }

    #[test]
    fn parses_requests_strictly() {
        assert_eq!(serde_json::from_str::<Request>(r#"{"op":"status"}"#).unwrap(), Request::Status);
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"op":"progress","zone":"ram","completed":3}"#).unwrap(),
            progress("ram", 3)
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"op":"progress","zone":"ram","completed":3,"transition":"overlap","overlap_ms":30}"#).unwrap(),
            progress_with("ram", 3, Transition::Overlap, Some(30))
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"op":"start_set","functions":["a","b"]}"#).unwrap(),
            start_set(&["a", "b"])
        );
        assert!(serde_json::from_str::<Request>(r#"{"op":"start","function":"a","extra":1}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"op":"reboot"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"op":"progress","zone":"ram","completed":-1}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"op":"replace","function":"a","transition":"teleport"}"#).is_err());
    }

    #[test]
    fn builds_client_requests_from_arguments() {
        let words = |text: &str| text.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(client_request(&words("status")).unwrap(), json!({ "op": "status" }));
        assert_eq!(
            client_request(&words("start-set ambient_ram ambient_eye")).unwrap(),
            json!({ "op": "start_set", "functions": ["ambient_ram", "ambient_eye"] })
        );
        assert_eq!(
            client_request(&words("progress ram 16 --transition overlap --overlap-ms 30")).unwrap(),
            json!({ "op": "progress", "zone": "ram", "completed": 16, "transition": "overlap", "overlap_ms": 30 })
        );
        assert_eq!(
            client_request(&words("replace ambient_ram --transition sequential")).unwrap(),
            json!({ "op": "replace", "function": "ambient_ram", "transition": "sequential" })
        );
        assert!(client_request(&words("start ambient_ram --transition overlap")).is_err());
        assert!(client_request(&words("progress ram")).is_err());
        assert!(client_request(&words("start-set")).is_err());
        assert!(client_request(&words("progress ram 1 --overlap-ms soon")).is_err());
    }

    #[tokio::test]
    async fn serves_json_lines_over_an_owner_only_socket() {
        let directory = std::env::temp_dir().join(format!("monolithd-gateway-test-{}", std::process::id()));
        let path = directory.join("monolith-events").join("gateway.sock");
        let listener = prepare_socket(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let uid = std::fs::metadata(&path).unwrap().uid();
        let fake = Fake::default();
        tokio::spawn(serve(Arc::new(gateway(&fake)), listener, uid));

        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream.write_all(b"{\"op\":\"start\",\"function\":\"ambient_ram\"}\nnot json\n{\"op\":\"status\"}\n").await.unwrap();
        let mut lines = BufReader::new(stream).lines();
        let first: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let second: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let third: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["ok"], true);
        assert_eq!(second["code"], "bad_request");
        assert_eq!(third["zones"]["ram"]["owner"], "ambient_ram");

        // An oversized request is refused rather than buffered.
        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream.write_all(&vec![b'x'; MAX_REQUEST_BYTES]).await.unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).await.unwrap();
        assert!(reply.contains("request_too_large"), "{reply}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn refuses_to_replace_a_live_gateway_socket_but_clears_a_dead_one() {
        let directory = std::env::temp_dir().join(format!("monolithd-gateway-test-live-{}", std::process::id()));
        let path = directory.join("gateway.sock");
        let live = prepare_socket(&path).unwrap();
        assert!(prepare_socket(&path).unwrap_err().contains("already listening"));
        drop(live);
        assert!(prepare_socket(&path).is_ok(), "a dead socket file should be cleared");
        let _ = std::fs::remove_dir_all(&directory);
    }
}
