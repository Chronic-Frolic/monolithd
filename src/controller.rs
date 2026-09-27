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
/// The longest a single pre-sleep request may hold the cue before it must be renewed.
const MAX_PRE_SLEEP_SECONDS: u64 = 3600;
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
    /// Zones a warning claims when raised with no explicit `--zones` (each needs its
    /// own registered `warning_<zone>` asset). Owner-editable: as of 2026-09-22 every
    /// zone can author warning content, so this picks which one(s) actually show it.
    #[serde(default = "default_warning_zones")]
    pub warning_zones: Vec<String>,
    /// Zones the automatic "a job is running" indicator claims. Same shape as
    /// `warning_zones`, for `working_<zone>`.
    #[serde(default = "default_working_zones")]
    pub working_zones: Vec<String>,
    /// What an animated bar shows on its empty side: the base look (`base`) or the
    /// distinct working look (`working`). Both were asked for (owner, 2026-09-26); the
    /// lit side is always the current ambient set's own Full look. Static bars ignore it.
    #[serde(default)]
    pub progress_empty: ProgressEmpty,
    /// Which working look the job-running indicator and a bar's working empty side use:
    /// `violet` (violet plasma's motion with white glitter, `working_<zone>`) or `white`
    /// (white glinting in the base look's own colors, `working_white_<zone>`). Two sister
    /// looks to choose from (owner, 2026-09-26).
    #[serde(default)]
    pub working_look: WorkingLook,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkingLook {
    #[default]
    Violet,
    White,
}

impl WorkingLook {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Violet => "violet",
            Self::White => "white",
        }
    }

    /// The registered assets' prefix (`<prefix>_<zone>`), which is also the look a bar's
    /// working empty side is drawn in.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Violet => "working",
            Self::White => "working_white",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEmpty {
    #[default]
    Base,
    Working,
}

impl ProgressEmpty {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Working => "working",
        }
    }
}

fn default_poll_interval() -> u64 {
    1000
}

fn default_warning_zones() -> Vec<String> {
    vec!["rog_eye".to_owned()]
}

fn default_working_zones() -> Vec<String> {
    vec!["rog_eye".to_owned()]
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
    check_state_zones(&mut problems, "warning_zones", &config.warning_zones, "warning", registry);
    check_state_zones(&mut problems, "working_zones", &config.working_zones, config.working_look.prefix(), registry);
    problems
}

/// Shared validation for `warning_zones`/`working_zones`: non-empty, no duplicates,
/// every zone real, every zone has the matching `<prefix>_<zone>` asset registered.
pub fn check_state_zones(problems: &mut Vec<String>, field: &str, zones: &[String], prefix: &str, registry: &Registry) {
    if zones.is_empty() {
        problems.push(format!("{field} is empty"));
    }
    let mut seen = BTreeSet::new();
    for zone in zones {
        if !seen.insert(zone) {
            problems.push(format!("{field}: zone {zone} is listed twice"));
        }
        if !registry.zones.contains_key(zone) {
            problems.push(format!("{field}: unknown zone {zone}"));
        } else if registry.function(&format!("{prefix}_{}", zone_suffix(zone))).is_none() {
            problems.push(format!("{field}: no {prefix} asset for zone {zone} ({prefix}_{} is not registered)", zone_suffix(zone)));
        }
    }
}

// ---------------------------------------------------------------- planning

/// warning or fault, exactly as the original event contract's `fault.raise(id, severity, reason)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Warning,
    Fault,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Fault => "fault",
        }
    }

    /// The prefix of the state asset's name: `<prefix>_<zone suffix>`.
    fn asset_prefix(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Fault => "fault",
        }
    }
}

/// The short zone name state assets are named after (`rog_eye` -> `eye`), matching the
/// QLC+ Scenes authored for Phase 4 (`warning_eye`, `fault_ram`, `quiet_strip`, ...).
pub(crate) fn zone_suffix(zone: &str) -> &str {
    match zone {
        "rog_eye" => "eye",
        other => other,
    }
}

/// A raised warning or fault, claiming zones until cleared.
#[derive(Debug, Clone)]
struct ActiveFault {
    severity: Severity,
    zones: BTreeSet<String>,
    #[allow(dead_code)] // carried for status/diagnosis, not read by planning
    reason: String,
}

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
    /// Take over several zones together in one gateway call, whatever currently owns
    /// them: used for the fault/warning/quiet layer (never an ambient-set member, so it
    /// cannot wait for a phase-aligned rejoin) and for bringing a whole ambient set up
    /// from nothing running (no existing phase to align to, so individually rejoining
    /// each zone would stagger the bring-up by a full cycle per zone — found live
    /// 2026-09-22, both directions: raising a multi-zone fault/quiet and clearing one).
    PreemptSet(Vec<String>),
    Rejoin(String),
    Progress { zone: String, step: u32, family: String },
}

impl Action {
    fn request(&self) -> Value {
        match self {
            Self::Stop(function) => json!({ "op": "stop", "function": function }),
            Self::PreemptSet(functions) => json!({ "op": "preempt_set", "functions": functions }),
            Self::Rejoin(function) => json!({ "op": "rejoin", "function": function }),
            Self::Progress { zone, step, family } => json!({ "op": "progress", "zone": zone, "completed": step, "pattern": family }),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Stop(function) => format!("stop {function}"),
            Self::PreemptSet(functions) => format!("preempt set [{}]", functions.join(", ")),
            Self::Rejoin(function) => format!("rejoin {function}"),
            Self::Progress { zone, step, family } => format!("progress {zone} {step} ({family})"),
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
fn plan(view: &GatewayView, wants: &BTreeMap<String, Want>, state: &BTreeMap<String, String>, registry: &Registry) -> Plan {
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
    for zone in wants.keys().chain(state.keys()) {
        match view.zones.get(zone) {
            None => return blocked(format!("the gateway does not know zone {zone}")),
            Some(entry) if entry.uncertain => return blocked(format!("zone {zone} is in an unconfirmed state")),
            Some(_) => {}
        }
    }

    // The preemption layer always goes first: it must never wait behind an ambient batch.
    // One preempt_set call takes over every zone it claims at once, whatever currently
    // owns them (composable or not) — a fault can never fail silently and retry forever
    // because a lone replace was refused, and several zones changing together (a
    // multi-zone fault, or clearing one) land in one QLC+ write instead of staggering.
    let mut actions: Vec<Action> = Vec::new();
    // No dedup needed here: each zone's fault/warning/quiet asset is that zone's own
    // suffixed function (fault_ram, fault_eye, ...), never shared across zones, so
    // `state` can never contribute the same function twice and trip preempt_set's
    // overlapping_set guard.
    let preempting: Vec<String> = state
        .iter()
        .filter(|(zone, function)| view.zones[zone.as_str()].owner.as_deref() != Some(function.as_str()))
        .map(|(_, function)| function.clone())
        .collect();
    if !preempting.is_empty() {
        actions.push(Action::PreemptSet(preempting));
    }

    // Whether any zone already correctly shows its ambient want: a real, pre-existing
    // phase reference for an individual rejoin to align to. Computed once, up front, not
    // accumulated zone-by-zone inside the loop below: deciding a zone's routing before
    // every zone has been looked at would let an early zone (in BTreeMap order) be routed
    // as though the whole set were down even when a later zone is already running it.
    let ambient_running = wants.iter().any(|(zone, want)| {
        matches!(want, Want::Ambient { .. }) && !state.contains_key(zone) && view.zones[zone].owner.as_deref() == Some(want.owner_name().as_str())
    });

    let mut stops: Vec<String> = Vec::new();
    let mut free_for_ambient: Vec<String> = Vec::new();
    let mut rejoins: Vec<String> = Vec::new();
    let mut progress: Vec<Action> = Vec::new();

    // A non-composable owner (the bench scene, say) has to be stopped explicitly before
    // an individual replace/rejoin can take over its zone. A fresh, whole-set bring-up
    // needs no such call here: preempt_set below stops any current owner itself,
    // composable or not.
    let stop_foreign = |name: &str, stops: &mut Vec<String>| {
        if !registry.function(name).is_some_and(|entry| entry.composable) && !stops.iter().any(|known| known == name) {
            stops.push(name.to_owned());
        }
    };

    for (zone, want) in wants {
        if state.contains_key(zone) {
            continue; // preempted this pass; the base want waits for the next one
        }
        let owner = view.zones[zone].owner.as_ref();
        if owner.is_some_and(|name| *name == want.owner_name()) {
            continue;
        }
        match (want, classify(owner, registry)) {
            (Want::Ambient { function }, Owner::Free) => {
                if ambient_running {
                    rejoins.push(function.clone());
                } else {
                    free_for_ambient.push(function.clone());
                }
            }
            (Want::Ambient { function }, Owner::Ambient | Owner::Progress) => {
                if ambient_running {
                    // The wanted set already runs elsewhere (a zone returning from a
                    // progress bar, say): rejoin in phase with it.
                    rejoins.push(function.clone());
                } else {
                    // Nothing of the wanted set runs yet, so this is a switch between
                    // looks: every zone changes together in one preempt_set. Rejoining
                    // zone by zone would align each to whichever started first and
                    // stagger the switch by up to a whole loop (found live 2026-09-26,
                    // violet plasma <-> fire).
                    free_for_ambient.push(function.clone());
                }
            }
            (Want::Ambient { function }, Owner::Foreign(name)) => {
                if ambient_running {
                    // A stable phase reference already runs elsewhere in the set: align
                    // to it individually. replace's own logic stops a composable owner
                    // as part of the switch; only a non-composable one needs a prior stop.
                    stop_foreign(name, &mut stops);
                    rejoins.push(function.clone());
                } else {
                    // Nothing in the set is running: batch this zone into the fresh start
                    // below (preempt_set), which stops its owner itself, instead of an
                    // individual rejoin that would align to whatever zone happened to
                    // start first and stagger the bring-up by a full cycle per zone
                    // (found live, 2026-09-22, returning from quiet).
                    free_for_ambient.push(function.clone());
                }
            }
            (Want::Progress { zone, step, family }, foreign_or_other) => {
                if let Owner::Foreign(name) = foreign_or_other {
                    stop_foreign(name, &mut stops);
                }
                progress.push(Action::Progress { zone: zone.clone(), step: *step, family: family.clone() });
            }
        }
    }

    actions.extend(stops.into_iter().map(Action::Stop));
    if !free_for_ambient.is_empty() {
        actions.push(Action::PreemptSet(free_for_ambient));
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
    faults: BTreeMap<String, ActiveFault>,
    quiet: Option<String>,
    /// The pre-sleep cue (phase 3 of the sleep policy): why, and until when. It expires on
    /// its own, so a sleep policy that dies mid-countdown cannot leave it up; the policy
    /// renews it while it counts down.
    pre_sleep: Option<(String, Instant)>,
    /// The (look, empty mode) each progress zone's bar was last requested in. A bar reports
    /// only `family:step`, so without this a bar would keep a stale look after an ambient
    /// switch until its next step; unknown (after a start) counts as stale.
    bar_styles: BTreeMap<String, (String, &'static str)>,
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
        Self { link, registry, config, planner, ambient, paused: false, dirty: true, faults: BTreeMap::new(), quiet: None, pre_sleep: None, bar_styles: BTreeMap::new(), report: Report::default() }
    }

    fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Which state asset (if any) is registered for a zone at a given severity/quiet.
    fn state_asset(&self, prefix: &str, zone: &str) -> Option<String> {
        let name = format!("{prefix}_{}", zone_suffix(zone));
        self.registry.function(&name).map(|_| name)
    }

    /// Validate and default the zones a raised warning/fault claims. Warning defaults to
    /// `controller.toml`'s `warning_zones` (owner-editable, since 2026-09-22 every zone
    /// can author warning content); fault defaults to every managed zone that has a
    /// fault asset. An explicit zone with no matching asset is refused.
    fn resolve_state_zones(&self, severity: Severity, zones: Option<Vec<String>>) -> Result<BTreeSet<String>, String> {
        let requested: Vec<String> = match zones {
            None => match severity {
                Severity::Warning => self.config.warning_zones.clone(),
                Severity::Fault => self.registry.zones.keys().cloned().collect(),
            },
            Some(zones) if zones.is_empty() => return Err("at least one zone is required".to_owned()),
            Some(zones) => zones,
        };
        let mut resolved = BTreeSet::new();
        for zone in requested {
            if !self.registry.zones.contains_key(&zone) {
                return Err(format!("unknown zone {zone}"));
            }
            if self.state_asset(severity.asset_prefix(), &zone).is_none() {
                return Err(format!("no {} asset for zone {zone} ({}_{} is not registered)", severity.as_str(), severity.asset_prefix(), zone_suffix(&zone)));
            }
            resolved.insert(zone);
        }
        Ok(resolved)
    }

    /// The preemption layer: zone -> the function that must own it right now, fault over
    /// warning over quiet over the pre-sleep cue over the automatic working indicator. Empty when nothing is
    /// raised, quiet is not set, and no job is active.
    fn state_wants(&self) -> BTreeMap<String, String> {
        let mut wants = BTreeMap::new();
        // Lowest tier: a job is running somewhere, shown on `controller.toml`'s
        // `working_zones` (working_eye's white is the same unmodulated FixtureVal as
        // reference_white_eye, so the eye copy tracks its calibration gain like every
        // other white asset there; ram/strip copies exist since 2026-09-22 too, with
        // no separate color config beyond what the owner authors). Computed fresh
        // every pass from planner state, not tracked as its own ActiveFault, so
        // quiet/warning/fault below all still overwrite it: an explicit "go quiet"
        // request must suppress this cosmetic hint too, and a real operator warning is
        // a more specific (and differently colored) signal than "something is
        // running". A zone that is itself progress-capable and currently holding a
        // real lease is skipped here (2026-09-22, owner: reverse that priority) -- its
        // own progress bar is strictly more informative than the generic "something is
        // running" indicator, so it wins on the zone it actually occupies; the
        // indicator still covers every *other* configured zone, including a
        // progress-capable one that's simply idle right now.
        if !self.planner.jobs().is_empty() {
            for zone in &self.config.working_zones {
                if matches!(self.planner.target(zone), ZoneTarget::Progress { .. }) {
                    continue;
                }
                if let Some(name) = self.state_asset(self.config.working_look.prefix(), zone) {
                    wants.insert(zone.clone(), name);
                }
            }
        }
        // The pre-sleep cue: above the job indicator and progress (a job resets the sleep
        // clock, so the two never meet in practice), below an explicit quiet request and
        // any warning or fault.
        if self.pre_sleep.is_some() {
            for zone in self.registry.zones.keys() {
                if let Some(name) = self.state_asset("pre_sleep", zone) {
                    wants.insert(zone.clone(), name);
                }
            }
        }
        if self.quiet.is_some() {
            for zone in self.registry.zones.keys() {
                if let Some(name) = self.state_asset("quiet", zone) {
                    wants.insert(zone.clone(), name);
                }
            }
        }
        for fault in self.faults.values().filter(|fault| fault.severity == Severity::Warning) {
            for zone in &fault.zones {
                if let Some(name) = self.state_asset("warning", zone) {
                    wants.insert(zone.clone(), name);
                }
            }
        }
        for fault in self.faults.values().filter(|fault| fault.severity == Severity::Fault) {
            for zone in &fault.zones {
                if let Some(name) = self.state_asset("fault", zone) {
                    wants.insert(zone.clone(), name);
                }
            }
        }
        wants
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
            if let ZoneTarget::Progress { step, pattern } = self.planner.target(zone) {
                // A pattern only applies if it actually belongs to the zone the job
                // landed on (it might have been requested for a different zone and
                // assigned elsewhere, e.g. RAM was busy so the job took Strip instead);
                // otherwise fall back to the zone's own default family.
                let family = pattern
                    .as_deref()
                    .and_then(|name| self.registry.progress_by_name(name))
                    .filter(|entry| entry.zone == *zone)
                    .or_else(|| self.registry.progress_for_zone(zone));
                if let Some(family) = family {
                    wants.insert(zone.clone(), Want::Progress { zone: zone.clone(), family: family.name.clone(), step });
                }
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
        if self.pre_sleep.as_ref().is_some_and(|(_, until)| *until <= now) {
            self.pre_sleep = None;
        }
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
        let state = self.state_wants();
        self.report.wants = wants.iter().map(|(zone, want)| (zone.clone(), want.owner_name())).collect();
        for (zone, function) in &state {
            self.report.wants.insert(zone.clone(), function.clone());
        }

        let plan = plan(&view, &wants, &state, &self.registry);
        self.block(plan.blocked);
        let empty = match self.config.progress_empty {
            ProgressEmpty::Base => "base",
            ProgressEmpty::Working => self.config.working_look.prefix(),
        };
        let style = (self.ambient.clone(), empty);
        self.bar_styles.retain(|zone, _| matches!(wants.get(zone), Some(Want::Progress { .. })));
        let mut actions = plan.actions;
        for (zone, want) in &wants {
            // A bar already showing its step, but drawn in another look: ask again.
            if let Want::Progress { step, family, .. } = want {
                let shown = view.zones.get(zone).and_then(|entry| entry.owner.as_deref()) == Some(want.owner_name().as_str());
                if shown && !state.contains_key(zone) && self.bar_styles.get(zone) != Some(&style) {
                    actions.push(Action::Progress { zone: zone.clone(), step: *step, family: family.clone() });
                }
            }
        }
        for action in actions {
            let mut request = action.request();
            if let Action::Progress { zone, .. } = &action {
                // Which looks an animated bar is drawn with; the gateway falls back to the
                // static step Scene when it has no loops for them.
                request["look"] = json!(style.0);
                request["empty"] = json!(style.1);
                self.bar_styles.remove(zone);
            }
            let reply = match self.link.call(request).await {
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
            if let Action::Progress { zone, .. } = &action {
                self.bar_styles.insert(zone.clone(), style.clone());
            }
            // The gateway schedules a rejoin with a long wait for its loop boundary; each
            // reconcile until then asks again and hears "still scheduled", which is not news.
            if reply["scheduled"] == Value::Bool(true) {
                if reply["new"] == Value::Bool(true) {
                    self.remember(format!("{} (scheduled in {} ms)", action.describe(), reply["aligned_wait_ms"]));
                }
                continue;
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
            ControllerRequest::JobStart { id, label, total, priority, pattern } => {
                match &pattern {
                    Some(name) if self.registry.progress_by_name(name).is_none() => Err(("unknown_pattern", format!("no progress pattern named {name}"))),
                    _ => self.planner.start(&id, &label, total, priority, pattern).map(|()| json!({ "job": self.job_json(&id) })).map_err(|error| ("bad_request", error)),
                }
            }
            ControllerRequest::JobProgress { id, completed, total } => {
                self.planner.progress(&id, completed, total).map(|()| json!({ "job": self.job_json(&id) })).map_err(|error| ("unknown_job", error))
            }
            ControllerRequest::JobComplete { id } => self.planner.complete(&id, now).map(|()| json!({ "job": self.job_json(&id) })).map_err(|error| ("unknown_job", error)),
            ControllerRequest::JobFail { id, reason } => self.planner.fail(&id, &reason).map(|()| json!({})).map_err(|error| ("unknown_job", error)),
            ControllerRequest::FaultRaise { id, severity, zones, reason } => {
                let severity = match severity.as_str() {
                    "warning" => Ok(Severity::Warning),
                    "fault" => Ok(Severity::Fault),
                    other => Err(format!("severity must be \"warning\" or \"fault\", not {other:?}")),
                };
                match severity.and_then(|severity| self.resolve_state_zones(severity, zones).map(|zones| (severity, zones))) {
                    Ok((severity, zones)) => {
                        let reply = json!({ "id": id, "severity": severity.as_str(), "zones": zones });
                        self.faults.insert(id, ActiveFault { severity, zones, reason });
                        Ok(reply)
                    }
                    Err(error) => Err(("bad_request", error)),
                }
            }
            ControllerRequest::FaultClear { id } => match self.faults.remove(&id) {
                Some(_) => Ok(json!({ "id": id })),
                None => Err(("unknown_fault", format!("no active warning/fault with id {id}"))),
            },
            ControllerRequest::QuietSet { reason } => {
                self.quiet = Some(reason.clone());
                Ok(json!({ "reason": reason }))
            }
            ControllerRequest::QuietClear => {
                self.quiet = None;
                Ok(json!({}))
            }
            ControllerRequest::Pause => {
                self.paused = true;
                Ok(json!({ "paused": true }))
            }
            ControllerRequest::Resume => {
                self.paused = false;
                // The watchdog resumes the controller after every wake; a pre-sleep cue left
                // from before the suspend no longer means anything.
                self.pre_sleep = None;
                Ok(json!({ "paused": false }))
            }
            ControllerRequest::PreSleepSet { reason, seconds } => {
                if !(1..=MAX_PRE_SLEEP_SECONDS).contains(&seconds) {
                    Err(("bad_request", format!("seconds must be between 1 and {MAX_PRE_SLEEP_SECONDS}")))
                } else if !self.registry.zones.keys().any(|zone| self.state_asset("pre_sleep", zone).is_some()) {
                    Err(("bad_request", "no pre_sleep_<zone> asset is registered".to_owned()))
                } else {
                    self.pre_sleep = Some((reason.clone(), now + Duration::from_secs(seconds)));
                    Ok(json!({ "reason": reason, "seconds": seconds }))
                }
            }
            ControllerRequest::PreSleepClear => {
                self.pre_sleep = None;
                Ok(json!({}))
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
            "progress_empty": self.config.progress_empty.as_str(),
            "working_look": self.config.working_look.as_str(),
            "waiting": self.report.blocked,
            "gateway": self.report.gateway,
            "zones": zones,
            "jobs": self.planner.jobs().iter().map(job_json).collect::<Vec<_>>(),
            "failures": failures,
            "quiet": self.quiet,
            "pre_sleep": self.pre_sleep.as_ref().map(|(reason, _)| reason),
            "active_faults": self.faults.iter().map(|(id, fault)| json!({ "id": id, "severity": fault.severity.as_str(), "zones": fault.zones, "reason": fault.reason })).collect::<Vec<_>>(),
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
    json!({ "id": job.id, "label": job.label, "completed": job.completed, "total": job.total, "priority": job.priority, "pattern": job.pattern, "state": state, "zone": zone })
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
        /// A named progress family to render this job's zone with, instead of the
        /// zone's default (e.g. progress_ram_interleaved instead of progress_ram).
        #[serde(default)]
        pattern: Option<String>,
    },
    #[serde(rename = "job.progress")]
    JobProgress { id: String, completed: u32, total: Option<u32> },
    #[serde(rename = "job.complete")]
    JobComplete { id: String },
    #[serde(rename = "job.fail")]
    JobFail { id: String, reason: String },
    /// Zone list defaults per severity if omitted; see `resolve_state_zones`.
    #[serde(rename = "fault.raise")]
    FaultRaise {
        id: String,
        severity: String,
        #[serde(default)]
        zones: Option<Vec<String>>,
        reason: String,
    },
    #[serde(rename = "fault.clear")]
    FaultClear { id: String },
    #[serde(rename = "quiet.set")]
    QuietSet { reason: String },
    #[serde(rename = "quiet.clear")]
    QuietClear,
    /// Show the pre-sleep cue for `seconds` (renewable) unless something higher wins.
    #[serde(rename = "presleep.set")]
    PreSleepSet { reason: String, seconds: u64 },
    #[serde(rename = "presleep.clear")]
    PreSleepClear,
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
    crate::paths::config_dir()
}

/// `monolithd controller`: run the controller until it is stopped.
pub async fn run() -> Result<(), String> {
    eprintln!("monolithd controller: {}", crate::paths::describe());
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

const USAGE: &str = "usage: monolithd event <status | ambient SET | job-start ID LABEL TOTAL [PRIORITY] [--pattern NAME] | job-progress ID COMPLETED [TOTAL] | job-complete ID | job-fail ID REASON... | fault-raise ID warning|fault [--zones a,b,c] REASON... | fault-clear ID | quiet-set REASON... | quiet-clear | presleep-set SECONDS REASON... | presleep-clear | pause | resume>";

fn client_request(arguments: &[String]) -> Result<Value, String> {
    let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let number = |text: &str| text.parse::<u32>().map_err(|_| format!("{text:?} is not a non-negative integer"));
    Ok(match words.as_slice() {
        ["status"] => json!({ "op": "status" }),
        ["ambient", set] => json!({ "op": "ambient.select", "set": set }),
        ["job-start", rest @ ..] if !rest.is_empty() => {
            let mut rest: Vec<&str> = rest.to_vec();
            let pattern = if let Some(position) = rest.iter().position(|word| *word == "--pattern") {
                if position + 1 >= rest.len() {
                    return Err("--pattern needs a value".to_owned());
                }
                let value = rest[position + 1];
                rest.drain(position..=position + 1);
                Some(value)
            } else {
                None
            };
            let (id, label, total, priority) = match rest.as_slice() {
                [id, label, total] => (*id, *label, *total, None),
                [id, label, total, priority] => (*id, *label, *total, Some(*priority)),
                _ => return Err(USAGE.to_owned()),
            };
            let mut request = json!({ "op": "job.start", "id": id, "label": label, "total": number(total)? });
            if let Some(priority) = priority {
                request["priority"] = json!(priority.parse::<i32>().map_err(|_| format!("{priority:?} is not an integer"))?);
            }
            if let Some(pattern) = pattern {
                request["pattern"] = json!(pattern);
            }
            request
        }
        ["job-progress", id, completed] => json!({ "op": "job.progress", "id": id, "completed": number(completed)? }),
        ["job-progress", id, completed, total] => json!({ "op": "job.progress", "id": id, "completed": number(completed)?, "total": number(total)? }),
        ["job-complete", id] => json!({ "op": "job.complete", "id": id }),
        ["job-fail", id, reason @ ..] if !reason.is_empty() => json!({ "op": "job.fail", "id": id, "reason": reason.join(" ") }),
        ["fault-raise", rest @ ..] if rest.len() >= 3 => {
            let mut rest: Vec<&str> = rest.to_vec();
            let zones = if let Some(position) = rest.iter().position(|word| *word == "--zones") {
                if position + 1 >= rest.len() {
                    return Err("--zones needs a comma-separated value".to_owned());
                }
                let value = rest[position + 1];
                rest.drain(position..=position + 1);
                Some(value.split(',').map(str::to_owned).collect::<Vec<_>>())
            } else {
                None
            };
            let (id, severity, reason) = match rest.as_slice() {
                [id, severity, reason @ ..] if !reason.is_empty() => (*id, *severity, reason.join(" ")),
                _ => return Err(USAGE.to_owned()),
            };
            let mut request = json!({ "op": "fault.raise", "id": id, "severity": severity, "reason": reason });
            if let Some(zones) = zones {
                request["zones"] = json!(zones);
            }
            request
        }
        ["fault-clear", id] => json!({ "op": "fault.clear", "id": id }),
        ["quiet-set", reason @ ..] if !reason.is_empty() => json!({ "op": "quiet.set", "reason": reason.join(" ") }),
        ["quiet-clear"] => json!({ "op": "quiet.clear" }),
        ["presleep-set", seconds, reason @ ..] if !reason.is_empty() => json!({ "op": "presleep.set", "seconds": seconds.parse::<u64>().map_err(|_| format!("{seconds:?} is not a number of seconds"))?, "reason": reason.join(" ") }),
        ["presleep-clear"] => json!({ "op": "presleep.clear" }),
        ["pause"] => json!({ "op": "pause" }),
        ["resume"] => json!({ "op": "resume" }),
        _ => return Err(USAGE.to_owned()),
    })
}

/// Send one request to the controller socket and return its reply.
pub async fn call(request: &Value) -> Result<Value, String> {
    let path = socket_path()?;
    let exchange = async {
        let mut stream = UnixStream::connect(&path).await.map_err(|error| format!("connect {} (is the controller running?): {error}", path.display()))?;
        stream.write_all(format!("{request}\n").as_bytes()).await.map_err(|error| format!("send request: {error}"))?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).await.map_err(|error| format!("read reply: {error}"))?;
        Ok::<String, String>(reply)
    };
    let reply = timeout(CLIENT_TIMEOUT, exchange).await.map_err(|_| "controller did not answer in time".to_owned())??;
    serde_json::from_str(&reply).map_err(|error| format!("controller sent invalid JSON: {error}"))
}

/// `monolithd event ...`: a one-shot client for the controller socket.
pub async fn client(arguments: Vec<String>) -> Result<(), String> {
    let request = client_request(&arguments)?;
    let reply = call(&request).await?;
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
        name = "ember_ram"
        kind = "chaser"
        id = 211
        children = []
        zones = ["ram"]
        composable = true
        [[functions]]
        name = "ember_eye"
        kind = "chaser"
        id = 212
        children = []
        zones = ["rog_eye"]
        composable = true
        [[functions]]
        name = "ember_strip"
        kind = "chaser"
        id = 213
        children = []
        zones = ["strip"]
        composable = true
        [[functions]]
        name = "reference_white_eye"
        kind = "scene"
        id = 116
        children = []
        zones = ["rog_eye"]
        composable = true
        [[functions]]
        name = "warning_eye"
        kind = "scene"
        id = 200
        children = []
        zones = ["rog_eye"]
        composable = true
        [[functions]]
        name = "fault_ram"
        kind = "scene"
        id = 201
        children = []
        zones = ["ram"]
        composable = true
        [[functions]]
        name = "fault_eye"
        kind = "scene"
        id = 202
        children = []
        zones = ["rog_eye"]
        composable = true
        [[functions]]
        name = "fault_strip"
        kind = "scene"
        id = 203
        children = []
        zones = ["strip"]
        composable = true
        [[functions]]
        name = "quiet_ram"
        kind = "scene"
        id = 204
        children = []
        zones = ["ram"]
        composable = true
        [[functions]]
        name = "quiet_eye"
        kind = "scene"
        id = 205
        children = []
        zones = ["rog_eye"]
        composable = true
        [[functions]]
        name = "quiet_strip"
        kind = "scene"
        id = 206
        children = []
        zones = ["strip"]
        composable = true
        [[functions]]
        name = "working_eye"
        kind = "scene"
        id = 207
        children = []
        zones = ["rog_eye"]
        composable = true
        [[functions]]
        name = "pre_sleep_ram"
        kind = "scene"
        id = 208
        children = []
        zones = ["ram"]
        composable = true
        [[functions]]
        name = "pre_sleep_eye"
        kind = "scene"
        id = 209
        children = []
        zones = ["rog_eye"]
        composable = true
        [[functions]]
        name = "pre_sleep_strip"
        kind = "scene"
        id = 210
        children = []
        zones = ["strip"]
        composable = true
        [[ambient_sets]]
        name = "deep_violet"
        functions = ["ambient_ram", "ambient_eye", "ambient_strip"]
        [[ambient_sets]]
        name = "ember"
        functions = ["ember_ram", "ember_eye", "ember_strip"]
        [[progress]]
        name = "progress_ram"
        zone = "ram"
        label = "RAM"
        first_id = 0
        total = 32
        [[progress]]
        name = "progress_ram_interleaved"
        zone = "ram"
        label = "RAM (interleaved)"
        first_id = 300
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

    /// `registry()` plus warning/working assets for ram and strip (production has
    /// these since 2026-09-22; the shared `registry()`/`REGISTRY` fixture deliberately
    /// keeps them eye-only so `requests_are_rejected_for_unknown_severities_zones_or_missing_assets`
    /// still has a real "zone with no matching asset" case to exercise).
    fn registry_with_whole_machine_states() -> Registry {
        let extra = r#"
        [[functions]]
        name = "warning_ram"
        kind = "scene"
        id = 208
        children = []
        zones = ["ram"]
        composable = true
        [[functions]]
        name = "warning_strip"
        kind = "scene"
        id = 209
        children = []
        zones = ["strip"]
        composable = true
        [[functions]]
        name = "working_ram"
        kind = "scene"
        id = 210
        children = []
        zones = ["ram"]
        composable = true
        [[functions]]
        name = "working_strip"
        kind = "scene"
        id = 211
        children = []
        zones = ["strip"]
        composable = true
        "#;
        toml::from_str(&format!("{REGISTRY}\n{extra}")).unwrap()
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
        plan(&GatewayView::from_status(status).unwrap(), &wants_for(controller), &controller.state_wants(), &controller.registry)
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

    /// `warning_zones`/`working_zones` covering all three zones, for testing the
    /// whole-machine-authorable states added 2026-09-22 (default `config()` stays
    /// eye-only, matching this project's original, still-valid default).
    fn config_with_whole_machine_states() -> ControllerConfig {
        toml::from_str(
            "version = 1\ndefault_ambient = \"deep_violet\"\nprogress_zones = [\"ram\", \"strip\"]\ncomplete_hold_seconds = 15\n\
             warning_zones = [\"ram\", \"rog_eye\", \"strip\"]\nworking_zones = [\"ram\", \"rog_eye\", \"strip\"]\n",
        )
        .unwrap()
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
        // boot_proof, non-composable, is stopped as part of the same preempt_set call
        // that starts the whole set — no separate Stop action is needed up front.
        assert_eq!(plan.actions, vec![Action::PreemptSet(vec!["ambient_ram".to_owned(), "ambient_eye".to_owned(), "ambient_strip".to_owned()])]);
    }

    #[test]
    fn switching_between_looks_changes_every_zone_together() {
        let mut controller = controller_with(&FakeLink::default());
        controller.handle(ControllerRequest::AmbientSelect { set: "ember".into() }, Instant::now());
        let plan = plan_for(&healthy(AMBIENT), &controller);
        assert_eq!(plan.actions, vec![Action::PreemptSet(vec!["ember_ram".to_owned(), "ember_eye".to_owned(), "ember_strip".to_owned()])]);
        // Half-switched (the gateway confirmed only the RAM): the rest rejoin it in phase.
        let plan = plan_for(&healthy([Some("ember_ram"), Some("ambient_eye"), Some("ambient_strip")]), &controller);
        assert_eq!(plan.actions, vec![Action::Rejoin("ember_eye".to_owned()), Action::Rejoin("ember_strip".to_owned())]);
    }

    #[test]
    fn a_dark_machine_gets_the_set_in_one_batch() {
        let controller = controller_with(&FakeLink::default());
        let plan = plan_for(&healthy([None; 3]), &controller);
        assert_eq!(plan.actions, vec![Action::PreemptSet(vec!["ambient_ram".to_owned(), "ambient_eye".to_owned(), "ambient_strip".to_owned()])]);
    }

    #[test]
    fn nothing_to_do_when_the_plant_already_matches() {
        let controller = controller_with(&FakeLink::default());
        assert_eq!(plan_for(&healthy(AMBIENT), &controller).actions, vec![]);
    }

    #[test]
    fn multiple_zones_returning_to_a_fully_stopped_set_start_together() {
        // Reproduces the bug found live 2026-09-22: resuming from quiet, all three zones
        // were owned by composable-but-foreign functions with nothing in the set running.
        let controller = controller_with(&FakeLink::default());
        let plan = plan_for(&healthy([Some("quiet_ram"), Some("quiet_eye"), Some("quiet_strip")]), &controller);
        assert_eq!(
            plan.actions,
            vec![Action::PreemptSet(vec!["ambient_ram".to_owned(), "ambient_eye".to_owned(), "ambient_strip".to_owned()])],
            "the whole set must come up together in one call, not three staggered stops plus a start"
        );
    }

    #[test]
    fn a_mix_of_free_and_foreign_owned_zones_still_batches_into_one_preempt_set() {
        let controller = controller_with(&FakeLink::default());
        let plan = plan_for(&healthy([None, Some("quiet_eye"), Some("fault_strip")]), &controller);
        assert_eq!(plan.actions, vec![Action::PreemptSet(vec!["ambient_ram".to_owned(), "ambient_eye".to_owned(), "ambient_strip".to_owned()])]);
    }

    #[test]
    fn a_single_zone_returning_while_its_set_still_has_a_running_member_uses_an_aligned_rejoin() {
        // eye and strip are already running ambient; only ram needs to come back (say, a
        // cleared fault). This must stay an individual, phase-aligned rejoin — batching
        // would needlessly restart eye and strip, which were never disturbed.
        let controller = controller_with(&FakeLink::default());
        let plan = plan_for(&healthy([Some("fault_ram"), Some("ambient_eye"), Some("ambient_strip")]), &controller);
        assert_eq!(plan.actions, vec![Action::Rejoin("ambient_ram".to_owned())]);
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
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 10, priority: 0, pattern: None }, Instant::now());
        controller.handle(ControllerRequest::JobProgress { id: "a".into(), completed: 5, total: None }, Instant::now());
        let plan = plan_for(&healthy(AMBIENT), &controller);
        // The eye also picks up the job-running indicator (see the working-indicator
        // tests below); the state layer always goes first.
        assert_eq!(plan.actions, vec![Action::PreemptSet(vec!["working_eye".to_owned()]), Action::Progress { zone: "ram".to_owned(), step: 16, family: "progress_ram".to_owned() }]);
        assert_eq!(plan_for(&healthy([Some("progress_ram:16"), Some("working_eye"), Some("ambient_strip")]), &controller).actions, vec![], "already showing it");
        assert_eq!(
            plan_for(&healthy([Some("progress_ram:12"), Some("working_eye"), Some("ambient_strip")]), &controller).actions,
            vec![Action::Progress { zone: "ram".to_owned(), step: 16, family: "progress_ram".to_owned() }],
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
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 4, priority: 0, pattern: None }, Instant::now());
        let plan = plan_for(&healthy([Some("boot_proof"); 3]), &controller);
        // The eye is claimed by the job-running indicator, not the ambient bring-up, so
        // it drops out of that batch entirely; ram's progress transition still needs
        // boot_proof (non-composable) stopped explicitly first, which incidentally frees
        // strip too, and strip goes through its own preempt_set, same as any other fresh
        // whole-zone bring-up.
        assert_eq!(
            plan.actions,
            vec![
                Action::PreemptSet(vec!["working_eye".to_owned()]),
                Action::Stop("boot_proof".to_owned()),
                Action::PreemptSet(vec!["ambient_strip".to_owned()]),
                Action::Progress { zone: "ram".to_owned(), step: 0, family: "progress_ram".to_owned() },
            ]
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

    #[test]
    fn a_job_can_choose_an_alternate_progress_pattern() {
        let mut controller = controller_with(&FakeLink::default());
        controller.handle(
            ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 10, priority: 0, pattern: Some("progress_ram_interleaved".into()) },
            Instant::now(),
        );
        controller.handle(ControllerRequest::JobProgress { id: "a".into(), completed: 5, total: None }, Instant::now());
        assert_eq!(controller.wants()["ram"], Want::Progress { zone: "ram".to_owned(), family: "progress_ram_interleaved".to_owned(), step: 16 });
    }

    #[test]
    fn a_pattern_for_a_different_zone_than_the_job_landed_on_falls_back_to_that_zones_default() {
        // RAM is taken first, so a second job requesting the RAM-only interleaved
        // pattern lands on Strip instead; Strip has no such pattern, so it must fall
        // back to its own default family rather than silently applying nothing or a
        // wrong family.
        let mut controller = controller_with(&FakeLink::default());
        controller.handle(ControllerRequest::JobStart { id: "first".into(), label: "x".into(), total: 10, priority: 0, pattern: None }, Instant::now());
        controller.handle(
            ControllerRequest::JobStart { id: "second".into(), label: "x".into(), total: 10, priority: 0, pattern: Some("progress_ram_interleaved".into()) },
            Instant::now(),
        );
        controller.handle(ControllerRequest::JobProgress { id: "second".into(), completed: 5, total: None }, Instant::now());
        assert_eq!(controller.wants()["strip"], Want::Progress { zone: "strip".to_owned(), family: "progress_strip".to_owned(), step: 35 });
    }

    // ------------------------------------------------------------ the loop

    #[tokio::test]
    async fn reconciliation_brings_the_lights_up_from_the_boot_scene() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy([Some("boot_proof"); 3]);
        let mut controller = controller_with(&link);
        controller.reconcile(Instant::now()).await;
        assert_eq!(ops(&link), vec![r#"{"functions":["ambient_ram","ambient_eye","ambient_strip"],"op":"preempt_set"}"#]);
    }

    #[tokio::test]
    async fn a_job_runs_from_lease_to_completion_hold_to_ambient() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy(AMBIENT);
        let mut controller = controller_with(&link);
        let start = Instant::now();

        controller.handle(ControllerRequest::JobStart { id: "backup".into(), label: "Backup".into(), total: 32, priority: 0, pattern: None }, start);
        controller.handle(ControllerRequest::JobProgress { id: "backup".into(), completed: 8, total: None }, start);
        controller.reconcile(start).await;
        // The eye picks up the job-running indicator in the same pass, ahead of the
        // progress action (the state layer always goes first).
        assert_eq!(ops(&link), vec![r#"{"functions":["working_eye"],"op":"preempt_set"}"#, r#"{"completed":8,"empty":"base","look":"deep_violet","op":"progress","pattern":"progress_ram","zone":"ram"}"#]);

        // The plant now shows it; a completed job holds at 100%.
        *link.status.lock().unwrap() = healthy([Some("progress_ram:8"), Some("ambient_eye"), Some("ambient_strip")]);
        controller.handle(ControllerRequest::JobComplete { id: "backup".into() }, start);
        controller.reconcile(start + Duration::from_secs(1)).await;
        assert_eq!(ops(&link).last().unwrap(), r#"{"completed":32,"empty":"base","look":"deep_violet","op":"progress","pattern":"progress_ram","zone":"ram"}"#);

        // After the 15 s hold the zone returns to the ambient set.
        *link.status.lock().unwrap() = healthy([Some("progress_ram:32"), Some("ambient_eye"), Some("ambient_strip")]);
        controller.reconcile(start + Duration::from_secs(16)).await;
        assert_eq!(ops(&link).last().unwrap(), r#"{"function":"ambient_ram","op":"rejoin"}"#);
        assert!(controller.planner.jobs().is_empty());
    }

    #[tokio::test]
    async fn a_bar_is_asked_for_again_when_the_ambient_look_changes_under_it() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy(AMBIENT);
        let mut controller = controller_with(&link);
        let start = Instant::now();
        controller.handle(ControllerRequest::JobStart { id: "backup".into(), label: "Backup".into(), total: 32, priority: 0, pattern: None }, start);
        controller.handle(ControllerRequest::JobProgress { id: "backup".into(), completed: 8, total: None }, start);
        controller.reconcile(start).await;
        *link.status.lock().unwrap() = healthy([Some("progress_ram:8"), Some("working_eye"), Some("ambient_strip")]);
        let before = ops(&link).len();
        controller.reconcile(start).await;
        assert_eq!(ops(&link).len(), before, "the bar shows what was asked, in the look it was asked in: {:?}", &ops(&link)[before..]);

        controller.handle(ControllerRequest::AmbientSelect { set: "ember".into() }, start);
        controller.reconcile(start).await;
        let bar = r#"{"completed":8,"empty":"base","look":"ember","op":"progress","pattern":"progress_ram","zone":"ram"}"#;
        assert!(ops(&link)[before..].iter().any(|op| op == bar), "{:?}", &ops(&link)[before..]);
        let after = ops(&link).len();
        controller.reconcile(start).await;
        assert!(!ops(&link)[after..].iter().any(|op| op.contains(r#""op":"progress""#)), "asked once: {:?}", &ops(&link)[after..]);
    }

    #[tokio::test]
    async fn the_controller_re_asserts_state_after_a_stack_restart() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy(AMBIENT);
        let mut controller = controller_with(&link);
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 10, priority: 0, pattern: None }, Instant::now());
        controller.handle(ControllerRequest::JobProgress { id: "a".into(), completed: 5, total: None }, Instant::now());
        // The stack restarts: the gateway comes back with nothing running.
        *link.status.lock().unwrap() = healthy([None; 3]);
        controller.reconcile(Instant::now()).await;
        // The eye is claimed by the job-running indicator instead of the ambient batch.
        assert_eq!(
            ops(&link),
            vec![
                r#"{"functions":["working_eye"],"op":"preempt_set"}"#,
                r#"{"functions":["ambient_strip"],"op":"preempt_set"}"#,
                r#"{"completed":16,"empty":"base","look":"deep_violet","op":"progress","pattern":"progress_ram","zone":"ram"}"#,
            ]
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
        // A job is leasing ram, so the boot-scene plan has four actions: the eye's
        // job-running indicator (state layer, always first and a different op, so it is
        // not refused), Stop(boot_proof), PreemptSet([strip]), Progress{ram}. Refusing
        // "stop" must stop the planner from attempting the last two this pass.
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy([Some("boot_proof"); 3]);
        *link.refuse.lock().unwrap() = Some("stop".to_owned());
        let mut controller = controller_with(&link);
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 4, priority: 0, pattern: None }, Instant::now());
        controller.reconcile(Instant::now()).await;
        assert_eq!(ops(&link).len(), 2, "the eye's indicator succeeds; the preempt_set and progress must not be attempted after the refused stop");
        assert!(controller.report.recent.back().unwrap().contains("refused"));
        *link.refuse.lock().unwrap() = None;
        controller.reconcile(Instant::now()).await;
        // FakeLink's simulated status never reflects a sent command's effect (it is a
        // fixed fixture, not a real gateway), so the retry replans from scratch: the
        // eye's indicator, the stop, the preempt_set, and the progress, four more ops.
        assert_eq!(ops(&link).len(), 6, "the full four-action plan is retried, on top of the two ops already sent");
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
        let reply = controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "Backup".into(), total: 10, priority: 0, pattern: None }, now);
        assert_eq!((reply["ok"].clone(), reply["job"]["state"].clone(), reply["job"]["zone"].clone()), (json!(true), json!("leased"), json!("ram")));
        controller.handle(ControllerRequest::JobStart { id: "b".into(), label: "x".into(), total: 10, priority: 0, pattern: None }, now);
        let third = controller.handle(ControllerRequest::JobStart { id: "c".into(), label: "x".into(), total: 10, priority: 0, pattern: None }, now);
        assert_eq!(third["job"]["state"], "queued");

        let unknown = controller.handle(ControllerRequest::JobProgress { id: "zzz".into(), completed: 1, total: None }, now);
        assert_eq!((unknown["ok"].clone(), unknown["code"].clone()), (json!(false), json!("unknown_job")));
        assert_eq!(controller.handle(ControllerRequest::JobStart { id: "z".into(), label: "x".into(), total: 0, priority: 0, pattern: None }, now)["code"], "bad_request");
        assert_eq!(controller.handle(ControllerRequest::AmbientSelect { set: "nope".into() }, now)["code"], "unknown_set");
        assert_eq!(controller.handle(ControllerRequest::AmbientSelect { set: "deep_violet".into() }, now)["ok"], true);

        assert_eq!(controller.handle(ControllerRequest::JobStart { id: "y".into(), label: "x".into(), total: 10, priority: 0, pattern: Some("nonsense".into()) }, now)["code"], "unknown_pattern");
        assert!(!controller.planner.jobs().iter().any(|job| job.id == "y"), "the job must not be created on a rejected pattern");

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
            ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 3, priority: 0, pattern: None }
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
        assert_eq!(
            client_request(&words("job-start a Backup 10 2 --pattern progress_ram_interleaved")).unwrap(),
            json!({ "op": "job.start", "id": "a", "label": "Backup", "total": 10, "priority": 2, "pattern": "progress_ram_interleaved" })
        );
        assert_eq!(
            client_request(&words("job-start a Backup 10 --pattern progress_ram_interleaved")).unwrap(),
            json!({ "op": "job.start", "id": "a", "label": "Backup", "total": 10, "pattern": "progress_ram_interleaved" }),
            "the pattern flag works without an explicit priority too"
        );
        assert!(client_request(&words("job-start a Backup 10 --pattern")).is_err(), "--pattern needs a value");
        assert_eq!(client_request(&words("job-progress a 4 8")).unwrap(), json!({ "op": "job.progress", "id": "a", "completed": 4, "total": 8 }));
        assert_eq!(client_request(&words("job-fail a disk is full")).unwrap(), json!({ "op": "job.fail", "id": "a", "reason": "disk is full" }));
        assert_eq!(client_request(&words("ambient deep_violet")).unwrap(), json!({ "op": "ambient.select", "set": "deep_violet" }));
        assert!(client_request(&words("job-start a Backup")).is_err());
        assert!(client_request(&words("job-fail a")).is_err());
        assert!(client_request(&words("job-progress a many")).is_err());
        assert_eq!(
            client_request(&words("fault-raise psu fault PSU over temperature")).unwrap(),
            json!({ "op": "fault.raise", "id": "psu", "severity": "fault", "reason": "PSU over temperature" })
        );
        assert_eq!(
            client_request(&words("fault-raise psu fault --zones ram,strip PSU over temperature")).unwrap(),
            json!({ "op": "fault.raise", "id": "psu", "severity": "fault", "reason": "PSU over temperature", "zones": ["ram", "strip"] })
        );
        assert_eq!(client_request(&words("fault-clear psu")).unwrap(), json!({ "op": "fault.clear", "id": "psu" }));
        assert_eq!(client_request(&words("quiet-set movie night")).unwrap(), json!({ "op": "quiet.set", "reason": "movie night" }));
        assert_eq!(client_request(&words("quiet-clear")).unwrap(), json!({ "op": "quiet.clear" }));
        assert!(client_request(&words("fault-raise psu fault")).is_err(), "a reason is required");
        assert!(client_request(&words("fault-raise psu fault --zones")).is_err(), "--zones needs a value");
        assert!(client_request(&words("quiet-set")).is_err());
    }

    // ------------------------------------------------------------ configuration

    #[test]
    fn the_empty_side_of_an_animated_bar_is_the_base_look_unless_configured() {
        let base = "version = 1\ndefault_ambient = \"deep_violet\"\nprogress_zones = [\"ram\"]\ncomplete_hold_seconds = 15\n";
        let parse = |extra: &str| toml::from_str::<ControllerConfig>(&format!("{base}{extra}"));
        assert_eq!(parse("").unwrap().progress_empty, ProgressEmpty::Base);
        assert_eq!(parse("progress_empty = \"working\"\n").unwrap().progress_empty.as_str(), "working");
        assert!(parse("progress_empty = \"sideways\"\n").is_err());
        assert_eq!(parse("").unwrap().working_look, WorkingLook::Violet);
        assert_eq!(parse("working_look = \"white\"\n").unwrap().working_look.prefix(), "working_white");
        assert!(check_config(&parse("working_look = \"white\"\n").unwrap(), &registry()).iter().any(|problem| problem.contains("working_white_eye is not registered")));
    }

    #[tokio::test]
    async fn a_working_bar_is_drawn_in_the_chosen_working_look() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy(AMBIENT);
        let config: ControllerConfig = toml::from_str(
            "version = 1\ndefault_ambient = \"deep_violet\"\nprogress_zones = [\"ram\", \"strip\"]\ncomplete_hold_seconds = 15\nprogress_empty = \"working\"\nworking_look = \"white\"\nworking_zones = [\"ram\"]\n",
        )
        .unwrap();
        let mut controller = Controller::new(link.clone(), registry(), config);
        let start = Instant::now();
        controller.handle(ControllerRequest::JobStart { id: "backup".into(), label: "Backup".into(), total: 32, priority: 0, pattern: None }, start);
        controller.handle(ControllerRequest::JobProgress { id: "backup".into(), completed: 8, total: None }, start);
        controller.reconcile(start).await;
        assert!(ops(&link).iter().any(|op| op.contains(r#""empty":"working_white""#)), "{:?}", ops(&link));
    }

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

    // ------------------------------------------------------------ warning/fault/quiet

    fn fault_raise(id: &str, severity: &str, zones: Option<&[&str]>, reason: &str) -> ControllerRequest {
        ControllerRequest::FaultRaise {
            id: id.to_owned(),
            severity: severity.to_owned(),
            zones: zones.map(|zones| zones.iter().map(|zone| (*zone).to_owned()).collect()),
            reason: reason.to_owned(),
        }
    }

    fn fault_clear(id: &str) -> ControllerRequest {
        ControllerRequest::FaultClear { id: id.to_owned() }
    }

    #[test]
    fn a_fault_with_no_zones_given_claims_every_managed_zone_by_default() {
        let mut controller = controller_with(&FakeLink::default());
        let reply = controller.handle(fault_raise("disk", "fault", None, "disk full"), Instant::now());
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["zones"], json!(["ram", "rog_eye", "strip"]));
        let state = controller.state_wants();
        assert_eq!(state, BTreeMap::from([("ram".to_owned(), "fault_ram".to_owned()), ("rog_eye".to_owned(), "fault_eye".to_owned()), ("strip".to_owned(), "fault_strip".to_owned())]));
    }

    #[test]
    fn a_warning_with_no_zones_given_defaults_to_the_eye_only() {
        let mut controller = controller_with(&FakeLink::default());
        let reply = controller.handle(fault_raise("thermal", "warning", None, "running warm"), Instant::now());
        assert_eq!(reply["zones"], json!(["rog_eye"]));
        assert_eq!(controller.state_wants(), BTreeMap::from([("rog_eye".to_owned(), "warning_eye".to_owned())]));
    }

    #[test]
    fn warning_zones_is_owner_editable_and_can_claim_the_whole_machine() {
        let link = FakeLink::default();
        let mut controller = Controller::new(link.clone(), registry_with_whole_machine_states(), config_with_whole_machine_states());
        let reply = controller.handle(fault_raise("thermal", "warning", None, "running warm"), Instant::now());
        assert_eq!(reply["zones"], json!(["ram", "rog_eye", "strip"]));
        assert_eq!(
            controller.state_wants(),
            BTreeMap::from([
                ("ram".to_owned(), "warning_ram".to_owned()),
                ("rog_eye".to_owned(), "warning_eye".to_owned()),
                ("strip".to_owned(), "warning_strip".to_owned()),
            ])
        );
    }

    #[test]
    fn working_zones_is_owner_editable_and_can_claim_the_whole_machine() {
        // The job leases "ram" (config's progress_zones puts it first), so the
        // indicator must not claim ram too -- see the next test for that half of the
        // behavior. This one just proves working_zones reaches beyond the eye at all.
        let link = FakeLink::default();
        let mut controller = Controller::new(link.clone(), registry_with_whole_machine_states(), config_with_whole_machine_states());
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 10, priority: 0, pattern: None }, Instant::now());
        assert_eq!(
            controller.state_wants(),
            BTreeMap::from([("rog_eye".to_owned(), "working_eye".to_owned()), ("strip".to_owned(), "working_strip".to_owned())])
        );
    }

    #[test]
    fn a_zones_own_progress_lease_wins_over_the_generic_working_indicator() {
        // Per the owner (2026-09-22): "progress bars if configured for a zone get
        // shown rather than the base scene" -- reversing the working indicator's
        // priority on the specific zone a job actually occupies, not everywhere.
        let link = FakeLink::default();
        let mut controller = Controller::new(link.clone(), registry_with_whole_machine_states(), config_with_whole_machine_states());
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 10, priority: 0, pattern: None }, Instant::now());
        // ram holds the real lease: state_wants() must not claim it for the indicator...
        assert!(!controller.state_wants().contains_key("ram"), "{:?}", controller.state_wants());
        // ...so wants()'s own progress entry for ram is free to take effect instead.
        assert!(matches!(controller.wants().get("ram"), Some(Want::Progress { .. })), "{:?}", controller.wants());
        // rog_eye and strip have no lease, so the indicator still covers them normally.
        assert_eq!(
            controller.state_wants(),
            BTreeMap::from([("rog_eye".to_owned(), "working_eye".to_owned()), ("strip".to_owned(), "working_strip".to_owned())])
        );
    }

    #[test]
    fn requests_are_rejected_for_unknown_severities_zones_or_missing_assets() {
        let mut controller = controller_with(&FakeLink::default());
        let now = Instant::now();
        assert_eq!(controller.handle(fault_raise("a", "critical", None, "x"), now)["code"], "bad_request");
        assert_eq!(controller.handle(fault_raise("a", "fault", Some(&["nowhere"]), "x"), now)["code"], "bad_request");
        // This fixture registers a warning asset only for the eye (production has all three
        // since 2026-09-22): asking for a zone with no matching asset must be refused, not silently substituted.
        let reply = controller.handle(fault_raise("a", "warning", Some(&["ram"]), "x"), now);
        assert_eq!(reply["code"], "bad_request");
        assert!(reply["error"].as_str().unwrap().contains("warning_ram"), "{reply}");
        assert_eq!(controller.handle(fault_raise("a", "fault", Some(&[]), "x"), now)["code"], "bad_request", "an empty zone list is not a silent default");
    }

    #[test]
    fn clearing_an_unknown_fault_is_refused_and_a_known_one_releases_its_zones() {
        let mut controller = controller_with(&FakeLink::default());
        let now = Instant::now();
        assert_eq!(controller.handle(fault_clear("nope"), now)["code"], "unknown_fault");
        controller.handle(fault_raise("a", "fault", Some(&["ram"]), "x"), now);
        assert!(!controller.state_wants().is_empty());
        let reply = controller.handle(fault_clear("a"), now);
        assert_eq!(reply["ok"], true);
        assert!(controller.state_wants().is_empty());
        assert_eq!(controller.handle(fault_clear("a"), now)["code"], "unknown_fault", "already cleared");
    }

    #[test]
    fn quiet_set_and_clear_toggle_every_zone_and_report_the_reason() {
        let mut controller = controller_with(&FakeLink::default());
        let now = Instant::now();
        let reply = controller.handle(ControllerRequest::QuietSet { reason: "movie night".into() }, now);
        assert_eq!(reply["ok"], true);
        assert_eq!(
            controller.state_wants(),
            BTreeMap::from([("ram".to_owned(), "quiet_ram".to_owned()), ("rog_eye".to_owned(), "quiet_eye".to_owned()), ("strip".to_owned(), "quiet_strip".to_owned())])
        );
        assert_eq!(controller.status_json()["quiet"], "movie night");
        controller.handle(ControllerRequest::QuietClear, now);
        assert!(controller.state_wants().is_empty());
        assert_eq!(controller.status_json()["quiet"], Value::Null);
    }

    #[test]
    fn a_running_job_shows_white_on_the_eye_via_its_own_working_asset() {
        let mut controller = controller_with(&FakeLink::default());
        assert!(!controller.state_wants().contains_key("rog_eye"), "nothing running yet");
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 4, priority: 0, pattern: None }, Instant::now());
        assert_eq!(controller.state_wants()["rog_eye"], "working_eye", "its own asset, not a reuse of warning_eye");
        // Still in the planner during the completion hold, so still shown.
        controller.handle(ControllerRequest::JobComplete { id: "a".into() }, Instant::now());
        assert_eq!(controller.state_wants()["rog_eye"], "working_eye", "the hold still counts as running");
    }

    #[test]
    fn quiet_and_a_real_fault_still_win_over_the_running_job_indicator() {
        let mut controller = controller_with(&FakeLink::default());
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 4, priority: 0, pattern: None }, Instant::now());
        assert_eq!(controller.state_wants()["rog_eye"], "working_eye");

        controller.handle(ControllerRequest::QuietSet { reason: "recording".into() }, Instant::now());
        assert_eq!(controller.state_wants()["rog_eye"], "quiet_eye", "an explicit quiet request must suppress the cosmetic hint too");
        controller.handle(ControllerRequest::QuietClear, Instant::now());

        controller.handle(fault_raise("f", "fault", Some(&["rog_eye"]), "x"), Instant::now());
        assert_eq!(controller.state_wants()["rog_eye"], "fault_eye", "a real fault still wins over the automatic job indicator");
    }

    #[test]
    fn a_real_warning_is_visually_distinguishable_from_the_job_indicator() {
        // Different colors, different assets: raising a real operator warning (amber)
        // while a job is running (white) must show warning_eye, not working_eye, and
        // clearing the warning must fall back to working_eye (white again, now
        // provably the job's doing, not a stuck warning) rather than silently looking
        // like nothing happened.
        let mut controller = controller_with(&FakeLink::default());
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 4, priority: 0, pattern: None }, Instant::now());
        assert_eq!(controller.state_wants()["rog_eye"], "working_eye");
        controller.handle(fault_raise("w", "warning", Some(&["rog_eye"]), "thermal"), Instant::now());
        assert_eq!(controller.state_wants()["rog_eye"], "warning_eye", "the real warning wins, and is a distinct asset from the job indicator");
        controller.handle(fault_clear("w"), Instant::now());
        assert_eq!(controller.state_wants()["rog_eye"], "working_eye", "falls back to the job indicator, not to nothing, since the job is still running");
    }

    #[test]
    fn the_pre_sleep_cue_sits_below_quiet_and_warnings_and_above_the_job_indicator() {
        let mut controller = controller_with(&FakeLink::default());
        let now = Instant::now();
        controller.handle(ControllerRequest::JobStart { id: "a".into(), label: "x".into(), total: 4, priority: 0, pattern: None }, now);
        assert_eq!(controller.handle(ControllerRequest::PreSleepSet { reason: "suspending at 03:00".into(), seconds: 300 }, now)["ok"], true);
        let cue = BTreeMap::from([("ram".to_owned(), "pre_sleep_ram".to_owned()), ("rog_eye".to_owned(), "pre_sleep_eye".to_owned()), ("strip".to_owned(), "pre_sleep_strip".to_owned())]);
        assert_eq!(controller.state_wants(), cue, "over the job indicator");
        assert_eq!(controller.status_json()["pre_sleep"], "suspending at 03:00");
        controller.handle(fault_raise("w", "warning", Some(&["rog_eye"]), "x"), now);
        assert_eq!(controller.state_wants()["rog_eye"], "warning_eye", "a warning beats the cue");
        controller.handle(ControllerRequest::QuietSet { reason: "x".into() }, now);
        assert_eq!(controller.state_wants()["ram"], "quiet_ram", "quiet beats the cue");
        controller.handle(ControllerRequest::QuietClear, now);
        controller.handle(ControllerRequest::PreSleepClear, now);
        assert!(!controller.state_wants().contains_key("ram"));
        assert_eq!(controller.status_json()["pre_sleep"], Value::Null);
    }

    #[tokio::test]
    async fn the_pre_sleep_cue_expires_and_every_wake_clears_it() {
        let link = FakeLink::default();
        let mut controller = controller_with(&link);
        let now = Instant::now();
        controller.handle(ControllerRequest::PreSleepSet { reason: "x".into(), seconds: 60 }, now);
        controller.reconcile(now + Duration::from_secs(59)).await;
        assert!(controller.state_wants().contains_key("ram"), "still inside its time");
        controller.reconcile(now + Duration::from_secs(60)).await;
        assert!(controller.state_wants().is_empty(), "a sleep policy that stopped renewing cannot leave it up");
        controller.handle(ControllerRequest::PreSleepSet { reason: "x".into(), seconds: 300 }, now);
        controller.handle(ControllerRequest::Pause, now);
        controller.handle(ControllerRequest::Resume, now);
        assert!(controller.state_wants().is_empty(), "the watchdog's resume after a wake clears it");
        assert_eq!(controller.handle(ControllerRequest::PreSleepSet { reason: "x".into(), seconds: 0 }, now)["code"], "bad_request");
        assert_eq!(controller.handle(ControllerRequest::PreSleepSet { reason: "x".into(), seconds: 3601 }, now)["code"], "bad_request");
        assert_eq!(client_request(&["presleep-set", "300", "suspending", "soon"].map(String::from)).unwrap(), json!({ "op": "presleep.set", "seconds": 300, "reason": "suspending soon" }));
        assert_eq!(client_request(&["presleep-clear".to_owned()]).unwrap(), json!({ "op": "presleep.clear" }));
    }

    #[test]
    fn priority_is_fault_over_warning_over_quiet() {
        let mut controller = controller_with(&FakeLink::default());
        let now = Instant::now();
        controller.handle(ControllerRequest::QuietSet { reason: "x".into() }, now);
        controller.handle(fault_raise("w", "warning", Some(&["rog_eye"]), "x"), now);
        assert_eq!(controller.state_wants()["rog_eye"], "warning_eye", "warning beats quiet on the shared zone");
        assert_eq!(controller.state_wants()["ram"], "quiet_ram", "quiet still covers what nothing else claims");
        controller.handle(fault_raise("f", "fault", Some(&["rog_eye"]), "x"), now);
        assert_eq!(controller.state_wants()["rog_eye"], "fault_eye", "fault beats warning on the shared zone");
    }

    #[tokio::test]
    async fn a_fault_preempts_a_running_job_and_the_bar_resumes_unharmed_after_it_clears() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy(AMBIENT);
        let mut controller = controller_with(&link);
        let start = Instant::now();
        controller.handle(ControllerRequest::JobStart { id: "backup".into(), label: "Backup".into(), total: 32, priority: 0, pattern: None }, start);
        controller.handle(ControllerRequest::JobProgress { id: "backup".into(), completed: 16, total: None }, start);
        controller.reconcile(start).await;
        // The eye picks up the job-running indicator in the same pass, ahead of the
        // progress action (the state layer always goes first).
        assert_eq!(ops(&link), vec![r#"{"functions":["working_eye"],"op":"preempt_set"}"#, r#"{"completed":16,"empty":"base","look":"deep_violet","op":"progress","pattern":"progress_ram","zone":"ram"}"#]);

        // RAM is now showing the bar; raise a RAM fault.
        *link.status.lock().unwrap() = healthy([Some("progress_ram:16"), Some("ambient_eye"), Some("ambient_strip")]);
        controller.handle(fault_raise("psu", "fault", Some(&["ram"]), "PSU over temperature"), start);
        controller.reconcile(start).await;
        // fault_ram and the still-active job's eye indicator both land in the same
        // preempt_set call: unrelated zone claims changing at once still batch together.
        assert_eq!(ops(&link).last().unwrap(), r#"{"functions":["fault_ram","working_eye"],"op":"preempt_set"}"#, "preempted, not a rejoin: fault_ram is not an ambient-set member");
        assert!(!controller.planner.jobs().is_empty(), "the lease is untouched; only the display was preempted");

        // Clear it: the zone must go straight back to the progress bar, unharmed.
        *link.status.lock().unwrap() = healthy([Some("fault_ram"), Some("ambient_eye"), Some("ambient_strip")]);
        controller.handle(fault_clear("psu"), start);
        controller.reconcile(start).await;
        assert_eq!(ops(&link).last().unwrap(), r#"{"completed":16,"empty":"base","look":"deep_violet","op":"progress","pattern":"progress_ram","zone":"ram"}"#);
    }

    #[tokio::test]
    async fn a_quiet_request_preempts_the_whole_ambient_set_and_reconciles_in_one_pass() {
        let link = FakeLink::default();
        *link.status.lock().unwrap() = healthy(AMBIENT);
        let mut controller = controller_with(&link);
        controller.handle(ControllerRequest::QuietSet { reason: "recording".into() }, Instant::now());
        controller.reconcile(Instant::now()).await;
        // All three zones go dark together in one preempt_set call, not three
        // individually confirmed replaces (found live 2026-09-22: those showed a
        // visible zone-to-zone gap, the same staggering bug as the resume direction).
        let sent = ops(&link);
        assert_eq!(sent, vec![r#"{"functions":["quiet_ram","quiet_eye","quiet_strip"],"op":"preempt_set"}"#]);

        let after_first_reconcile = ops(&link).len();
        *link.status.lock().unwrap() = healthy([Some("quiet_ram"), Some("quiet_eye"), Some("quiet_strip")]);
        controller.handle(ControllerRequest::QuietClear, Instant::now());
        controller.reconcile(Instant::now()).await;
        // Back to ambient: nothing in the set was running, so the whole set is stopped
        // and started together in one preempt_set call (not three individually
        // phase-aligned rejoins, which would each align to the last and stagger the
        // bring-up by a full cycle per zone).
        let sent = ops(&link)[after_first_reconcile..].to_vec();
        assert_eq!(sent, vec![r#"{"functions":["ambient_ram","ambient_eye","ambient_strip"],"op":"preempt_set"}"#]);
    }

    #[tokio::test]
    async fn a_preempted_zone_is_left_out_of_the_uncertain_check_for_other_zones() {
        // Regression guard: plan() must fold state zones into its readiness check too.
        let link = FakeLink::default();
        let mut status = healthy(AMBIENT);
        status["zones"]["ram"]["uncertain"] = json!(true);
        *link.status.lock().unwrap() = status;
        let mut controller = controller_with(&link);
        controller.handle(fault_raise("f", "fault", Some(&["ram"]), "x"), Instant::now());
        controller.reconcile(Instant::now()).await;
        assert!(ops(&link).is_empty(), "an uncertain zone must block even a preemption action");
        assert!(controller.report.blocked.as_deref().unwrap().contains("unconfirmed"));
    }

    #[test]
    fn state_wants_falls_back_silently_when_an_asset_is_somehow_missing() {
        // resolve_state_zones already refuses this at raise time; state_wants defends in depth
        // in case a fault is constructed without going through it (defensive, not reachable via the API).
        let mut controller = controller_with(&FakeLink::default());
        controller.faults.insert("x".to_owned(), ActiveFault { severity: Severity::Fault, zones: BTreeSet::from(["gpu_bracket".to_owned()]), reason: "x".to_owned() });
        assert!(controller.state_wants().is_empty(), "no fault_bracket asset exists, so nothing is planned for it");
    }
}
