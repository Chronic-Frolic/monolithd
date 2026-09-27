//! `monolithd reporters`: one process for the API-based reporters (owner decision 2026-09-25).
//!
//! It runs the Steam download reporter, the storage observer (config/reporters.toml
//! `[storage] mounts`, empty until filesystem day), the hardware-reset watch (a Fault when the
//! machine reset itself after a hardware error) and the sweep that ends `rsync:` jobs whose
//! wrapper was killed. Each keeps its own job and fault IDs; if any of them stops, the process
//! exits and systemd restarts it, and five failures in five minutes raise fault
//! `reporters:down` through the unit's OnFailure.

use crate::reporter::{self, Executor};
use crate::{hardware, job, paths, steam, storage};

const DOWN_ID: &str = "reporters:down";
const USAGE: &str = "usage: monolithd reporters [--dry-run]";

pub async fn run(arguments: Vec<String>) -> Result<(), String> {
    let dry_run = match arguments.as_slice() {
        [] => false,
        [flag] if flag == "--dry-run" => true,
        _ => return Err(USAGE.to_owned()),
    };
    let config = reporter::load_config()?;
    let mut executor = Executor::new("reporters", dry_run);
    let overrides: Vec<String> = config.bars.services.iter().map(|(service, mode)| format!("{service} {mode:?}")).collect();
    executor.log(format!("{}; bars {:?} by default{}", paths::describe(), config.bars.default, if overrides.is_empty() { String::new() } else { format!(" ({})", overrides.join(", ")) }));
    if let Some(status) = executor.status().await {
        if status["active_faults"].as_array().into_iter().flatten().any(|fault| fault["id"] == DOWN_ID) {
            executor.clear(DOWN_ID).await;
        }
    }
    tokio::select! {
        result = steam::follow_downloads(dry_run) => result.map_err(|error| format!("steam reporter stopped: {error}")),
        result = storage::follow(config.storage.mounts, dry_run) => result.map_err(|error| format!("storage reporter stopped: {error}")),
        result = job::sweep(dry_run) => result.map_err(|error| format!("copy-job sweep stopped: {error}")),
        result = hardware::follow(dry_run) => result.map_err(|error| format!("hardware-reset watch stopped: {error}")),
    }
}
