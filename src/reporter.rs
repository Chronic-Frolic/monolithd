//! Controller-side machinery shared by every reporter (extracted from the Steam reporter
//! 2026-09-25): announcing jobs, reporting progress, ending each job exactly once (retried
//! while the controller is unreachable), and raising and clearing warnings.
//!
//! A reporter decides *what* to say from its service's own API; this module only says it.
//! The rules it keeps for everyone: a refused announcement is retried later (most often the
//! same job is still in its completion hold); `unknown_job` on progress means the controller
//! restarted and the job must be announced again; an ending that cannot be delivered is kept
//! and retried, never dropped.

use crate::controller;
use serde_json::{json, Value};
use std::fmt::Display;

pub enum Sent {
    Ok,
    Refused(String),
    Unreachable,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ending {
    Complete,
    Fail(String),
}

pub struct Executor {
    /// Log prefix, for example `steam-reporter`.
    name: &'static str,
    dry_run: bool,
    /// Job endings the controller has not acknowledged yet.
    pending: Vec<(String, Ending)>,
    last_error: Option<String>,
}

impl Executor {
    pub fn new(name: &'static str, dry_run: bool) -> Self {
        Self { name, dry_run, pending: Vec::new(), last_error: None }
    }

    pub fn log(&self, message: impl Display) {
        eprintln!("monolithd {}: {message}", self.name);
    }

    pub async fn send(&mut self, request: Value) -> Sent {
        if self.dry_run {
            println!("monolithd {} (dry run): {request}", self.name);
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
                    self.log(&error);
                    self.last_error = Some(error);
                }
                Sent::Unreachable
            }
        }
    }

    /// The controller's status, for recovering after a restart; `None` in a dry run.
    pub async fn status(&self) -> Option<Value> {
        if self.dry_run {
            return None;
        }
        controller::call(&json!({ "op": "status" })).await.ok()
    }

    /// Announce a job. `false` means it was not announced; try again later.
    pub async fn start(&mut self, id: &str, label: &str, total: u32) -> bool {
        match self.send(json!({ "op": "job.start", "id": id, "label": label, "total": total })).await {
            Sent::Ok => {
                self.log(format!("announced {id} ({label}, {total})"));
                true
            }
            Sent::Refused(_) | Sent::Unreachable => false,
        }
    }

    /// Report progress. `false` means the controller no longer knows the job (it restarted),
    /// so the caller must announce it again.
    pub async fn progress(&mut self, id: &str, completed: u32, total: u32) -> bool {
        if let Sent::Refused(code) = self.send(json!({ "op": "job.progress", "id": id, "completed": completed, "total": total })).await {
            if code == "unknown_job" {
                self.log(format!("controller forgot {id}; announcing it again"));
                return false;
            }
        }
        true
    }

    /// End a job exactly once; kept and retried while the controller cannot be reached.
    pub async fn end(&mut self, id: String, ending: Ending) {
        let request = match &ending {
            Ending::Complete => json!({ "op": "job.complete", "id": id }),
            Ending::Fail(reason) => json!({ "op": "job.fail", "id": id, "reason": reason }),
        };
        match self.send(request).await {
            Sent::Unreachable => self.pending.push((id, ending)),
            Sent::Ok | Sent::Refused(_) => match ending {
                Ending::Complete => self.log(format!("{id} completed")),
                Ending::Fail(reason) => self.log(format!("{id} ended: {reason}")),
            },
        }
    }

    /// Deliver endings that could not be delivered before.
    pub async fn retry(&mut self) {
        for (id, ending) in std::mem::take(&mut self.pending) {
            self.end(id, ending).await;
        }
    }

    pub async fn warn(&mut self, id: &str, reason: &str) {
        self.raise(id, reason).await;
        self.log(format!("warning {id}: {reason}"));
    }

    /// Raise a warning again without logging; the controller forgets faults on restart.
    pub async fn raise(&mut self, id: &str, reason: &str) {
        self.send(json!({ "op": "fault.raise", "id": id, "severity": "warning", "reason": reason })).await;
    }

    pub async fn clear(&mut self, id: &str) {
        self.send(json!({ "op": "fault.clear", "id": id })).await;
        self.log(format!("cleared {id}"));
    }
}
