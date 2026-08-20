//! Daemon-side checkpoint-outbox replay worker (continuity design, phase P2).
//!
//! Clients durably publish checkpoint deliveries to the outbox when live
//! delivery fails. This worker closes the loop: it scans the same candidate
//! roots the publisher writes to, re-validates each ready record, and
//! re-ingests it through the normal checkpoint path — the trace-ingest fence,
//! family sequencing, and stream authorization all apply exactly as for a
//! live delivery. Application is at-least-once: the persisted `delivery_id`
//! deduplicates replays of an already-applied delivery, so a record is
//! removed only after `ingest_checkpoint_delivery` returns success.
//!
//! Everything here runs on its own tokio task, entirely off the trace2
//! ingestion path. Records that cannot be decoded, whose repository no longer
//! exists or is no longer allowed for collection, or that keep failing are
//! quarantined (bounded, pruned after a retention window) instead of
//! retrying forever.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::model::repository::checkpoint_outbox::{
    self, CheckpointOutboxError, DEFAULT_MAX_ENCODED_RECORD_BYTES,
};
use crate::operations::git::repository::discover_repository_in_path_no_git_exec;

use super::ActorDaemonCoordinator;
use super::daemon_config::DaemonConfig;

const OUTBOX_POLL_INTERVAL: Duration = Duration::from_secs(30);
const OUTBOX_IMPORT_BATCH: usize = 128;
const OUTBOX_QUARANTINE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const OUTBOX_MAX_APPLY_ATTEMPTS: u32 = 5;
#[cfg(feature = "test-support")]
pub(super) const TEST_POLL_INTERVAL_ENV: &str = "GIT_AI_TEST_OUTBOX_POLL_MS";

pub(crate) fn start(coordinator: Arc<ActorDaemonCoordinator>, config: &DaemonConfig) {
    let roots =
        crate::operations::commands::checkpoint_agent::delivery_runtime::checkpoint_outbox_root_selection(
            config,
        )
        .roots;
    tokio::spawn(async move {
        run_worker(coordinator, roots).await;
    });
}

async fn run_worker(coordinator: Arc<ActorDaemonCoordinator>, roots: Vec<PathBuf>) {
    let poll_interval = poll_interval();
    let mut retry = RetryState::default();

    // Bounded initial scan first so records published while no daemon was
    // running replay promptly, then steady-state polling.
    loop {
        for root in &roots {
            if coordinator.is_shutting_down() {
                return;
            }
            consume_root(&coordinator, root, &mut retry).await;
        }

        tokio::select! {
            _ = tokio::time::sleep(poll_interval) => {}
            _ = coordinator.wait_for_shutdown() => return,
        }
    }
}

#[derive(Default)]
struct RetryState {
    attempts_by_name: HashMap<String, u32>,
    skip_rounds_by_name: HashMap<String, u32>,
}

async fn consume_root(
    coordinator: &Arc<ActorDaemonCoordinator>,
    root: &Path,
    retry: &mut RetryState,
) {
    let scan = match checkpoint_outbox::scan_ready_records(root, OUTBOX_IMPORT_BATCH) {
        Ok(scan) => scan,
        Err(CheckpointOutboxError::LockBusy) => return,
        Err(error) => {
            tracing::debug!(%error, root = %root.display(), "outbox scan failed");
            return;
        }
    };

    for name in &scan.invalid {
        quarantine(root, name, "unsafe ready-record name");
    }

    for name in &scan.ready {
        if coordinator.is_shutting_down() {
            return;
        }
        // Capped exponential backoff in poll ticks: 1, 2, 4, 8 rounds.
        if let Some(rounds) = retry.skip_rounds_by_name.get_mut(name)
            && *rounds > 0
        {
            *rounds -= 1;
            continue;
        }
        match replay_record(coordinator, root, name).await {
            ReplayOutcome::Applied => {
                retry.attempts_by_name.remove(name);
                retry.skip_rounds_by_name.remove(name);
                if let Err(error) = checkpoint_outbox::remove_ready_record(root, name) {
                    // The applied delivery is recorded in the working log, so a
                    // rescan of the surviving record deduplicates and retries
                    // this removal.
                    tracing::warn!(%error, name, "failed removing applied outbox record");
                }
            }
            ReplayOutcome::Discard(reason) => {
                retry.attempts_by_name.remove(name);
                retry.skip_rounds_by_name.remove(name);
                quarantine(root, name, reason);
            }
            ReplayOutcome::Retry(error) => {
                let attempts = retry.attempts_by_name.entry(name.clone()).or_insert(0);
                *attempts += 1;
                if *attempts >= OUTBOX_MAX_APPLY_ATTEMPTS {
                    tracing::warn!(
                        %error,
                        name,
                        attempts = *attempts,
                        "outbox record kept failing; quarantining"
                    );
                    retry.attempts_by_name.remove(name);
                    retry.skip_rounds_by_name.remove(name);
                    quarantine(root, name, "replay attempts exhausted");
                } else {
                    tracing::debug!(%error, name, attempts = *attempts, "outbox replay failed; will retry");
                    retry
                        .skip_rounds_by_name
                        .insert(name.clone(), 1u32 << (*attempts - 1));
                }
            }
        }
    }

    match checkpoint_outbox::prune_quarantined_records(root, OUTBOX_QUARANTINE_RETENTION) {
        Ok(0) | Err(CheckpointOutboxError::LockBusy) => {}
        Ok(pruned) => {
            tracing::debug!(pruned, root = %root.display(), "pruned quarantined outbox records")
        }
        Err(error) => tracing::debug!(%error, "outbox quarantine prune failed"),
    }
}

enum ReplayOutcome {
    Applied,
    Discard(&'static str),
    Retry(crate::error::GitAiError),
}

async fn replay_record(
    coordinator: &Arc<ActorDaemonCoordinator>,
    root: &Path,
    name: &str,
) -> ReplayOutcome {
    let bytes =
        match checkpoint_outbox::read_ready_record(root, name, DEFAULT_MAX_ENCODED_RECORD_BYTES) {
            Ok(bytes) => bytes,
            Err(CheckpointOutboxError::RecordTooLarge { .. })
            | Err(CheckpointOutboxError::UnsafeReadyRecord) => {
                return ReplayOutcome::Discard("ready record failed validation");
            }
            Err(error) => {
                return ReplayOutcome::Retry(crate::error::GitAiError::Generic(error.to_string()));
            }
        };
    let delivery = match checkpoint_outbox::decode_delivery(&bytes) {
        Ok(delivery) => delivery,
        Err(_) => return ReplayOutcome::Discard("ready record failed to decode"),
    };

    // Re-check repository eligibility at replay time: the allowlist may have
    // changed since capture, and a repository that no longer exists cannot
    // accept a checkpoint.
    let Some(first_file) = delivery.request.files.first() else {
        return ReplayOutcome::Discard("delivery contains no files");
    };
    let repo = match discover_repository_in_path_no_git_exec(&first_file.repo_work_dir) {
        Ok(repo) => repo,
        Err(_) => return ReplayOutcome::Discard("repository no longer exists"),
    };
    if !repo.is_collection_allowed(&crate::config::Config::fresh()) {
        return ReplayOutcome::Discard("repository is not allowed for collection");
    }

    match coordinator.ingest_checkpoint_delivery(delivery).await {
        Ok(response) if response.ok => ReplayOutcome::Applied,
        Ok(response) => ReplayOutcome::Retry(crate::error::GitAiError::Generic(
            response
                .error
                .unwrap_or_else(|| "checkpoint delivery rejected".to_string()),
        )),
        Err(error) => ReplayOutcome::Retry(error),
    }
}

fn quarantine(root: &Path, name: &str, reason: &'static str) {
    tracing::warn!(name, reason, root = %root.display(), "quarantining outbox record");
    if let Err(error) = checkpoint_outbox::quarantine_ready_record(root, name) {
        tracing::debug!(%error, name, "failed quarantining outbox record");
    }
}

fn poll_interval() -> Duration {
    #[cfg(feature = "test-support")]
    if let Ok(raw) = std::env::var(TEST_POLL_INTERVAL_ENV)
        && let Ok(milliseconds) = raw.parse::<u64>()
        && milliseconds > 0
    {
        return Duration::from_millis(milliseconds);
    }

    OUTBOX_POLL_INTERVAL
}
