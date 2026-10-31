//! Hourly housekeeping: prunes state that would otherwise grow forever,
//! logs table sizes and makes the daily backup of the state database.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use crate::{AppState, store::Retention};

const INTERVAL: Duration = Duration::from_secs(3600);
const BACKUP_EVERY: Duration = Duration::from_secs(24 * 3600);

/// Where daily backups go and how many are kept (0: none).
#[derive(Clone)]
pub struct Backups {
    pub dir: PathBuf,
    pub keep: usize,
}

pub fn spawn(state: Arc<AppState>, backups: Backups) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(INTERVAL);
        loop {
            tick.tick().await;
            // SQLite work that reads every snapshot: off the async workers,
            // so requests that do not need the store keep running.
            let (state, backups) = (state.clone(), backups.clone());
            let done = tokio::task::spawn_blocking(move || {
                run(&state);
                if backups.keep > 0
                    && let Err(e) = backup_if_due(&state, &backups)
                {
                    tracing::warn!(error = format!("{e:#}"), "backup failed");
                }
            })
            .await;
            if let Err(e) = done {
                tracing::error!(error = %e, "maintenance panicked");
            }
        }
    });
}

/// Backups in `dir`, newest first.
fn existing(dir: &Path) -> Vec<(SystemTime, PathBuf)> {
    let mut found: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("state-"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    found.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    found
}

/// Makes a backup when the newest is a day old, then keeps the newest
/// `keep` of them.
pub fn backup_if_due(state: &AppState, backups: &Backups) -> anyhow::Result<()> {
    std::fs::create_dir_all(&backups.dir)?;
    let due = existing(&backups.dir)
        .first()
        .is_none_or(|(at, _)| at.elapsed().unwrap_or(BACKUP_EVERY) >= BACKUP_EVERY);
    if due {
        let name = format!(
            "state-{}.sqlite",
            chrono::Utc::now().format("%Y%m%d-%H%M%S")
        );
        let path = backups.dir.join(name);
        state.store.backup(&path)?;
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        tracing::info!(path = %path.display(), "backed up the state database");
    }
    for (_, old) in existing(&backups.dir).into_iter().skip(backups.keep) {
        std::fs::remove_file(&old)?;
    }
    Ok(())
}

fn run(state: &AppState) {
    match state.store.prune(&Retention::default()) {
        Ok(pruned) => tracing::info!(
            devices = pruned.devices,
            client_ids = pruned.client_ids,
            item_ids = pruned.item_ids,
            "pruned state"
        ),
        Err(e) => tracing::warn!(error = format!("{e:#}"), "pruning state failed"),
    }
    state.jmap.prune();
    match state.store.row_counts() {
        Ok(counts) => {
            let counts: Vec<String> = counts.iter().map(|(t, n)| format!("{t}={n}")).collect();
            tracing::info!(rows = counts.join(" "), "state database");
        }
        Err(e) => tracing::warn!(error = format!("{e:#}"), "counting rows failed"),
    }
}
