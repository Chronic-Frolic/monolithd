//! Monolith Remote: the narrow, authenticated API for the machine.
//!
//! This is the recovery path: it wakes nothing but it can suspend, reboot, power off,
//! switch between Gaming and Desktop Mode, and hold or release the manual suspend block
//! when everything else is broken. It therefore runs as its own process and unit, apart
//! from the lighting stack and the controller, and accepts only a fixed allow-list of
//! actions behind a bearer token. It listens on loopback only; Tailscale Serve exposes it.
//!
//! It is a drop-in replacement for the Python server it folded in: the same routes, JSON,
//! and status codes, so the Android shortcuts and the `monolith-remote` script (renamed 2026-09-21 from `monolithctl`, kept as a symlink alias) are unchanged.
//! `/status` keeps its original keys and adds a `lighting` summary.

use crate::{controller, gateway};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::timeout;

const DEFAULT_PORT: u16 = 8787;
const MANUAL_BLOCK_UNIT: &str = "monolith-suspend-block.service";
const MANUAL_BLOCK_RUNTIME_SECONDS: f64 = 12.0 * 60.0 * 60.0;
/// An accepted action runs this long after the reply, so the caller receives its 202 first.
const ACTION_DELAY: Duration = Duration::from_millis(500);
/// `/status` must never hang on the lighting stack, which is exactly what may be broken.
const LIGHTING_TIMEOUT: Duration = Duration::from_secs(2);

/// The allow-list, in the order `/status` reports it.
const ACTIONS: [(&str, &[&str]); 7] = [
    ("/suspend", &["systemctl", "suspend"]),
    ("/reboot", &["systemctl", "reboot"]),
    ("/shutdown", &["systemctl", "poweroff"]),
    ("/desktop", &["steamosctl", "switch-to-desktop-mode"]),
    ("/gaming", &["steamosctl", "switch-to-game-mode"]),
    ("/suspend-block", &["systemctl", "--user", "start", MANUAL_BLOCK_UNIT]),
    ("/suspend-unblock", &["systemctl", "--user", "stop", MANUAL_BLOCK_UNIT]),
];

struct AppState {
    token: String,
    actions: Vec<(String, Vec<String>)>,
    action_delay: Duration,
}

type Shared = Arc<AppState>;

impl AppState {
    fn production(token: String) -> Self {
        let actions = ACTIONS.iter().map(|(path, argv)| ((*path).to_owned(), argv.iter().map(|word| (*word).to_owned()).collect())).collect();
        Self { token, actions, action_delay: ACTION_DELAY }
    }
}

fn reply(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

async fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, json!({ "error": "not found" }))
}

fn unauthorized() -> Response {
    reply(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }))
}

/// Compare two byte strings without stopping at the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = a.len() ^ b.len();
    for index in 0..a.len().max(b.len()) {
        difference |= usize::from(a.get(index).copied().unwrap_or(0) ^ b.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

fn authenticated(state: &AppState, headers: &HeaderMap) -> bool {
    let given = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()).unwrap_or("");
    constant_time_eq(given.as_bytes(), format!("Bearer {}", state.token).as_bytes())
}

async fn health() -> Response {
    reply(StatusCode::OK, json!({ "status": "ok" }))
}

async fn status(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if !authenticated(&state, &headers) {
        return unauthorized();
    }
    let (block, lighting) = tokio::join!(manual_block_status(), lighting_status());
    let actions: Vec<&str> = state.actions.iter().map(|(path, _)| path.as_str()).collect();
    reply(StatusCode::OK, json!({ "status": "ok", "actions": actions, "manual_suspend_block": block, "lighting": lighting }))
}

async fn action(State(state): State<Shared>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    let path = format!("/{name}");
    // An unknown path is a 404 before authentication is even considered.
    let Some((_, argv)) = state.actions.iter().find(|(known, _)| *known == path) else { return not_found().await };
    if !authenticated(&state, &headers) {
        return unauthorized();
    }
    let argv = argv.clone();
    let delay = state.action_delay;
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        execute(&path, &argv).await;
    });
    reply(StatusCode::ACCEPTED, json!({ "accepted": true, "action": name }))
}

async fn execute(action: &str, argv: &[String]) {
    eprintln!("Executing action: {action}");
    match Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).output().await {
        Ok(output) => {
            let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            eprintln!("{action}: exit={} output={}", output.status.code().map_or_else(|| "signal".to_owned(), |code| code.to_string()), text.trim());
        }
        Err(error) => eprintln!("{action}: could not run {}: {error}", argv[0]),
    }
}

fn property<'a>(output: &'a str, name: &str) -> &'a str {
    let prefix = format!("{name}=");
    output.lines().find_map(|line| line.strip_prefix(prefix.as_str())).unwrap_or("")
}

fn monotonic_seconds() -> f64 {
    let mut spec = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime only writes into the timespec we pass it.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut spec) };
    spec.tv_sec as f64 + spec.tv_nsec as f64 / 1e9
}

/// Whether the manual suspend block is held, and how long until it expires on its own.
async fn manual_block_status() -> Value {
    let output = Command::new("systemctl")
        .args(["--user", "show", MANUAL_BLOCK_UNIT, "-p", "ActiveState", "-p", "ActiveEnterTimestampMonotonic"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;
    let text = output.map(|output| String::from_utf8_lossy(&output.stdout).into_owned()).unwrap_or_default();
    let active = property(&text, "ActiveState") == "active";
    let started = property(&text, "ActiveEnterTimestampMonotonic").parse::<u64>().ok();
    let expires_in_seconds = match (active, started) {
        (true, Some(microseconds)) => Some((MANUAL_BLOCK_RUNTIME_SECONDS - (monotonic_seconds() - microseconds as f64 / 1e6)).max(0.0).round() as u64),
        _ => None,
    };
    json!({ "active": active, "reason": active.then_some("Manual remote suspend block"), "expires_in_seconds": expires_in_seconds })
}

// ---------------------------------------------------------------- the lighting summary

async fn ask<F: std::future::Future<Output = Result<Value, String>>>(request: F) -> Result<Value, String> {
    timeout(LIGHTING_TIMEOUT, request).await.unwrap_or_else(|_| Err("no answer in time".to_owned()))
}

/// A compact view of the lighting stack and the controller. Either may be down, and that
/// is exactly when this API is needed, so each is reported as unreachable, never as an error.
async fn lighting_status() -> Value {
    let request = json!({ "op": "status" });
    let (gateway, controller) = tokio::join!(ask(gateway::call(&request)), ask(controller::call(&request)));
    json!({ "gateway": summarize_gateway(gateway), "controller": summarize_controller(controller) })
}

fn unreachable(error: String) -> Value {
    json!({ "reachable": false, "error": error })
}

fn summarize_gateway(status: Result<Value, String>) -> Value {
    let status = match status {
        Ok(status) if status["ok"] == Value::Bool(true) => status,
        Ok(status) => return unreachable(status["error"].as_str().unwrap_or("status failed").to_owned()),
        Err(error) => return unreachable(error),
    };
    let zones: Map<String, Value> = status["zones"].as_object().map(|zones| zones.iter().map(|(zone, entry)| (zone.clone(), entry["owner"].clone())).collect()).unwrap_or_default();
    let sets: Map<String, Value> = status["ambient_sets"]
        .as_object()
        .map(|sets| sets.iter().map(|(name, entry)| (name.clone(), json!({ "members": entry["members"], "period_ms": entry["period_ms"] }))).collect())
        .unwrap_or_default();
    json!({
        "reachable": true,
        "mode": status["mode"],
        "qlc_reachable": status["qlc_reachable"],
        "output": { "state": status["output"]["state"], "reconnects": status["output"]["reconnects"], "failures_in_a_row": status["output"]["failures_in_a_row"], "last_error": status["output"]["last_error"] },
        "calibration": status["calibration"]["state"],
        "zones": zones,
        "ambient_sets": sets,
    })
}

fn summarize_controller(status: Result<Value, String>) -> Value {
    let status = match status {
        Ok(status) if status["ok"] == Value::Bool(true) => status,
        Ok(status) => return unreachable(status["error"].as_str().unwrap_or("status failed").to_owned()),
        Err(error) => return unreachable(error),
    };
    json!({
        "reachable": true,
        "ambient": status["ambient"],
        "paused": status["paused"],
        "waiting": status["waiting"],
        "zones": status["zones"],
        "jobs": status["jobs"],
        "failed_jobs": status["failures"].as_array().map_or(0, Vec::len),
    })
}

// ---------------------------------------------------------------- serving

fn router(state: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/{action}", post(action))
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .with_state(state)
}

fn token_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/monolith-remote/token"))
}

/// `monolithd remote [PORT]`: serve the API on loopback until stopped.
pub async fn run(port: Option<String>) -> Result<(), String> {
    let port = match port {
        None => DEFAULT_PORT,
        Some(text) => text.parse::<u16>().map_err(|_| format!("{text:?} is not a port number"))?,
    };
    let path = token_path()?;
    let token = std::fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?.trim().to_owned();
    if token.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(address).await.map_err(|error| format!("bind {address}: {error}"))?;
    eprintln!("Monolith Remote listening on {address}");
    axum::serve(listener, router(Arc::new(AppState::production(token)))).await.map_err(|error| format!("serve: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    const TOKEN: &str = "test-token-value";

    /// A server whose actions only append their name to a file, so nothing real is ever suspended.
    async fn serve(directory: &std::path::Path, delay: Duration) -> SocketAddr {
        let actions = ACTIONS
            .iter()
            .map(|(path, _)| ((*path).to_owned(), vec!["sh".to_owned(), "-c".to_owned(), format!("echo {path} >> {}", directory.join("ran").display())]))
            .collect();
        let state = Arc::new(AppState { token: TOKEN.to_owned(), actions, action_delay: delay });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
        address
    }

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("monolithd-remote-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    async fn http(address: SocketAddr, method: &str, path: &str, authorization: Option<&str>) -> (u16, Value) {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let header = authorization.map(|value| format!("Authorization: {value}\r\n")).unwrap_or_default();
        stream.write_all(format!("{method} {path} HTTP/1.1\r\nHost: test\r\n{header}Connection: close\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        let status = raw.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(raw.to_ascii_lowercase().contains("content-type: application/json"), "{raw}");
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    fn bearer() -> String {
        format!("Bearer {TOKEN}")
    }

    fn ran(directory: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(directory.join("ran")).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    #[tokio::test]
    async fn health_needs_no_token() {
        let address = serve(&scratch("health"), Duration::ZERO).await;
        assert_eq!(http(address, "GET", "/health", None).await, (200, json!({ "status": "ok" })));
    }

    #[tokio::test]
    async fn status_needs_the_right_token_and_keeps_its_original_keys() {
        let address = serve(&scratch("status"), Duration::ZERO).await;
        for authorization in [None, Some("Bearer wrong"), Some("Bearer"), Some(TOKEN), Some("bearer test-token-value")] {
            assert_eq!(http(address, "GET", "/status", authorization).await, (401, json!({ "error": "unauthorized" })), "{authorization:?}");
        }
        let (code, body) = http(address, "GET", "/status", Some(&bearer())).await;
        assert_eq!(code, 200);
        assert_eq!(body["status"], "ok");
        assert_eq!(
            body["actions"],
            json!(["/suspend", "/reboot", "/shutdown", "/desktop", "/gaming", "/suspend-block", "/suspend-unblock"]),
            "the allow-list, in its original order"
        );
        assert!(body["manual_suspend_block"]["active"].is_boolean());
        assert!(body["manual_suspend_block"].get("reason").is_some() && body["manual_suspend_block"].get("expires_in_seconds").is_some());
        assert!(body["lighting"]["gateway"]["reachable"].is_boolean() && body["lighting"]["controller"]["reachable"].is_boolean());
    }

    #[tokio::test]
    async fn unknown_paths_and_wrong_methods_are_404_before_authentication() {
        let address = serve(&scratch("notfound"), Duration::ZERO).await;
        for (method, path) in [("GET", "/"), ("GET", "/nope"), ("GET", "/suspend"), ("POST", "/nope"), ("POST", "/health"), ("POST", "/status"), ("POST", "/")] {
            assert_eq!(http(address, method, path, None).await, (404, json!({ "error": "not found" })), "{method} {path}");
        }
    }

    #[tokio::test]
    async fn an_unauthenticated_action_is_refused_and_never_runs() {
        let directory = scratch("refused");
        let address = serve(&directory, Duration::ZERO).await;
        for authorization in [None, Some("Bearer wrong")] {
            assert_eq!(http(address, "POST", "/suspend", authorization).await, (401, json!({ "error": "unauthorized" })));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(ran(&directory).is_empty());
    }

    #[tokio::test]
    async fn an_accepted_action_replies_first_and_runs_afterwards() {
        let directory = scratch("accepted");
        let address = serve(&directory, Duration::from_millis(300)).await;
        assert_eq!(http(address, "POST", "/reboot", Some(&bearer())).await, (202, json!({ "accepted": true, "action": "reboot" })));
        assert!(ran(&directory).is_empty(), "the action must not have run when the reply arrives");
        for _ in 0..40 {
            if !ran(&directory).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(ran(&directory), vec!["/reboot"]);
    }

    #[tokio::test]
    async fn every_allow_listed_action_is_accepted_and_only_those() {
        let directory = scratch("all");
        let address = serve(&directory, Duration::ZERO).await;
        for (path, _) in ACTIONS {
            let (code, body) = http(address, "POST", path, Some(&bearer())).await;
            assert_eq!((code, body["action"].clone()), (202, json!(path.trim_start_matches('/'))), "{path}");
        }
        assert_eq!(http(address, "POST", "/poweroff", Some(&bearer())).await.0, 404, "anything else is refused");
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut done = ran(&directory);
        done.sort();
        let mut expected: Vec<String> = ACTIONS.iter().map(|(path, _)| (*path).to_owned()).collect();
        expected.sort();
        assert_eq!(done, expected);
    }

    #[test]
    fn token_comparison_is_exact() {
        assert!(constant_time_eq(b"Bearer abc", b"Bearer abc"));
        assert!(!constant_time_eq(b"Bearer abc", b"Bearer abd"));
        assert!(!constant_time_eq(b"Bearer abc", b"Bearer abcd"));
        assert!(!constant_time_eq(b"Bearer abcd", b"Bearer abc"));
        assert!(!constant_time_eq(b"", b"Bearer abc"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn the_production_allow_list_is_exactly_the_seven_original_actions() {
        let state = AppState::production("t".to_owned());
        let commands: Vec<String> = state.actions.iter().map(|(path, argv)| format!("{path}: {}", argv.join(" "))).collect();
        assert_eq!(
            commands,
            vec![
                "/suspend: systemctl suspend",
                "/reboot: systemctl reboot",
                "/shutdown: systemctl poweroff",
                "/desktop: steamosctl switch-to-desktop-mode",
                "/gaming: steamosctl switch-to-game-mode",
                "/suspend-block: systemctl --user start monolith-suspend-block.service",
                "/suspend-unblock: systemctl --user stop monolith-suspend-block.service",
            ]
        );
        assert_eq!(state.action_delay, Duration::from_millis(500));
    }

    #[test]
    fn systemd_properties_are_parsed() {
        let output = "ActiveState=active\nActiveEnterTimestampMonotonic=123456\n";
        assert_eq!(property(output, "ActiveState"), "active");
        assert_eq!(property(output, "ActiveEnterTimestampMonotonic"), "123456");
        assert_eq!(property(output, "Missing"), "");
        assert_eq!(property("ActiveState=inactive\n", "ActiveState"), "inactive");
    }

    #[test]
    fn the_lighting_summary_is_compact_and_reports_a_down_component_as_unreachable() {
        let gateway = json!({
            "ok": true, "mode": "control", "qlc_reachable": true,
            "output": { "state": "ok", "reconnects": 2, "failures_in_a_row": 0, "last_error": "Operation has timed out" },
            "calibration": { "state": "loaded", "zones": { "strip": { "gain": [0.18, 0.18, 1.0] } } },
            "zones": { "ram": { "owner": "ambient_ram", "qlc": "Running" }, "strip": { "owner": null } },
            "ambient_sets": { "deep_violet": { "members": ["ambient_ram"], "period_ms": 3600, "phase_ms": 5 } },
        });
        let summary = summarize_gateway(Ok(gateway));
        assert_eq!(summary["reachable"], true);
        assert_eq!(summary["calibration"], "loaded", "only the state, not the gains");
        assert_eq!(summary["zones"], json!({ "ram": "ambient_ram", "strip": null }));
        assert_eq!(summary["output"]["reconnects"], 2);
        assert_eq!(summary["ambient_sets"]["deep_violet"], json!({ "members": ["ambient_ram"], "period_ms": 3600 }));
        assert_eq!(summarize_gateway(Err("connect: refused".to_owned())), json!({ "reachable": false, "error": "connect: refused" }));
        assert_eq!(summarize_gateway(Ok(json!({ "ok": false, "error": "read only" }))), json!({ "reachable": false, "error": "read only" }));

        let controller = json!({ "ok": true, "ambient": "deep_violet", "paused": false, "waiting": null, "zones": {}, "jobs": [{ "id": "a" }], "failures": [{ "id": "x" }, { "id": "y" }], "recent_actions": ["long"] });
        let summary = summarize_controller(Ok(controller));
        assert_eq!((summary["reachable"].clone(), summary["failed_jobs"].clone(), summary["jobs"].clone()), (json!(true), json!(2), json!([{ "id": "a" }])));
        assert!(summary.get("recent_actions").is_none());
        assert_eq!(summarize_controller(Err("down".to_owned()))["reachable"], false);
    }
}
