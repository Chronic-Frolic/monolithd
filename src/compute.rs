//! The Monolith Compute reporter (owner decision 2026-10-23): every `monolith-compute` job
//! that is actually running becomes a controller job `compute:<jobid>` with its own bar.
//!
//! The runner (`monolith-compute-run` in server-config) keeps `state.json` current in each job
//! folder, and a job may write `mc/progress` (`/work/mc/progress` inside the container) as
//! `COMPLETED TOTAL` or `COMPLETED/TOTAL`, in real units (frames, plate steps, cases). This
//! follower only reads those files and says what they say:
//!
//! - `running`, `paused-game` and `requeued-game` are announced (a paused or requeued job still
//!   owns its work and its sleep block, so its bar stays); `queued` and `waiting-*` are not.
//! - A job that writes no progress is announced as 0 of 1: an empty bar, whose empty side is
//!   the working look, which is the truth (running, with no count to show).
//! - `done` completes it; `failed` and `cancelled` fail it; a job folder that vanished while
//!   announced fails it ("removed"). Controller jobs `compute:*` that nothing on disk is still
//!   running (left over from before a restart) are ended the same way.
//!
//! Jobs from different agents run side by side, each taking a free progress zone; extra jobs
//! wait for one, as the protocol says.

use crate::reporter::{Ending, Executor};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::{interval, MissedTickBehavior};

pub const JOB_PREFIX: &str = "compute:";
const POLL: Duration = Duration::from_secs(3);
const REASSERT: Duration = Duration::from_secs(30);

/// One job folder as read from disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub id: String,
    pub name: String,
    pub state: String,
    pub detail: String,
    pub progress: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Start { id: String, label: String, total: u32 },
    Progress { id: String, completed: u32, total: u32 },
    End { id: String, ending: Ending },
}

/// What the controller has been told for one job; `sent` is `None` until progress is
/// (re)sent, which is how a reassert makes the next plan resend it.
#[derive(Debug, Clone, PartialEq)]
struct Announced {
    sent: Option<(u32, u32)>,
}

#[derive(Debug, Default)]
pub struct Core {
    announced: BTreeMap<String, Announced>,
}

fn is_active(state: &str) -> bool {
    matches!(state, "running" | "paused-game" | "requeued-game")
}

fn controller_id(job: &str) -> String {
    format!("{JOB_PREFIX}{job}")
}

/// `(completed, total)` for the bar: the job's own numbers, or 0 of 1 when it reports none.
fn bar(progress: Option<(u32, u32)>) -> (u32, u32) {
    match progress {
        Some((completed, total)) if total > 0 => (completed.min(total), total),
        _ => (0, 1),
    }
}

/// `42 240` or `42/240` (first line); anything else is no progress.
pub fn parse_progress(text: &str) -> Option<(u32, u32)> {
    let line = text.lines().next()?;
    let mut numbers = line.split(|c: char| c.is_whitespace() || c == '/').filter(|word| !word.is_empty());
    let completed = numbers.next()?.parse().ok()?;
    let total = numbers.next()?.parse().ok()?;
    if numbers.next().is_some() || total == 0 {
        return None;
    }
    Some((completed, total))
}

impl Core {
    /// Take over controller jobs left from before this process started, so ones that already
    /// ended get ended and live ones get their progress resent.
    pub fn recover(&mut self, status: &Value) {
        for id in status["jobs"].as_array().into_iter().flatten().filter_map(|job| job["id"].as_str()) {
            if let Some(job) = id.strip_prefix(JOB_PREFIX) {
                self.announced.insert(job.to_owned(), Announced { sent: None });
            }
        }
    }

    /// Resend every job's progress on the next plan (the controller forgets jobs on restart).
    pub fn reassert(&mut self) {
        for entry in self.announced.values_mut() {
            entry.sent = None;
        }
    }

    pub fn plan(&self, seen: &[Seen]) -> Vec<Action> {
        let mut actions = Vec::new();
        for job in seen {
            let id = controller_id(&job.id);
            let known = self.announced.get(&job.id);
            if is_active(&job.state) {
                let (completed, total) = bar(job.progress);
                match known {
                    None => {
                        actions.push(Action::Start { id: id.clone(), label: format!("Compute: {}", job.name), total });
                        if completed > 0 {
                            actions.push(Action::Progress { id, completed, total });
                        }
                    }
                    Some(entry) if entry.sent != Some((completed, total)) => actions.push(Action::Progress { id, completed, total }),
                    Some(_) => {}
                }
            } else if known.is_some() {
                let ending = match job.state.as_str() {
                    "done" => Some(Ending::Complete),
                    "failed" => Some(Ending::Fail(if job.detail.is_empty() { "the job failed".to_owned() } else { job.detail.clone() })),
                    "cancelled" => Some(Ending::Fail("the job was cancelled".to_owned())),
                    // queued or waiting again: keep the bar (should not happen after running)
                    _ => None,
                };
                if let Some(ending) = ending {
                    actions.push(Action::End { id, ending });
                }
            }
        }
        for job in self.announced.keys() {
            if !seen.iter().any(|seen| &seen.id == job) {
                actions.push(Action::End { id: controller_id(job), ending: Ending::Fail("the job folder was removed".to_owned()) });
            }
        }
        actions
    }

    pub fn started(&mut self, job: &str, sent: Option<(u32, u32)>) {
        self.announced.insert(job.to_owned(), Announced { sent });
    }

    pub fn progressed(&mut self, job: &str, sent: (u32, u32)) {
        if let Some(entry) = self.announced.get_mut(job) {
            entry.sent = Some(sent);
        }
    }

    pub fn ended(&mut self, job: &str) {
        self.announced.remove(job);
    }
}

fn read_job(directory: &Path) -> Option<Seen> {
    let state: Value = serde_json::from_str(&std::fs::read_to_string(directory.join("state.json")).ok()?).ok()?;
    let text = |key: &str| state[key].as_str().unwrap_or_default().to_owned();
    let progress = std::fs::read_to_string(directory.join("mc/progress")).ok().and_then(|text| parse_progress(&text));
    Some(Seen { id: text("id"), name: text("name"), state: text("state"), detail: text("detail"), progress })
}

/// Every job folder with a readable `state.json`. A folder still being copied in has none yet.
pub fn scan(jobs: &Path) -> Vec<Seen> {
    let Ok(entries) = std::fs::read_dir(jobs) else { return Vec::new() };
    let mut seen: Vec<Seen> = entries.filter_map(Result::ok).filter_map(|entry| read_job(&entry.path())).filter(|job| !job.id.is_empty()).collect();
    seen.sort_by(|a, b| a.id.cmp(&b.id));
    seen
}

pub fn jobs_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty()).ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share/monolith-compute/jobs"))
}

async fn apply(executor: &mut Executor, core: &mut Core, action: Action) {
    match action {
        Action::Start { id, label, total } => {
            if executor.start(&id, &label, total).await {
                core.started(&id[JOB_PREFIX.len()..], Some((0, total)));
            }
        }
        Action::Progress { id, completed, total } => {
            let job = id[JOB_PREFIX.len()..].to_owned();
            if executor.progress(&id, completed, total).await {
                core.progressed(&job, (completed, total));
            } else if executor.start(&id, &id, total).await {
                // the controller restarted and forgot it: announced again, progress follows next pass
                core.started(&job, Some((0, total)));
            }
        }
        Action::End { id, ending } => {
            executor.end(id.clone(), ending).await;
            core.ended(&id[JOB_PREFIX.len()..]);
        }
    }
}

pub async fn follow(dry_run: bool) -> Result<(), String> {
    let jobs = jobs_dir()?;
    let mut executor = Executor::new("compute-reporter", dry_run);
    let mut core = Core::default();
    if let Some(status) = executor.status().await {
        core.recover(&status);
    }
    executor.log(format!("following {}", jobs.display()));
    let mut tick = interval(POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut reassert = interval(REASSERT);
    reassert.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = reassert.tick() => core.reassert(),
        }
        executor.retry().await;
        for action in core.plan(&scan(&jobs)) {
            apply(&mut executor, &mut core, action).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(id: &str, state: &str, progress: Option<(u32, u32)>) -> Seen {
        Seen { id: id.into(), name: format!("name-{id}"), state: state.into(), detail: String::new(), progress }
    }

    #[test]
    fn progress_files_parse_strictly() {
        assert_eq!(parse_progress("42 240\n"), Some((42, 240)));
        assert_eq!(parse_progress("42/240"), Some((42, 240)));
        assert_eq!(parse_progress("0 0"), None, "a zero total is no bar");
        assert_eq!(parse_progress("1 2 3"), None);
        assert_eq!(parse_progress("half way"), None);
        assert_eq!(parse_progress(""), None);
        assert_eq!(bar(Some((9, 4))), (4, 4), "completed is clamped");
        assert_eq!(bar(None), (0, 1));
    }

    #[test]
    fn only_a_running_job_is_announced_and_a_silent_one_is_an_empty_bar() {
        let core = Core::default();
        assert!(core.plan(&[seen("a", "queued", None), seen("b", "waiting-gpu", None), seen("c", "waiting-game", None), seen("d", "done", None)]).is_empty());
        assert_eq!(core.plan(&[seen("a", "running", None)]), vec![Action::Start { id: "compute:a".into(), label: "Compute: name-a".into(), total: 1 }]);
        assert_eq!(
            core.plan(&[seen("a", "running", Some((3, 10)))]),
            vec![Action::Start { id: "compute:a".into(), label: "Compute: name-a".into(), total: 10 }, Action::Progress { id: "compute:a".into(), completed: 3, total: 10 }]
        );
    }

    #[test]
    fn progress_is_sent_on_change_only_and_a_reassert_resends_it() {
        let mut core = Core::default();
        core.started("a", Some((0, 10)));
        assert!(core.plan(&[seen("a", "running", Some((0, 10)))]).is_empty());
        assert_eq!(core.plan(&[seen("a", "running", Some((4, 10)))]), vec![Action::Progress { id: "compute:a".into(), completed: 4, total: 10 }]);
        core.progressed("a", (4, 10));
        assert!(core.plan(&[seen("a", "paused-game", Some((4, 10)))]).is_empty(), "a paused job keeps its bar");
        assert!(core.plan(&[seen("a", "requeued-game", Some((4, 10)))]).is_empty(), "so does a requeued GPU job");
        core.reassert();
        assert_eq!(core.plan(&[seen("a", "running", Some((4, 10)))]).len(), 1);
    }

    #[test]
    fn endings_follow_the_job_state_once() {
        let mut core = Core::default();
        for job in ["a", "b", "c"] {
            core.started(job, Some((0, 1)));
        }
        let mut failed = seen("b", "failed", None);
        failed.detail = "run.sh exited with 2".into();
        let plan = core.plan(&[seen("a", "done", None), failed, seen("c", "cancelled", None)]);
        assert_eq!(
            plan,
            vec![
                Action::End { id: "compute:a".into(), ending: Ending::Complete },
                Action::End { id: "compute:b".into(), ending: Ending::Fail("run.sh exited with 2".into()) },
                Action::End { id: "compute:c".into(), ending: Ending::Fail("the job was cancelled".into()) },
            ]
        );
        core.ended("a");
        let later = core.plan(&[seen("a", "done", None), seen("b", "failed", None), seen("c", "cancelled", None)]);
        assert!(later.iter().all(|action| !matches!(action, Action::End { id, .. } if id == "compute:a")), "an ended job is never ended twice");
    }

    #[test]
    fn a_removed_folder_and_leftovers_after_a_restart_are_ended() {
        let mut core = Core::default();
        core.recover(&serde_json::json!({ "jobs": [{ "id": "compute:gone" }, { "id": "compute:live" }, { "id": "steam:1" }, { "id": "rsync:5" }] }));
        let plan = core.plan(&[seen("live", "running", Some((1, 2)))]);
        assert_eq!(
            plan,
            vec![
                Action::Progress { id: "compute:live".into(), completed: 1, total: 2 },
                Action::End { id: "compute:gone".into(), ending: Ending::Fail("the job folder was removed".into()) },
            ],
            "other services' jobs are left alone"
        );
    }

    #[test]
    fn reads_the_runners_state_file_and_the_jobs_progress_file() {
        let root = std::env::temp_dir().join(format!("monolithd-compute-test-{}", std::process::id()));
        let job = root.join("20261023-153553-x-b27f");
        std::fs::create_dir_all(job.join("mc")).unwrap();
        std::fs::write(root.join("20261023-000000-copying-0000"), "not a folder").unwrap();
        std::fs::create_dir_all(root.join("20261023-000001-early-0001")).unwrap();
        std::fs::write(job.join("state.json"), r#"{"id":"20261023-153553-x-b27f","name":"x","state":"running","detail":""}"#).unwrap();
        std::fs::write(job.join("mc/progress"), "7 24\n").unwrap();
        let jobs = scan(&root);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(jobs, vec![Seen { id: "20261023-153553-x-b27f".into(), name: "x".into(), state: "running".into(), detail: String::new(), progress: Some((7, 24)) }]);
    }
}
