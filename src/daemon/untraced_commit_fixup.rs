//! Periodic driver of the untraced-commit fixup: every interval, stat the
//! `HEAD` reflog of each known worktree and schedule a fixup pass (see
//! `RefCursor::claim_untraced_commits`) for the ones that grew. Commits made
//! by clients that emit no trace2, from sandboxes, or while the daemon was
//! off are attributed within roughly one interval plus the minimum record
//! age. The first tick runs at startup so a restart catches up at once.
//!
//! The worker touches nothing on the trace ingestion path: it reads two
//! counters to stay out of the way while frames are still being ingested,
//! and every pass it schedules runs through the family sequencer like a
//! command.

use crate::daemon::ActorDaemonCoordinator;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::{MissedTickBehavior, interval};

/// Default time between scans. With the default 5 s minimum record age, an
/// untraced commit is attributed within about 15 s.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

pub fn spawn_untraced_fixup_worker(
    coordinator: Weak<ActorDaemonCoordinator>,
    period: Duration,
    shutdown_notify: Arc<Notify>,
) {
    tokio::spawn(async move {
        tracing::info!(
            interval_ms = period.as_millis() as u64,
            "untraced fixup worker started"
        );
        let mut ticker = interval(period);
        // After suspend/resume one scan covers everything; do not replay
        // every missed tick.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown_notify.notified() => break,
                _ = ticker.tick() => {
                    let Some(coordinator) = coordinator.upgrade() else {
                        break;
                    };
                    if let Err(error) = coordinator.run_untraced_fixup_tick().await {
                        tracing::warn!(%error, "untraced fixup tick failed");
                    }
                }
            }
        }
        tracing::info!("untraced fixup worker shutdown complete");
    });
}
