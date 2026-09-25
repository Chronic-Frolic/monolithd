//! Controller-side machinery shared by every reporter (extracted from the Steam reporter
//! 2026-09-25): announcing jobs, reporting progress, ending each job exactly once (retried
//! while the controller is unreachable), and raising and clearing warnings.
//!
//! A reporter decides *what* to say from its service's own API; this module only says it.
//! The rules it keeps for everyone: a refused announcement is retried later (most often the
//! same job is still in its completion hold); `unknown_job` on progress means the controller
//! restarted and the job must be announced again; an ending that cannot be delivered is kept
//! and retried, never dropped.

use crate::{controller, paths};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
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
        self.raise_as(id, "warning", reason).await;
    }

    /// Raise a Fault (all zones), for data-integrity threats only (owner decision 2026-09-25).
    pub async fn fault(&mut self, id: &str, reason: &str) {
        self.raise_as(id, "fault", reason).await;
        self.log(format!("FAULT {id}: {reason}"));
    }

    /// Raise with a given severity (`warning` or `fault`) without logging.
    pub async fn raise_as(&mut self, id: &str, severity: &str, reason: &str) {
        self.send(json!({ "op": "fault.raise", "id": id, "severity": severity, "reason": reason })).await;
    }

    pub async fn clear(&mut self, id: &str) {
        self.send(json!({ "op": "fault.clear", "id": id })).await;
        self.log(format!("cleared {id}"));
    }
}

// ---------------------------------------------------------------- config/reporters.toml

/// One aggregate bar per service, or one bar per item (owner decision 2026-09-25: per
/// service by default, switchable per service). Read at start; restart to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BarMode {
    PerService,
    PerItem,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bars {
    pub default: BarMode,
    #[serde(default)]
    pub services: BTreeMap<String, BarMode>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// btrfs mounts to watch; empty until filesystem day, because a missing mount is a Fault.
    pub mounts: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportersConfig {
    pub version: u32,
    pub bars: Bars,
    pub storage: StorageConfig,
}

pub fn parse_config(text: &str) -> Result<ReportersConfig, String> {
    let config: ReportersConfig = toml::from_str(text).map_err(|error| error.to_string())?;
    if config.version != 1 {
        return Err(format!("unsupported version {}", config.version));
    }
    Ok(config)
}

pub fn load_config() -> Result<ReportersConfig, String> {
    let path = paths::config_dir().join("reporters.toml");
    let text = std::fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    parse_config(&text).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_reporters_config_is_valid_and_watches_nothing_yet() {
        let config = load_config().unwrap();
        assert_eq!((config.bars.default, config.bars.services.len(), config.storage.mounts.len()), (BarMode::PerService, 0, 0));
    }

    #[test]
    fn per_service_overrides_parse_and_mistakes_are_rejected() {
        let config = parse_config("version = 1\n[bars]\ndefault = \"per_service\"\n[bars.services]\nqbittorrent = \"per_item\"\n[storage]\nmounts = [\"/storage/protected\"]\n").unwrap();
        assert_eq!(config.bars.services["qbittorrent"], BarMode::PerItem);
        assert!(parse_config("version = 1\n[bars]\ndefault = \"per_torrent\"\n[storage]\nmounts = []\n").is_err());
        assert!(parse_config("version = 1\n[bars]\ndefault = \"per_item\"\n[storage]\nmounts = []\ntypo = 1\n").is_err());
        assert!(parse_config("version = 2\n[bars]\ndefault = \"per_item\"\n[storage]\nmounts = []\n").is_err());
    }
}
