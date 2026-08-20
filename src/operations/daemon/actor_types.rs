use crate::error::GitAiError;
use crate::model::checkpoint_request::CheckpointRequest;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, oneshot};
use tokio::time::Duration;

#[doc(hidden)]
pub enum FamilySequencerEntry {
    PendingRoot,
    ReadyCommand(Box<crate::model::domain::NormalizedCommand>),
    Checkpoint {
        request: Box<CheckpointRequest>,
        respond_to: Option<oneshot::Sender<Result<u64, GitAiError>>>,
    },
    Canceled,
}

impl std::fmt::Debug for FamilySequencerEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PendingRoot => write!(f, "PendingRoot"),
            Self::ReadyCommand(_) => write!(f, "ReadyCommand(..)"),
            Self::Checkpoint { .. } => write!(f, "Checkpoint {{ .. }}"),
            Self::Canceled => write!(f, "Canceled"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[doc(hidden)]
pub struct FamilySequencerOrder {
    pub(crate) started_at_ns: u128,
    pub(crate) ordinal: u64,
}

#[derive(Debug, Default)]
#[doc(hidden)]
pub struct FamilySequencerState {
    pub(crate) next_ordinal: u64,
    pub(crate) entries: BTreeMap<FamilySequencerOrder, FamilySequencerEntry>,
}

impl FamilySequencerState {
    pub(crate) fn new() -> Self {
        Self {
            next_ordinal: 1,
            ..Self::default()
        }
    }
    pub(crate) fn insert_entry(
        &mut self,
        started_at_ns: u128,
        entry: FamilySequencerEntry,
    ) -> FamilySequencerOrder {
        let order = FamilySequencerOrder {
            started_at_ns,
            ordinal: self.next_ordinal,
        };
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        self.entries.insert(order, entry);
        order
    }
}

#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct PendingRootSlot {
    pub(crate) family: String,
    pub(crate) order: FamilySequencerOrder,
}

#[doc(hidden)]
pub type CommitFileTimestampSnapshotHandle = tokio::task::JoinHandle<
    Option<crate::operations::authorship::attribution_recovery::FileTimestampsByPath>,
>;
#[doc(hidden)]
pub type CommitFileTimestampSnapshotHandles = HashMap<String, CommitFileTimestampSnapshotHandle>;

#[doc(hidden)]
pub const COMMIT_FILE_TIMESTAMP_SNAPSHOT_WAIT: Duration = Duration::from_millis(500);
#[doc(hidden)]
pub const SESSION_EVENT_RECOVERY_PREFLIGHT_WAIT: Duration = Duration::from_secs(2);
#[doc(hidden)]
pub const SESSION_EVENT_RECOVERY_PREFLIGHT_POLL: Duration = Duration::from_millis(100);

#[doc(hidden)]
pub fn run_blocking_side_effect<T>(operation: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct PendingSquashMerge {
    pub(crate) source_head: String,
    pub(crate) onto: String,
}

#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct PendingCherryPickNoCommit {
    pub(crate) source_commits: Vec<String>,
    pub(crate) head: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
#[doc(hidden)]
pub enum RecentReplayPrerequisite {
    CheckoutSwitchRename {
        target_head: String,
        old_head: String,
    },
    CheckoutSwitchMerge {
        target_head: String,
        old_head: String,
        final_state: HashMap<String, String>,
    },
}

#[derive(Debug, Default, Clone)]
#[doc(hidden)]
pub struct TraceIngressState {
    pub(crate) root_worktrees: HashMap<String, PathBuf>,
    pub(crate) root_families: HashMap<String, String>,
    pub(crate) root_argv: HashMap<String, Vec<String>>,
    pub(crate) root_started_at_ns: HashMap<String, u128>,
    pub(crate) root_reflog_start_offsets: HashMap<String, HashMap<String, u64>>,
    pub(crate) root_mutating: HashMap<String, bool>,
    pub(crate) root_target_repo_only: HashMap<String, bool>,
    pub(crate) root_last_activity_ns: HashMap<String, u64>,
    /// Roots whose start event was identified as definitely read-only. All
    /// subsequent events for these roots (including exit) take the fast path.
    pub(crate) root_definitely_read_only: HashSet<String>,
    pub(crate) root_open_connections: HashMap<String, usize>,
    pub(crate) unidentified_open_connections: usize,
    pub(crate) root_close_markers_enqueued: HashSet<String>,
}

#[doc(hidden)]
pub struct ActorDaemonCoordinator {
    pub(crate) backend: Arc<crate::operations::daemon::git_backend::SystemGitBackend>,
    pub(crate) coordinator: Arc<
        crate::operations::daemon::coordinator::Coordinator<
            crate::operations::daemon::git_backend::SystemGitBackend,
        >,
    >,
    pub(crate) normalizer: AsyncMutex<
        crate::operations::daemon::trace_normalizer::TraceNormalizer<
            crate::operations::daemon::git_backend::SystemGitBackend,
        >,
    >,
    pub(crate) pending_rebase_original_head_by_worktree:
        Mutex<HashMap<String, (String, Option<String>)>>,
    pub(crate) pending_cherry_pick_sources_by_worktree: Mutex<HashMap<String, Vec<String>>>,
    pub(crate) pending_cherry_pick_no_commit_by_worktree:
        Mutex<HashMap<String, PendingCherryPickNoCommit>>,
    pub(crate) pending_squash_merge_by_worktree: Mutex<HashMap<String, PendingSquashMerge>>,
    pub(crate) inflight_effects_by_family: Mutex<HashMap<String, usize>>,
    /// Files with an in-flight AI edit (PreFileEdit received, PostFileEdit not yet completed).
    /// Outer key: family. Inner key: absolute file path string. Value: registration timestamp (nanos).
    pub(crate) pending_ai_edits_by_family: Mutex<HashMap<String, HashMap<String, u128>>>,
    pub(crate) family_sequencers_by_family: Mutex<HashMap<String, FamilySequencerState>>,
    pub(crate) pending_root_slots_by_root: Mutex<HashMap<String, PendingRootSlot>>,
    pub(crate) commit_file_timestamp_snapshots_by_root:
        Mutex<HashMap<String, CommitFileTimestampSnapshotHandles>>,
    pub(crate) recent_replay_prerequisites_by_family:
        Mutex<HashMap<String, VecDeque<RecentReplayPrerequisite>>>,
    pub(crate) side_effect_errors_by_family: Mutex<HashMap<String, BTreeMap<u64, String>>>,
    pub(crate) side_effect_exec_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// Weak self-reference set by `register_self` after the coordinator is
    /// wrapped in its `Arc` (production daemons and full-daemon tests do
    /// this). Family drains are then scheduled on their own tasks so
    /// independent families progress concurrently; an unregistered
    /// coordinator (bare unit-test rigs) falls back to the historical inline
    /// drain.
    pub(crate) self_ref: std::sync::OnceLock<std::sync::Weak<ActorDaemonCoordinator>>,
    /// Coalescing state for scheduled family drains: present = a drain task
    /// is running for that family; `true` = re-drain requested while running.
    pub(crate) scheduled_family_drains: Mutex<HashMap<String, bool>>,
    pub(crate) bash_sessions: Mutex<crate::operations::daemon::bash_sessions::BashSessionState>,
    pub(crate) test_completion_log_dir: Option<PathBuf>,
    pub(crate) test_completion_log_lock: Mutex<()>,
    // OnceLock: set once at worker start, never cleared. The ingest worker
    // exits via the shutdown select! arm instead of relying on channel closure.
    pub(crate) trace_ingest_tx: std::sync::OnceLock<mpsc::Sender<Value>>,
    pub(crate) telemetry_worker:
        Option<crate::operations::daemon::telemetry_worker::DaemonTelemetryWorkerHandle>,
    pub(crate) stream_worker: Option<crate::operations::daemon::stream_worker::StreamWorkerHandle>,
    pub(crate) transcript_shutdown_notify: std::sync::OnceLock<Arc<tokio::sync::Notify>>,
    pub(crate) streams_db: Option<Arc<crate::model::repository::streams_db::StreamsDatabase>>,
    // Resolved once at daemon init; None in unit-test constructions (global() is the fallback).
    pub(crate) bash_history_db: Option<
        &'static std::sync::Mutex<crate::model::repository::bash_history_db::BashHistoryDatabase>,
    >,
    pub(crate) metrics_db:
        Option<&'static std::sync::Mutex<crate::model::repository::metrics_db::MetricsDatabase>>,
    pub(crate) next_trace_ingest_seq: AtomicUsize,
    pub(crate) queued_trace_payloads: AtomicUsize,
    pub(crate) queued_trace_payloads_by_root: Mutex<HashMap<String, usize>>,
    pub(crate) processed_trace_ingest_seq: AtomicUsize,
    pub(crate) trace_ingest_progress_notify: Notify,
    pub(crate) trace_ingress_state: Mutex<TraceIngressState>,
    pub(crate) shutting_down: AtomicBool,
    pub(crate) shutdown_action: AtomicU8,
    pub(crate) shutdown_notify: Notify,
    pub(crate) shutdown_condvar: std::sync::Condvar,
    pub(crate) shutdown_condvar_mutex: Mutex<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonExitAction {
    Stop,
    Restart,
    RestartAfterUpdate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonSelfUpdateOutcome {
    Installed,
    NoUpdate,
    Failed,
}

impl DaemonExitAction {
    pub(crate) fn as_u8(self) -> u8 {
        match self {
            Self::Stop => 0,
            Self::Restart => 1,
            Self::RestartAfterUpdate => 2,
        }
    }

    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Restart,
            2 => Self::RestartAfterUpdate,
            _ => Self::Stop,
        }
    }
}

#[doc(hidden)]
pub enum TracePayloadApplyOutcome {
    None,
    Applied(Box<crate::model::domain::AppliedCommand>),
    QueuedFamily,
}
