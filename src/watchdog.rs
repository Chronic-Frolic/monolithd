//! Independent health and sleep-lifecycle watchdog.
//!
//! A second, independent sensor loop: it never holds the setpoint (the controller
//! does), it only reads controller-process liveness and the sleep/wake signal, and
//! it has exactly one actuator path — the same scene gateway the controller uses.
//! It must never start or restart the controller.
//!
//! Reuses the mechanics of the retired Python watchdog (physically verified
//! 2026-09-19): `systemd-inhibit --mode=delay` held as a child process while awake,
//! and `gdbus monitor` on `org.freedesktop.login1`'s `PrepareForSleep` signal. Both
//! are subprocesses, so this needs no D-Bus crate.
//!
//! One narrow exception to the single-actuator-path rule: `prepare_sleep`/`resume`
//! also send the controller's own `pause`/`resume` control-socket ops (2026-09-22),
//! so its independent ~1 s reconcile loop cannot race the pre-sleep quiet handoff.
//! This does not start or restart the controller process -- it only holds the
//! already-running controller's own loop for the length of the sleep transition.

use crate::controller::zone_suffix;
use crate::registry::Registry;
use crate::{config, controller, gateway, paths, registry};
use serde::Deserialize;
use serde_json::{json, Value};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::interval;

const HEALTH_INTERVAL: Duration = Duration::from_secs(10);
const RESUME_GRACE: Duration = Duration::from_secs(15);
/// How long a gdbus/systemd-inhibit child that exited unexpectedly is waited out
/// before it is relaunched, mirroring the old watchdog's respawn backoff.
const RESPAWN_BACKOFF: Duration = Duration::from_secs(2);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------- config/watchdog.toml

/// `watchdog.toml`: the zones the watchdog takes over, with `controller_fault_<zone>`
/// while the controller is down and `quiet_<zone>` around sleep. Its own file, not a
/// key in `controller.toml`, so a broken controller policy cannot also blind the one
/// process that shows it is broken (owner decision 2026-09-26).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchdogConfig {
    version: u32,
    zones: Vec<String>,
}

pub fn parse_config(text: &str) -> Result<Vec<String>, String> {
    let config: WatchdogConfig = toml::from_str(text).map_err(|error| error.to_string())?;
    if config.version != 1 {
        return Err(format!("version {} is unsupported (expected 1)", config.version));
    }
    Ok(config.zones)
}

/// Every disagreement with the registry: each zone must be real, listed once, and have
/// both looks the watchdog selects.
pub fn check_zones(zones: &[String], registry: &Registry) -> Vec<String> {
    let mut problems = Vec::new();
    controller::check_state_zones(&mut problems, "zones", zones, "controller_fault", registry);
    controller::check_state_zones(&mut problems, "zones", zones, "quiet", registry);
    problems.dedup();
    problems
}

/// The zones in use, set once at start.
static ZONES: OnceLock<Vec<String>> = OnceLock::new();

/// The watchdog's zones and any problem finding them. Never fatal: the watchdog must run
/// even when configuration is broken, since it is what shows that something is.
fn load_zones(root: &Path) -> (Vec<String>, Option<String>) {
    let path = root.join("watchdog.toml");
    let configured = std::fs::read_to_string(&path).map_err(|error| error.to_string()).and_then(|text| parse_config(&text)).map_err(|error| format!("{}: {error}", path.display()));
    let registry = config::load_layout(&root.join("scene-layout.toml")).and_then(|layout| registry::load_and_validate(&root.join("qlc-functions.toml"), &layout).map(|(registry, _)| registry));
    let available = config::load_layout(&root.join("scene-layout.toml")).map(|layout| layout.zones.iter().filter(|(_, zone)| zone.available).map(|(name, _)| name.clone()).collect());
    choose_zones(configured, registry, available)
}

/// If `watchdog.toml` is unreadable, or names a zone the registry cannot show, use every
/// available zone in `scene-layout.toml`. If the registry itself cannot be loaded, use
/// the file's zones unchecked.
fn choose_zones(configured: Result<Vec<String>, String>, registry: Result<Registry, String>, available: Result<Vec<String>, String>) -> (Vec<String>, Option<String>) {
    let fallback = |problem: String| match &available {
        Ok(zones) => (zones.clone(), Some(format!("{problem}; using every available zone instead: {}", zones.join(", ")))),
        Err(error) => (Vec::new(), Some(format!("{problem}; and no fallback ({error}), so no zone can show a Controller Fault"))),
    };
    let zones = match configured {
        Ok(zones) => zones,
        Err(problem) => return fallback(problem),
    };
    match registry {
        Err(error) => (zones, Some(format!("zones not checked against the registry: {error}"))),
        Ok(registry) => match check_zones(&zones, &registry).into_iter().next() {
            None => (zones, None),
            Some(problem) => fallback(format!("watchdog.toml: {problem}")),
        },
    }
}

fn runtime_directory() -> Result<PathBuf, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(runtime).join("monolith-events"))
}

fn heartbeat_path() -> Result<PathBuf, String> {
    Ok(runtime_directory()?.join("watchdog-status.json"))
}

/// Holds a systemd-logind sleep inhibitor as a child process, released by
/// terminating the child: `systemd-inhibit --mode=<mode> ... sleep infinity`,
/// exactly as the retired Python watchdog did for its one, unconditional
/// `--mode=delay` inhibitor. Generalized (2026-09-22) so a second instance can hold
/// `--mode=block` conditionally, while a job is active, instead of duplicating this
/// spawn/acquire/release mechanic a second time.
struct Inhibitor {
    mode: &'static str,
    who: &'static str,
    why: &'static str,
    child: Option<Child>,
}

impl Inhibitor {
    fn new(mode: &'static str, who: &'static str, why: &'static str) -> Self {
        Self { mode, who, why, child: None }
    }

    /// Whether the inhibitor is currently held (the child is alive).
    fn held(&mut self) -> bool {
        self.child.as_mut().is_some_and(|child| child.try_wait().ok().flatten().is_none())
    }

    async fn acquire(&mut self) -> Result<(), String> {
        if self.held() {
            return Ok(());
        }
        let mode_flag = format!("--mode={}", self.mode);
        let who_flag = format!("--who={}", self.who);
        let why_flag = format!("--why={}", self.why);
        let mut child = Command::new("systemd-inhibit")
            .args(["--what=sleep", &mode_flag, &who_flag, &why_flag, "/usr/bin/sleep", "infinity"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn systemd-inhibit: {error}"))?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let _ = pipe.read_to_string(&mut stderr).await;
            }
            return Err(format!("systemd-inhibit exited {status}: {}", stderr.trim()));
        }
        self.child = Some(child);
        Ok(())
    }

    async fn release(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
    }
}

#[derive(Debug, Clone)]
struct SharedState {
    suspended: bool,
    fault_active: bool,
    last_error: Option<String>,
    /// A problem with `watchdog.toml`, reported until the watchdog restarts.
    config_problem: Option<String>,
    /// Whether the block-mode sleep inhibitor is currently held because a job is
    /// active. Reflects the inhibitor's actual state, not just "is a job running",
    /// so a failure to acquire it is visible here rather than silently assumed ok.
    jobs_blocking_sleep: bool,
}

impl Default for SharedState {
    fn default() -> Self {
        Self { suspended: false, fault_active: false, last_error: None, config_problem: None, jobs_blocking_sleep: false }
    }
}

fn write_heartbeat(state: &SharedState) {
    match heartbeat_path() {
        Ok(path) => write_heartbeat_to(&path, state),
        Err(error) => eprintln!("monolithd watchdog: cannot place heartbeat file: {error}"),
    }
}

fn write_heartbeat_to(path: &std::path::Path, state: &SharedState) {
    let directory = path.parent().unwrap_or(path);
    if let Err(error) = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(directory) {
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            eprintln!("monolithd watchdog: create {}: {error}", directory.display());
            return;
        }
    }
    let updated_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let body = json!({
        "updated_at": updated_at,
        "suspended": state.suspended,
        "controller_fault_active": state.fault_active,
        "last_error": state.last_error,
        "config_problem": state.config_problem,
        "jobs_blocking_sleep": state.jobs_blocking_sleep,
    });
    let temporary = directory.join(format!(".watchdog-status.{}.tmp", std::process::id()));
    let write = std::fs::write(&temporary, format!("{body}\n"))
        .and_then(|()| std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)))
        .and_then(|()| std::fs::rename(&temporary, path));
    if let Err(error) = write {
        eprintln!("monolithd watchdog: write {}: {error}", path.display());
    }
}

/// Ask the gateway to preempt every managed zone with the named severity's assets
/// (`fault_*` or `quiet_*`), all in one gateway call: the zones change together
/// instead of staggering one at a time, the same one-zone-at-a-time pattern found
/// live 2026-09-22 and already fixed in the controller's own fault/warning/quiet layer.
async fn preempt_all_zones(prefix: &str) -> Result<(), String> {
    let zones = ZONES.get().map(Vec::as_slice).unwrap_or_default();
    let functions: Vec<String> = zones.iter().map(|zone| format!("{prefix}_{}", zone_suffix(zone))).collect();
    let request = json!({ "op": "preempt_set", "functions": functions });
    match gateway::call(&request).await {
        Ok(reply) if reply["ok"] == Value::Bool(true) => Ok(()),
        Ok(reply) => Err(reply["error"].as_str().unwrap_or("refused").to_owned()),
        Err(error) => Err(error),
    }
}

async fn fetch_controller_status() -> Result<Value, String> {
    tokio::time::timeout(HEALTH_CHECK_TIMEOUT, controller::call(&json!({ "op": "status" })))
        .await
        .map_err(|_| "no answer within 3 s".to_owned())?
}

/// Bounded ping of the controller: reachable and answering `status` counts as healthy.
async fn controller_healthy() -> Result<(), String> {
    let reply = fetch_controller_status().await?;
    if reply["ok"] == Value::Bool(true) {
        Ok(())
    } else {
        Err(reply["error"].as_str().unwrap_or("controller reported an error").to_owned())
    }
}

/// Whether a `status` reply reports any job at all (leased, queued, or holding at
/// completion). Any job counts, not just a leased one: a queued job means more work
/// is coming, and a completing job's cosmetic 15 s hold is a negligible extra delay
/// against the cost of ever suspending mid-transfer.
fn jobs_active_in(status: &Value) -> bool {
    status["jobs"].as_array().is_some_and(|jobs| !jobs.is_empty())
}

async fn apply_controller_fault(state: &Mutex<SharedState>, reason: &str) {
    let mut guard = state.lock().await;
    if guard.fault_active {
        guard.last_error = Some(reason.to_owned());
        write_heartbeat(&guard);
        return;
    }
    match preempt_all_zones("controller_fault").await {
        Ok(()) => eprintln!("monolithd watchdog: Controller Fault applied: {reason}"),
        Err(error) => eprintln!("monolithd watchdog: Controller Fault requested ({reason}) but the gateway refused it too: {error}"),
    }
    guard.fault_active = true;
    guard.last_error = Some(reason.to_owned());
    write_heartbeat(&guard);
}

async fn clear_controller_fault(state: &Mutex<SharedState>) {
    let mut guard = state.lock().await;
    if guard.fault_active {
        // No explicit "restore" call: the controller's own reconcile loop already
        // treats a leftover controller_fault_* owner as a foreign composable owner
        // and reasserts ambient/progress on its own, exactly like clearing a fault.
        eprintln!("monolithd watchdog: controller recovered");
        guard.fault_active = false;
    }
    guard.last_error = None;
    write_heartbeat(&guard);
}

/// How long the block inhibitor may be held continuously before it is force-
/// released regardless of job state. Nothing in allocator.rs expires a stuck
/// Leased job (a dead external reporter never calls job.complete/job.fail), so
/// without this cap the inhibitor could be held forever, making "the machine
/// never sleeps" a silent steady state -- the opposite of the reliability this
/// feature is for. Simpler and fails safer than trying to detect "is this job
/// still actually making progress": worst case, a real job that legitimately runs
/// longer than this gets suspended, exactly like today's behavior with no
/// feature at all.
const MAX_BLOCK_DURATION: Duration = Duration::from_secs(6 * 3600);

/// Pure: whether a block held since `held_since` has exceeded `cap` as of `now`.
/// Separated from JobBlock so the policy is testable without a real inhibitor.
fn cap_exceeded(held_since: Instant, now: Instant, cap: Duration) -> bool {
    now.saturating_duration_since(held_since) >= cap
}

/// The block-mode sleep inhibitor, held exactly while a job is active, capped at
/// MAX_BLOCK_DURATION. `capped` latches once the cap fires: the very next tick
/// (jobs_active still true, the same stuck job) must not just re-acquire and
/// silently defeat the cap. It only resets when jobs_active goes false and true
/// again -- a fresh job gets its own full window.
struct JobBlock {
    inhibitor: Inhibitor,
    held_since: Option<Instant>,
    capped: bool,
}

impl JobBlock {
    fn new() -> Self {
        Self { inhibitor: Inhibitor::new("block", "Monolith-Event-Watchdog", "a job is actively running"), held_since: None, capped: false }
    }

    async fn update(&mut self, jobs_active: bool) -> bool {
        if !jobs_active {
            if self.inhibitor.held() {
                self.inhibitor.release().await;
                eprintln!("monolithd watchdog: no jobs active; sleep is no longer blocked");
            }
            self.held_since = None;
            self.capped = false;
        } else if self.capped {
            // Already gave up blocking for this unbroken stretch of activity.
        } else if !self.inhibitor.held() {
            match self.inhibitor.acquire().await {
                Ok(()) => {
                    eprintln!("monolithd watchdog: a job is active; blocking sleep");
                    self.held_since = Some(Instant::now());
                }
                Err(error) => eprintln!("monolithd watchdog: could not block sleep for the active job: {error}"),
            }
        } else if self.held_since.is_some_and(|since| cap_exceeded(since, Instant::now(), MAX_BLOCK_DURATION)) {
            self.inhibitor.release().await;
            self.capped = true;
            eprintln!(
                "monolithd watchdog: a job has blocked sleep for over {} h; releasing so the machine can still sleep (the job may be stuck)",
                MAX_BLOCK_DURATION.as_secs() / 3600
            );
        }
        self.inhibitor.held()
    }
}

/// Update the block inhibitor to match `jobs_active`, and record its actual
/// resulting state (not the intent) in `SharedState`.
async fn update_block_inhibitor(state: &Mutex<SharedState>, block: &Mutex<JobBlock>, jobs_active: bool) {
    let held_now = block.lock().await.update(jobs_active).await;
    let mut guard = state.lock().await;
    if guard.jobs_blocking_sleep != held_now {
        guard.jobs_blocking_sleep = held_now;
        write_heartbeat(&guard);
    }
}

async fn health_loop(state: Arc<Mutex<SharedState>>, block: Arc<Mutex<JobBlock>>) {
    let mut ticks = interval(HEALTH_INTERVAL);
    loop {
        ticks.tick().await;
        if state.lock().await.suspended {
            continue;
        }
        match fetch_controller_status().await {
            Ok(reply) if reply["ok"] == Value::Bool(true) => {
                clear_controller_fault(&state).await;
                update_block_inhibitor(&state, &block, jobs_active_in(&reply)).await;
            }
            Ok(reply) => {
                let error = reply["error"].as_str().unwrap_or("controller reported an error").to_owned();
                apply_controller_fault(&state, &error).await;
            }
            Err(error) => apply_controller_fault(&state, &error).await,
        }
    }
}

async fn prepare_sleep(state: &Mutex<SharedState>, delay: &Mutex<Inhibitor>) {
    state.lock().await.suspended = true;
    // Hold the controller's own reconcile loop first: it ticks independently, roughly
    // once a second, and would otherwise see the quiet call below as a mismatch
    // against its own wants (an active job's eye indicator, a progress bar, ...) and
    // immediately re-assert them in the race window before the machine actually
    // suspends, undoing this handoff.
    if let Err(error) = controller::call(&json!({ "op": "pause" })).await {
        eprintln!("monolithd watchdog: could not pause the controller before sleep: {error}");
    }
    match preempt_all_zones("quiet").await {
        Ok(()) => eprintln!("monolithd watchdog: quiet applied before sleep"),
        Err(error) => apply_controller_fault(state, &format!("pre-sleep handoff failed: {error}")).await,
    }
    delay.lock().await.release().await;
    write_heartbeat(&*state.lock().await);
}

/// `suspended` stays true for this whole function, not just until the inhibitor is
/// reacquired: it is what keeps the independent health_loop from probing the
/// controller at the same time this grace-period check does. Two uncoordinated
/// probes racing right after wake, while the controller is still doing legitimate
/// post-resume work, produced a spurious fault-then-clear flicker (found live,
/// 2026-09-22): the health_loop's own periodic tick landed mid-resume and timed out
/// a moment after this function's own check had already confirmed the controller
/// healthy. Only one check may be in flight during the grace window.
/// Poll `ping` for up to `grace`, one attempt per second. Pure with respect to
/// `SharedState` — the caller alone decides when `suspended` clears — so the
/// resume-race invariant (no concurrent probe while this is running) is testable
/// without a real controller, gateway, or delay inhibitor.
async fn poll_until_healthy<F, Fut>(grace: Duration, mut ping: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let deadline = tokio::time::Instant::now() + grace;
    let last_error = loop {
        let error = match ping().await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        if tokio::time::Instant::now() >= deadline {
            break error;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    Err(last_error)
}

/// `suspended` stays true for this whole function, not just until the inhibitor is
/// reacquired: it is what keeps the independent health_loop from probing the
/// controller at the same time this grace-period check does. Two uncoordinated
/// probes racing right after wake, while the controller is still doing legitimate
/// post-resume work, produced a spurious fault-then-clear flicker (found live,
/// 2026-09-22): the health_loop's own periodic tick landed mid-resume and timed out
/// a moment after this function's own check had already confirmed the controller
/// healthy. Only one check may be in flight during the grace window; see
/// poll_until_healthy and its regression test below.
async fn resume(state: &Mutex<SharedState>, delay: &Mutex<Inhibitor>) {
    if let Err(error) = delay.lock().await.acquire().await {
        eprintln!("monolithd watchdog: could not reacquire the sleep delay inhibitor: {error}");
    }
    let outcome = poll_until_healthy(RESUME_GRACE, controller_healthy).await;
    // Release the controller's reconcile loop regardless of outcome: leaving it
    // paused on a failed poll would mean nothing self-corrects again until a manual
    // resume. controller_healthy() above only asked whether the process answers its
    // socket at all, which it does even while paused, so this is safe either way.
    if let Err(error) = controller::call(&json!({ "op": "resume" })).await {
        eprintln!("monolithd watchdog: could not resume the controller after sleep: {error}");
    }
    {
        let mut guard = state.lock().await;
        guard.suspended = false;
        write_heartbeat(&guard);
    }
    match outcome {
        Ok(()) => {
            clear_controller_fault(state).await;
            eprintln!("monolithd watchdog: controller healthy after resume");
        }
        Err(last_error) => apply_controller_fault(state, &format!("controller did not recover after resume: {last_error}")).await,
    }
}

/// Parse one `gdbus monitor` line for `PrepareForSleep`'s boolean argument.
fn prepare_for_sleep_value(line: &str) -> Option<bool> {
    if !line.contains("PrepareForSleep") {
        return None;
    }
    if line.contains("true") {
        Some(true)
    } else if line.contains("false") {
        Some(false)
    } else {
        None
    }
}

/// Watch `org.freedesktop.login1`'s `PrepareForSleep` signal via `gdbus monitor`,
/// calling `on_signal(true)` just before suspend and `on_signal(false)` on resume.
/// Respawns the subprocess if it exits or fails to spawn (mirroring the old
/// watchdog's respawn backoff). Runs forever; the caller spawns this as its own
/// task. Generic so more than the watchdog can react to the same signal -- the
/// receiver also uses this to force an OpenRGB reconnect on resume (see e131.rs).
pub(crate) async fn watch_sleep_signal<F, Fut>(mut on_signal: F)
where
    F: FnMut(bool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    loop {
        let child = Command::new("gdbus")
            .args(["monitor", "--system", "--dest", "org.freedesktop.login1", "--object-path", "/org/freedesktop/login1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                eprintln!("monolithd: spawn gdbus monitor: {error}; retrying in {} s", RESPAWN_BACKOFF.as_secs());
                tokio::time::sleep(RESPAWN_BACKOFF).await;
                continue;
            }
        };
        let Some(stdout) = child.stdout.take() else { continue };
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(value) = prepare_for_sleep_value(&line) {
                on_signal(value).await;
            }
        }
        let _ = child.wait().await;
        eprintln!("monolithd: gdbus monitor exited; restarting in {} s", RESPAWN_BACKOFF.as_secs());
        tokio::time::sleep(RESPAWN_BACKOFF).await;
    }
}

async fn sleep_loop(state: Arc<Mutex<SharedState>>, delay: Arc<Mutex<Inhibitor>>) {
    if let Err(error) = delay.lock().await.acquire().await {
        eprintln!("monolithd watchdog: could not acquire the sleep delay inhibitor: {error}; sleep/wake handling is degraded");
    }
    watch_sleep_signal(|sleeping| {
        let state = state.clone();
        let delay = delay.clone();
        async move {
            if sleeping {
                prepare_sleep(&state, &delay).await;
            } else {
                resume(&state, &delay).await;
            }
        }
    })
    .await;
}

pub async fn run() -> Result<(), String> {
    eprintln!("monolithd watchdog: {}", paths::describe());
    let (zones, problem) = load_zones(&paths::config_dir());
    match &problem {
        Some(problem) => eprintln!("monolithd watchdog: CONFIG PROBLEM: {problem}"),
        None => eprintln!("monolithd watchdog: zones {}", zones.join(", ")),
    }
    let _ = ZONES.set(zones);
    let state = Arc::new(Mutex::new(SharedState { config_problem: problem, ..SharedState::default() }));
    write_heartbeat(&*state.lock().await);
    eprintln!("monolithd watchdog: watching controller health (every {} s) and sleep/wake", HEALTH_INTERVAL.as_secs());
    let delay = Arc::new(Mutex::new(Inhibitor::new("delay", "Monolith-Event-Watchdog", "RGB suspend handoff")));
    let block = Arc::new(Mutex::new(JobBlock::new()));
    tokio::join!(health_loop(state.clone(), block), sleep_loop(state, delay));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_sleep_signal_and_ignores_everything_else() {
        assert_eq!(prepare_for_sleep_value(r#"/org...: org.freedesktop.login1.Manager.PrepareForSleep (true,)"#), Some(true));
        assert_eq!(prepare_for_sleep_value(r#"/org...: org.freedesktop.login1.Manager.PrepareForSleep (false,)"#), Some(false));
        assert_eq!(prepare_for_sleep_value("some other signal entirely"), None);
        assert_eq!(prepare_for_sleep_value("PrepareForSleep with neither word present"), None);
    }

    #[test]
    fn jobs_active_in_reads_the_jobs_array_regardless_of_state() {
        assert!(!jobs_active_in(&json!({"ok": true, "jobs": []})));
        assert!(!jobs_active_in(&json!({"ok": true})), "a missing jobs field is not active jobs");
        assert!(jobs_active_in(&json!({"ok": true, "jobs": [{"id": "a", "state": "queued"}]})), "a queued job still counts");
        assert!(jobs_active_in(&json!({"ok": true, "jobs": [{"id": "a", "state": "completing"}]})), "a completing job's hold still counts");
    }

    #[tokio::test]
    async fn update_block_inhibitor_only_toggles_the_shared_state_on_an_actual_change() {
        // held() requires a real systemd-inhibit child, so this exercises the
        // no-job / already-released path (no acquire attempted) and confirms the
        // heartbeat is not rewritten when nothing changed.
        let state = Mutex::new(SharedState::default());
        let block = Mutex::new(JobBlock::new());
        update_block_inhibitor(&state, &block, false).await;
        assert!(!state.lock().await.jobs_blocking_sleep);
    }

    #[test]
    fn cap_exceeded_fires_at_the_cap_and_not_before() {
        let held_since = Instant::now();
        let cap = Duration::from_secs(60);
        assert!(!cap_exceeded(held_since, held_since + Duration::from_secs(59), cap));
        assert!(cap_exceeded(held_since, held_since + Duration::from_secs(60), cap));
        assert!(cap_exceeded(held_since, held_since + Duration::from_secs(3600), cap), "a stuck job stays capped, not just briefly over");
    }

    #[tokio::test]
    async fn a_stuck_job_is_capped_and_does_not_immediately_reacquire() {
        // Without a real systemd-inhibit binary this can't exercise acquire()
        // itself, but it proves the capped latch survives update() calls and does
        // not reset just because jobs_active is still true -- the exact bug this
        // cap exists to prevent (re-blocking the instant the cap releases).
        let mut block = JobBlock::new();
        block.capped = true;
        block.held_since = Some(Instant::now() - Duration::from_secs(3600));
        assert!(!block.update(true).await, "still capped, must not re-acquire while jobs_active stays true");
        assert!(block.capped, "the latch is not cleared by jobs_active alone");
        assert!(!block.update(false).await, "jobs_active going false clears it");
        assert!(!block.capped, "a fresh job (jobs_active false then true) gets its own window");
    }

    #[tokio::test]
    async fn a_missing_controller_applies_fault_once_and_a_recovery_clears_it() {
        let state = Mutex::new(SharedState::default());
        apply_controller_fault(&state, "connection refused").await;
        {
            let guard = state.lock().await;
            assert!(guard.fault_active);
            assert_eq!(guard.last_error.as_deref(), Some("connection refused"));
        }
        // A second failure while already faulted updates the reason but does not re-log "applied".
        apply_controller_fault(&state, "still refused").await;
        assert_eq!(state.lock().await.last_error.as_deref(), Some("still refused"));

        clear_controller_fault(&state).await;
        let guard = state.lock().await;
        assert!(!guard.fault_active);
        assert_eq!(guard.last_error, None);
    }

    #[tokio::test]
    async fn health_loop_cannot_probe_while_a_resume_health_check_is_in_progress() {
        // Simulates resume()'s own state handling (minus the delay inhibitor, which this
        // race does not involve) alongside a simulated health_loop tick firing every
        // 150 ms — faster than poll_until_healthy's own 1 s retry spacing, so if the old
        // bug (clearing `suspended` before the health check finished) were still present,
        // at least one tick would see `suspended == false` while the check is still
        // running and would have raced a second probe against it.
        let state = Arc::new(Mutex::new(SharedState { suspended: true, ..SharedState::default() }));
        let attempt = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let ping = {
            let attempt = attempt.clone();
            move || {
                let attempt = attempt.clone();
                async move {
                    // Healthy only on the second attempt: the controller is still doing
                    // legitimate post-resume work on the first, exactly like the live case.
                    if attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 { Err("still starting".to_owned()) } else { Ok(()) }
                }
            }
        };

        let resume_task = {
            let state = state.clone();
            tokio::spawn(async move {
                let outcome = poll_until_healthy(Duration::from_secs(5), ping).await;
                state.lock().await.suspended = false;
                outcome
            })
        };

        let mut would_have_raced = 0;
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            if !state.lock().await.suspended {
                would_have_raced += 1;
            }
        }

        assert!(resume_task.await.unwrap().is_ok(), "the simulated resume must still succeed on its second attempt");
        assert_eq!(would_have_raced, 0, "a concurrent health_loop-style check must never see suspended=false before the resume check finished");
    }

    #[tokio::test]
    async fn clearing_an_already_clear_state_is_a_no_op() {
        let state = Mutex::new(SharedState::default());
        clear_controller_fault(&state).await;
        assert!(!state.lock().await.fault_active);
    }

    #[test]
    fn writes_an_atomic_0600_heartbeat_file_with_the_expected_shape() {
        let directory = std::env::temp_dir().join(format!("monolithd-watchdog-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = directory.join("watchdog-status.json");
        let state = SharedState { suspended: true, fault_active: true, last_error: Some("boom".to_owned()), config_problem: Some("bad zone".to_owned()), jobs_blocking_sleep: true };
        write_heartbeat_to(&path, &state);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            (body["suspended"].clone(), body["controller_fault_active"].clone(), body["last_error"].clone(), body["config_problem"].clone(), body["jobs_blocking_sleep"].clone()),
            (json!(true), json!(true), json!("boom"), json!("bad zone"), json!(true))
        );
        assert!(body["updated_at"].as_u64().unwrap() > 0);
        assert!(std::fs::read_dir(&directory).unwrap().filter_map(Result::ok).all(|entry| !entry.file_name().to_string_lossy().contains(".tmp")), "no leftover temp file");

        // Writing again must not leave the previous temp file behind either.
        write_heartbeat_to(&path, &state);
        assert_eq!(std::fs::read_dir(&directory).unwrap().filter_map(Result::ok).count(), 1);
        let _ = std::fs::remove_dir_all(&directory);
    }

    fn shipped_registry() -> Registry {
        let root = crate::paths::config_dir();
        let layout = config::load_layout(&root.join("scene-layout.toml")).unwrap();
        registry::load_and_validate(&root.join("qlc-functions.toml"), &layout).unwrap().0
    }

    #[test]
    fn the_shipped_watchdog_config_names_todays_three_zones_and_they_check_out() {
        let root = crate::paths::config_dir();
        let zones = parse_config(&std::fs::read_to_string(root.join("watchdog.toml")).unwrap()).unwrap();
        assert_eq!(zones, vec!["ram", "rog_eye", "strip"]);
        assert_eq!(check_zones(&zones, &shipped_registry()), Vec::<String>::new());
        assert_eq!(load_zones(&root), (zones, None));
    }

    #[test]
    fn an_unknown_or_repeated_zone_is_rejected() {
        let registry = shipped_registry();
        let zones = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        assert_eq!(check_zones(&zones(&["ram", "nowhere"]), &registry), vec!["zones: unknown zone nowhere".to_owned()]);
        assert!(check_zones(&zones(&["ram", "ram"]), &registry).iter().any(|problem| problem.contains("listed twice")));
        assert!(!check_zones(&[], &registry).is_empty(), "an empty list is a problem");
    }

    #[test]
    fn malformed_watchdog_configs_are_rejected() {
        assert!(parse_config("version = 1\nzones = [\"ram\"]\n").is_ok());
        assert!(parse_config("version = 2\nzones = [\"ram\"]\n").is_err());
        assert!(parse_config("version = 1\n").is_err(), "zones is required");
        assert!(parse_config("version = 1\nzones = [\"ram\"]\ntypo = 1\n").is_err());
    }

    #[test]
    fn a_broken_watchdog_config_falls_back_to_every_available_zone() {
        let zones = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        let available = || Ok(zones(&["ram", "rog_eye", "strip"]));
        let (chosen, problem) = choose_zones(Ok(zones(&["ram", "nowhere"])), Ok(shipped_registry()), available());
        assert_eq!(chosen, zones(&["ram", "rog_eye", "strip"]));
        assert!(problem.unwrap().contains("unknown zone nowhere; using every available zone instead"));
        let (chosen, problem) = choose_zones(Err("watchdog.toml: missing".to_owned()), Ok(shipped_registry()), available());
        assert_eq!((chosen.len(), problem.unwrap().starts_with("watchdog.toml: missing")), (3, true));
        let (chosen, problem) = choose_zones(Ok(zones(&["ram"])), Err("no registry".to_owned()), available());
        assert_eq!(chosen, zones(&["ram"]), "without a registry the file is trusted, and the problem reported");
        assert!(problem.unwrap().contains("not checked"));
        let (chosen, problem) = choose_zones(Err("bad".to_owned()), Err("no registry".to_owned()), Err("no layout".to_owned()));
        assert!(chosen.is_empty());
        assert!(problem.unwrap().contains("no zone can show a Controller Fault"));
    }

    #[test]
    fn the_available_zones_leave_out_the_unmapped_gpu_bracket() {
        let (_, problem) = load_zones(&crate::paths::config_dir());
        assert_eq!(problem, None);
        let layout = config::load_layout(&crate::paths::config_dir().join("scene-layout.toml")).unwrap();
        let available: Vec<&String> = layout.zones.iter().filter(|(_, zone)| zone.available).map(|(name, _)| name).collect();
        assert_eq!(available, ["ram", "rog_eye", "strip"]);
    }
}
