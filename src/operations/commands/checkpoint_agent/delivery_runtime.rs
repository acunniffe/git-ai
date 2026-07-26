use super::delivery::{CheckpointDeliveryReport, deliver_checkpoint_batch};
use crate::model::checkpoint_delivery::CheckpointDelivery;
use crate::model::daemon_control::ControlRequest;
use crate::model::repository::checkpoint_outbox::{
    CheckpointOutboxError, OutboxFailureClass, candidate_roots, publish_delivery,
    record_publication_failure,
};
use crate::operations::daemon::DaemonConfig;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CHECKPOINT_DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const CHECKPOINT_OUTBOX_DIR_ENV: &str = "GIT_AI_CHECKPOINT_OUTBOX_DIR";
const CHECKPOINT_OUTBOX_LOCK_MAX_ATTEMPTS: usize = 3;
const CHECKPOINT_OUTBOX_MANAGED_LOCK_MAX_ATTEMPTS: usize = 11;
const CHECKPOINT_OUTBOX_LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);

pub(crate) struct CheckpointOutboxRootSelection {
    pub roots: Vec<PathBuf>,
    pub invalid_override: bool,
    pub managed_override: bool,
}

pub fn deliver_authorized_checkpoint_batch(
    config: &DaemonConfig,
    deliveries: &[CheckpointDelivery],
) -> CheckpointDeliveryReport {
    let live_config = ensure_checkpoint_daemon_running().unwrap_or_else(|_| config.clone());
    let selection = checkpoint_outbox_root_selection(config);

    deliver_checkpoint_batch(
        deliveries,
        |delivery| {
            crate::operations::daemon::send_control_request(
                &live_config.control_socket_path,
                &ControlRequest::CheckpointDeliver {
                    delivery: Box::new(delivery.clone()),
                },
            )
        },
        |delivery| {
            publish_to_candidate_roots(&selection.roots, selection.managed_override, delivery)
        },
    )
}

pub(crate) fn ensure_checkpoint_daemon_running() -> Result<DaemonConfig, String> {
    crate::operations::commands::daemon::ensure_daemon_running(CHECKPOINT_DAEMON_START_TIMEOUT)
}

pub(crate) fn checkpoint_outbox_root_selection(
    config: &DaemonConfig,
) -> CheckpointOutboxRootSelection {
    let explicit = std::env::var_os(CHECKPOINT_OUTBOX_DIR_ENV).map(PathBuf::from);
    checkpoint_outbox_root_selection_with(
        config,
        explicit.as_deref(),
        &std::env::temp_dir(),
        effective_uid(),
    )
}

fn checkpoint_outbox_root_selection_with(
    config: &DaemonConfig,
    explicit_override: Option<&Path>,
    temp_dir: &Path,
    effective_uid: u32,
) -> CheckpointOutboxRootSelection {
    let invalid_override = explicit_override.is_some_and(|path| !path.is_absolute());
    let valid_override = explicit_override.filter(|path| path.is_absolute());
    let roots = candidate_roots(
        &config.internal_dir,
        valid_override,
        temp_dir,
        effective_uid,
    )
    .unwrap_or_default();
    CheckpointOutboxRootSelection {
        roots,
        invalid_override,
        managed_override: valid_override.is_some(),
    }
}

fn effective_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and does not mutate process state.
        unsafe { libc::geteuid() }
    }

    #[cfg(not(unix))]
    {
        0
    }
}

fn publish_to_candidate_roots(
    roots: &[PathBuf],
    managed_override: bool,
    delivery: &CheckpointDelivery,
) -> Result<(), ()> {
    publish_to_candidate_roots_with(
        roots,
        managed_override,
        delivery,
        |root, delivery| match publish_delivery(root, delivery) {
            Ok(_) | Err(CheckpointOutboxError::AlreadyPublished) => Ok(()),
            Err(error) => Err(error),
        },
        record_publication_failure,
        std::thread::sleep,
    )
}

fn publish_to_candidate_roots_with<Publish, RecordFailure, Sleep>(
    roots: &[PathBuf],
    managed_override: bool,
    delivery: &CheckpointDelivery,
    mut publish: Publish,
    mut record_failure: RecordFailure,
    mut sleep: Sleep,
) -> Result<(), ()>
where
    Publish: FnMut(&Path, &CheckpointDelivery) -> Result<(), CheckpointOutboxError>,
    RecordFailure: FnMut(&Path, OutboxFailureClass) -> Result<(), CheckpointOutboxError>,
    Sleep: FnMut(Duration),
{
    let mut failures = Vec::with_capacity(roots.len());
    for (index, root) in roots.iter().enumerate() {
        let max_lock_attempts = if index == 0 && managed_override {
            CHECKPOINT_OUTBOX_MANAGED_LOCK_MAX_ATTEMPTS
        } else {
            CHECKPOINT_OUTBOX_LOCK_MAX_ATTEMPTS
        };
        let mut attempt = 1;
        let error = loop {
            match publish(root, delivery) {
                Ok(()) => return Ok(()),
                Err(error) if is_lock_busy(&error) && attempt < max_lock_attempts => {
                    attempt += 1;
                    sleep(CHECKPOINT_OUTBOX_LOCK_RETRY_DELAY);
                }
                Err(error) => break error,
            }
        };
        let lock_busy = is_lock_busy(&error);
        failures.push((root, OutboxFailureClass::from_error(&error)));
        if index == 0 && managed_override && lock_busy {
            break;
        }
    }

    for (root, class) in failures {
        if record_failure(root, class).is_ok() {
            break;
        }
    }
    Err(())
}

fn is_lock_busy(error: &CheckpointOutboxError) -> bool {
    matches!(error, CheckpointOutboxError::LockBusy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::checkpoint_request::{CheckpointRequest, PreparedPathRole};
    use crate::model::working_log::CheckpointKind;
    use std::collections::HashMap;

    fn delivery() -> CheckpointDelivery {
        CheckpointDelivery::from_requests_at(
            vec![CheckpointRequest {
                trace_id: "trace-1".to_string(),
                checkpoint_kind: CheckpointKind::Human,
                agent_id: None,
                files: Vec::new(),
                path_role: PreparedPathRole::Edited,
                stream_source: None,
                metadata: HashMap::new(),
            }],
            42,
        )
        .remove(0)
    }

    #[test]
    fn relative_override_does_not_disable_derived_fallback_roots() {
        let temp = tempfile::tempdir().unwrap();
        let config = DaemonConfig::from_home(&temp.path().join("home"));
        let selection = checkpoint_outbox_root_selection_with(
            &config,
            Some(Path::new("relative/outbox")),
            temp.path(),
            501,
        );
        let expected = candidate_roots(&config.internal_dir, None, temp.path(), 501).unwrap();

        assert!(selection.invalid_override);
        assert_eq!(selection.roots, expected);
        assert!(!selection.roots.is_empty());
    }

    #[test]
    fn absolute_override_marks_the_first_root_as_managed() {
        let temp = tempfile::tempdir().unwrap();
        let config = DaemonConfig::from_home(&temp.path().join("home"));
        let managed = temp.path().join("managed-outbox");

        let selection =
            checkpoint_outbox_root_selection_with(&config, Some(&managed), temp.path(), 501);

        assert!(!selection.invalid_override);
        assert!(selection.managed_override);
        assert_eq!(selection.roots.first(), Some(&managed));
    }

    #[test]
    fn checkpoint_outbox_publication_uses_first_writable_candidate() {
        let roots = vec![PathBuf::from("/first"), PathBuf::from("/second")];
        let mut attempted = Vec::new();

        let result = publish_to_candidate_roots_with(
            &roots,
            false,
            &delivery(),
            |root, _| {
                attempted.push(root.to_path_buf());
                if root == Path::new("/second") {
                    Ok(())
                } else {
                    Err(CheckpointOutboxError::RootIsNotDirectory)
                }
            },
            |_, _| Ok(()),
            |_| {},
        );

        assert_eq!(result, Ok(()));
        assert_eq!(attempted, roots);
    }

    #[test]
    fn checkpoint_outbox_publication_stops_after_success() {
        let roots = vec![
            PathBuf::from("/first"),
            PathBuf::from("/second"),
            PathBuf::from("/third"),
        ];
        let mut attempted = Vec::new();

        let result = publish_to_candidate_roots_with(
            &roots,
            false,
            &delivery(),
            |root, _| {
                attempted.push(root.to_path_buf());
                Ok(())
            },
            |_, _| Ok(()),
            |_| {},
        );

        assert_eq!(result, Ok(()));
        assert_eq!(attempted, [PathBuf::from("/first")]);
    }

    #[test]
    fn managed_outbox_retries_transient_lock_contention_without_falling_back() {
        let roots = vec![PathBuf::from("/managed"), PathBuf::from("/sandbox-local")];
        let mut attempted = Vec::new();
        let mut slept = Vec::new();
        let mut managed_attempts = 0;

        let result = publish_to_candidate_roots_with(
            &roots,
            true,
            &delivery(),
            |root, _| {
                attempted.push(root.to_path_buf());
                if root == Path::new("/sandbox-local") {
                    panic!("managed contention must not fall back");
                }
                managed_attempts += 1;
                if managed_attempts == 1 {
                    Err(CheckpointOutboxError::LockBusy)
                } else {
                    Ok(())
                }
            },
            |_, _| Ok(()),
            |delay| slept.push(delay),
        );

        assert_eq!(result, Ok(()));
        assert_eq!(
            attempted,
            [PathBuf::from("/managed"), PathBuf::from("/managed")]
        );
        assert_eq!(slept, [CHECKPOINT_OUTBOX_LOCK_RETRY_DELAY]);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn managed_outbox_persistent_lock_records_marker_in_managed_root() {
        use crate::model::repository::checkpoint_outbox::{OutboxRootState, inspect_outbox_root};
        use std::fs;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let managed = temp.path().join("managed");
        let diagnostic_fallback = temp.path().join("diagnostic-fallback");
        fs::create_dir(&managed).unwrap();
        fs::set_permissions(&managed, fs::Permissions::from_mode(0o700)).unwrap();

        let managed_lock = fs::File::open(&managed).unwrap();
        assert_eq!(
            unsafe { libc::flock(managed_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB,) },
            0
        );

        let result = publish_to_candidate_roots(
            &[managed.clone(), diagnostic_fallback.clone()],
            true,
            &delivery(),
        );

        assert_eq!(result, Err(()));
        let managed_status = inspect_outbox_root(&managed);
        assert_eq!(managed_status.state, OutboxRootState::Ready);
        assert_eq!(managed_status.pending_records, 0);
        assert_eq!(
            managed_status
                .last_failure
                .expect("redacted failure marker")
                .class,
            OutboxFailureClass::LockBusy
        );
        assert!(
            !diagnostic_fallback.exists(),
            "a managed checkpoint and its diagnostic must remain in the shared managed root"
        );
    }

    #[test]
    fn checkpoint_outbox_publication_reports_all_candidates_unavailable() {
        let roots = vec![PathBuf::from("/first"), PathBuf::from("/second")];
        let mut attempted = Vec::new();

        let result = publish_to_candidate_roots_with(
            &roots,
            false,
            &delivery(),
            |root, _| {
                attempted.push(root.to_path_buf());
                Err(CheckpointOutboxError::Io {
                    operation: "write temporary record",
                    kind: std::io::ErrorKind::StorageFull,
                })
            },
            |_, _| Ok(()),
            |_| {},
        );

        assert_eq!(result, Err(()));
        assert_eq!(attempted, roots);
    }

    #[test]
    fn failed_publication_records_one_redacted_failure_after_all_candidates_fail() {
        let roots = vec![PathBuf::from("/first"), PathBuf::from("/second")];
        let mut recorded = Vec::new();

        let result = publish_to_candidate_roots_with(
            &roots,
            false,
            &delivery(),
            |root, _| {
                if root == Path::new("/first") {
                    Err(CheckpointOutboxError::RootIsSymlink)
                } else {
                    Err(CheckpointOutboxError::ReadyCapacityExceeded {
                        ready_records: 8,
                        max_records: 8,
                        ready_bytes: 10,
                        max_bytes: 10,
                    })
                }
            },
            |root, class| {
                recorded.push((root.to_path_buf(), class));
                if root == Path::new("/second") {
                    Ok(())
                } else {
                    Err(CheckpointOutboxError::RootIsSymlink)
                }
            },
            |_| {},
        );

        assert_eq!(result, Err(()));
        assert_eq!(
            recorded,
            [
                (PathBuf::from("/first"), OutboxFailureClass::InvalidRoot),
                (PathBuf::from("/second"), OutboxFailureClass::Capacity),
            ]
        );
    }

    #[test]
    fn eventual_publication_success_does_not_record_a_loss_sentinel() {
        let roots = vec![PathBuf::from("/first"), PathBuf::from("/second")];
        let mut recorded = Vec::new();

        let result = publish_to_candidate_roots_with(
            &roots,
            false,
            &delivery(),
            |root, _| {
                if root == Path::new("/second") {
                    Ok(())
                } else {
                    Err(CheckpointOutboxError::RootIsNotDirectory)
                }
            },
            |root, class| {
                recorded.push((root.to_path_buf(), class));
                Ok(())
            },
            |_| {},
        );

        assert_eq!(result, Ok(()));
        assert!(recorded.is_empty());
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn repeated_publication_is_already_durable_in_the_first_root() {
        let temp = tempfile::tempdir().unwrap();
        let roots = vec![temp.path().join("first"), temp.path().join("second")];
        let delivery = delivery();

        publish_to_candidate_roots(&roots, false, &delivery).unwrap();
        publish_to_candidate_roots(&roots, false, &delivery).unwrap();

        assert!(roots[0].exists());
        assert!(
            !roots[1].exists(),
            "an existing delivery must not be copied into a fallback root"
        );
    }
}
