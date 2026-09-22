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
use crate::{controller, gateway};
use serde_json::{json, Value};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
const FAULT_ZONES: [&str; 3] = ["ram", "rog_eye", "strip"];

fn runtime_directory() -> Result<PathBuf, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(runtime).join("monolith-events"))
}

fn heartbeat_path() -> Result<PathBuf, String> {
    Ok(runtime_directory()?.join("watchdog-status.json"))
}

/// Holds the sleep delay inhibitor as a child process, exactly as the retired
/// Python watchdog did: `systemd-inhibit --mode=delay ... sleep infinity`, released
/// by terminating the child.
struct DelayInhibitor {
    child: Option<Child>,
}

impl DelayInhibitor {
    fn new() -> Self {
        Self { child: None }
    }

    async fn acquire(&mut self) -> Result<(), String> {
        if let Some(child) = &mut self.child {
            if child.try_wait().ok().flatten().is_none() {
                return Ok(()); // already held
            }
        }
        let mut child = Command::new("systemd-inhibit")
            .args(["--what=sleep", "--mode=delay", "--who=Monolith-Event-Watchdog", "--why=RGB suspend handoff", "/usr/bin/sleep", "infinity"])
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
}

impl Default for SharedState {
    fn default() -> Self {
        Self { suspended: false, fault_active: false, last_error: None }
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
    let body = json!({ "updated_at": updated_at, "suspended": state.suspended, "controller_fault_active": state.fault_active, "last_error": state.last_error });
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
    let functions: Vec<String> = FAULT_ZONES.iter().map(|zone| format!("{prefix}_{}", zone_suffix(zone))).collect();
    let request = json!({ "op": "preempt_set", "functions": functions });
    match gateway::call(&request).await {
        Ok(reply) if reply["ok"] == Value::Bool(true) => Ok(()),
        Ok(reply) => Err(reply["error"].as_str().unwrap_or("refused").to_owned()),
        Err(error) => Err(error),
    }
}

/// Bounded ping of the controller: reachable and answering `status` counts as healthy.
async fn controller_healthy() -> Result<(), String> {
    let reply = tokio::time::timeout(HEALTH_CHECK_TIMEOUT, controller::call(&json!({ "op": "status" })))
        .await
        .map_err(|_| "no answer within 3 s".to_owned())??;
    if reply["ok"] == Value::Bool(true) {
        Ok(())
    } else {
        Err(reply["error"].as_str().unwrap_or("controller reported an error").to_owned())
    }
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

async fn health_loop(state: Arc<Mutex<SharedState>>) {
    let mut ticks = interval(HEALTH_INTERVAL);
    loop {
        ticks.tick().await;
        if state.lock().await.suspended {
            continue;
        }
        match controller_healthy().await {
            Ok(()) => clear_controller_fault(&state).await,
            Err(error) => apply_controller_fault(&state, &error).await,
        }
    }
}

async fn prepare_sleep(state: &Mutex<SharedState>, delay: &Mutex<DelayInhibitor>) {
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
async fn resume(state: &Mutex<SharedState>, delay: &Mutex<DelayInhibitor>) {
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

async fn sleep_loop(state: Arc<Mutex<SharedState>>, delay: Arc<Mutex<DelayInhibitor>>) {
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
    let state = Arc::new(Mutex::new(SharedState::default()));
    write_heartbeat(&*state.lock().await);
    eprintln!("monolithd watchdog: watching controller health (every {} s) and sleep/wake", HEALTH_INTERVAL.as_secs());
    let delay = Arc::new(Mutex::new(DelayInhibitor::new()));
    tokio::join!(health_loop(state.clone()), sleep_loop(state, delay));
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
        let state = SharedState { suspended: true, fault_active: true, last_error: Some("boom".to_owned()) };
        write_heartbeat_to(&path, &state);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((body["suspended"].clone(), body["controller_fault_active"].clone(), body["last_error"].clone()), (json!(true), json!(true), json!("boom")));
        assert!(body["updated_at"].as_u64().unwrap() > 0);
        assert!(std::fs::read_dir(&directory).unwrap().filter_map(Result::ok).all(|entry| !entry.file_name().to_string_lossy().contains(".tmp")), "no leftover temp file");

        // Writing again must not leave the previous temp file behind either.
        write_heartbeat_to(&path, &state);
        assert_eq!(std::fs::read_dir(&directory).unwrap().filter_map(Result::ok).count(), 1);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
