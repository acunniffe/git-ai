use super::CheckpointOutboxError;
use crate::model::checkpoint_delivery::CheckpointDelivery;
use std::path::{Path, PathBuf};

#[cfg(all(test, target_os = "macos"))]
mod macos_acl_tests;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform_acl;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix_durability;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) use unix::open_private_record_at;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) use unix_durability::open_existing_root_path;

pub const DEFAULT_MAX_ENCODED_RECORD_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_READY_RECORDS: usize = 4_096;
pub const DEFAULT_MAX_READY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_READY_FILENAME_BYTES: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboxLimits {
    pub max_encoded_record_bytes: u64,
    pub max_ready_records: usize,
    pub max_ready_bytes: u64,
}

impl Default for OutboxLimits {
    fn default() -> Self {
        Self {
            max_encoded_record_bytes: DEFAULT_MAX_ENCODED_RECORD_BYTES,
            max_ready_records: DEFAULT_MAX_READY_RECORDS,
            max_ready_bytes: DEFAULT_MAX_READY_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedRecord {
    pub path: PathBuf,
    pub encoded_bytes: u64,
}

pub fn ready_filename(delivery: &CheckpointDelivery) -> Result<String, CheckpointOutboxError> {
    delivery.validate()?;
    let filename = format!(
        "{:020}-{}-{:010}-{}.ready",
        delivery.captured_at_unix_ms,
        delivery.batch_id,
        delivery.batch_ordinal,
        delivery.delivery_id
    );
    if filename.len() > MAX_READY_FILENAME_BYTES {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }
    Ok(filename)
}

pub fn publish_delivery(
    root: &Path,
    delivery: &CheckpointDelivery,
) -> Result<PublishedRecord, CheckpointOutboxError> {
    publish_delivery_with_limits(root, delivery, OutboxLimits::default())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn write_private_marker(
    root: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<(), CheckpointOutboxError> {
    validate_private_marker_name(name)?;
    unix::write_private_marker(root, name, bytes)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn write_private_marker(
    root: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<(), CheckpointOutboxError> {
    validate_private_marker_name(name)?;
    let _ = (root, bytes);
    Err(CheckpointOutboxError::UnsupportedPlatform)
}

fn validate_private_marker_name(name: &str) -> Result<(), CheckpointOutboxError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > MAX_READY_FILENAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn publish_delivery_with_limits(
    root: &Path,
    delivery: &CheckpointDelivery,
    limits: OutboxLimits,
) -> Result<PublishedRecord, CheckpointOutboxError> {
    let filename = ready_filename(delivery)?;
    let bytes = super::encode_delivery_with_limit(delivery, limits.max_encoded_record_bytes)?;
    unix::publish_encoded(root, &filename, &bytes, limits)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn publish_delivery_with_limits(
    root: &Path,
    delivery: &CheckpointDelivery,
    limits: OutboxLimits,
) -> Result<PublishedRecord, CheckpointOutboxError> {
    let _ = (root, delivery, limits);
    Err(CheckpointOutboxError::UnsupportedPlatform)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::super::{decode_delivery, encode_delivery};
    use super::*;
    use crate::model::checkpoint_delivery::CheckpointDelivery;
    use crate::model::checkpoint_request::{CheckpointRequest, PreparedPathRole};
    use crate::model::working_log::CheckpointKind;
    use std::collections::HashMap;
    use std::fs;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    fn delivery_with_trace(trace_id: &str) -> CheckpointDelivery {
        let request = CheckpointRequest {
            trace_id: trace_id.to_string(),
            checkpoint_kind: CheckpointKind::Human,
            agent_id: None,
            files: Vec::new(),
            path_role: PreparedPathRole::Edited,
            stream_source: None,
            metadata: HashMap::new(),
        };
        CheckpointDelivery::from_requests_at(vec![request], 42).remove(0)
    }

    fn entries(root: &Path) -> Vec<PathBuf> {
        let mut paths: Vec<_> = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        paths.sort();
        paths
    }

    fn tiny_limits() -> OutboxLimits {
        OutboxLimits {
            max_encoded_record_bytes: 1024 * 1024,
            max_ready_records: 1,
            max_ready_bytes: 8 * 1024 * 1024,
        }
    }

    #[test]
    fn publication_creates_private_root_and_complete_private_ready_record() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("nested").join("outbox");
        let delivery = delivery_with_trace("trace-1");

        let published = publish_delivery_with_limits(&root, &delivery, tiny_limits()).unwrap();

        let root_metadata = fs::symlink_metadata(&root).unwrap();
        let record_metadata = fs::symlink_metadata(&published.path).unwrap();
        assert!(root_metadata.is_dir());
        assert_eq!(root_metadata.mode() & 0o777, 0o700);
        assert_eq!(root_metadata.uid(), unsafe { libc::geteuid() });
        assert!(record_metadata.is_file());
        assert_eq!(record_metadata.mode() & 0o777, 0o600);
        assert_eq!(record_metadata.nlink(), 1);
        assert_eq!(
            decode_delivery(&fs::read(&published.path).unwrap())
                .unwrap()
                .delivery_id,
            delivery.delivery_id
        );
        assert_eq!(entries(&root), vec![published.path]);
    }

    #[test]
    fn symlink_root_is_rejected_without_writing_through_it() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let root = temp.path().join("outbox");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, &root).unwrap();

        assert!(matches!(
            publish_delivery_with_limits(
                &root,
                &delivery_with_trace("secret-trace"),
                tiny_limits()
            ),
            Err(CheckpointOutboxError::RootIsSymlink)
        ));
        assert!(entries(&target).is_empty());
    }

    #[test]
    fn non_private_existing_root_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            publish_delivery_with_limits(&root, &delivery_with_trace("trace"), tiny_limits()),
            Err(CheckpointOutboxError::RootModeMismatch {
                expected: 0o700,
                actual: 0o755
            })
        ));
        assert!(entries(&root).is_empty());
    }

    #[test]
    fn non_directory_root_is_rejected_before_opening_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        fs::write(&root, b"not a directory").unwrap();

        assert!(matches!(
            publish_delivery_with_limits(&root, &delivery_with_trace("trace"), tiny_limits()),
            Err(CheckpointOutboxError::RootIsNotDirectory)
        ));
        assert_eq!(fs::read(&root).unwrap(), b"not a directory");
    }

    #[test]
    fn overlong_ready_filename_is_rejected_before_touching_the_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let mut delivery = delivery_with_trace("trace");
        delivery.delivery_id = "a".repeat(256);

        assert!(matches!(
            publish_delivery_with_limits(&root, &delivery, tiny_limits()),
            Err(CheckpointOutboxError::UnsafeReadyRecord)
        ));
        assert!(!root.exists());
    }

    #[test]
    fn unsafe_private_marker_names_are_rejected_before_touching_the_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");

        for name in ["", ".", "..", "../escape", "nested/marker", "marker\0name"] {
            assert!(matches!(
                write_private_marker(&root, name, b"redacted"),
                Err(CheckpointOutboxError::UnsafeReadyRecord)
            ));
        }
        let overlong = "a".repeat(MAX_READY_FILENAME_BYTES + 1);
        assert!(matches!(
            write_private_marker(&root, &overlong, b"redacted"),
            Err(CheckpointOutboxError::UnsafeReadyRecord)
        ));
        assert!(!root.exists());
    }

    #[test]
    fn actual_encoded_record_size_is_bounded_before_publication() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let delivery = delivery_with_trace(&"sensitive".repeat(64));
        let encoded_len = encode_delivery(&delivery).unwrap().len() as u64;
        let limits = OutboxLimits {
            max_encoded_record_bytes: encoded_len - 1,
            ..tiny_limits()
        };

        let error = publish_delivery_with_limits(&root, &delivery, limits).unwrap_err();

        assert!(matches!(
            error,
            CheckpointOutboxError::RecordTooLarge {
                encoded_bytes,
                max_bytes
            } if encoded_bytes == encoded_len && max_bytes == encoded_len - 1
        ));
        assert!(!root.exists());
        assert!(!error.to_string().contains("sensitive"));
    }

    #[test]
    fn ready_count_capacity_rejects_new_record_without_eviction() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let limits = OutboxLimits {
            max_ready_records: 1,
            ..tiny_limits()
        };
        let first =
            publish_delivery_with_limits(&root, &delivery_with_trace("first"), limits).unwrap();

        assert!(matches!(
            publish_delivery_with_limits(&root, &delivery_with_trace("second"), limits),
            Err(CheckpointOutboxError::ReadyCapacityExceeded {
                ready_records: 1,
                max_records: 1,
                ..
            })
        ));
        assert_eq!(entries(&root), vec![first.path.clone()]);
    }

    #[test]
    fn ready_byte_capacity_counts_encoded_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let first_delivery = delivery_with_trace("first");
        let first_size = encode_delivery(&first_delivery).unwrap().len() as u64;
        let limits = OutboxLimits {
            max_ready_bytes: first_size,
            ..tiny_limits()
        };
        let first = publish_delivery_with_limits(&root, &first_delivery, limits).unwrap();

        assert!(matches!(
            publish_delivery_with_limits(&root, &delivery_with_trace("second"), limits),
            Err(CheckpointOutboxError::ReadyCapacityExceeded {
                ready_bytes,
                max_bytes
                , ..
            }) if ready_bytes == first_size && max_bytes == first_size
        ));
        assert_eq!(entries(&root), vec![first.path]);
    }

    #[test]
    fn orphaned_temporary_records_count_toward_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let mut orphan = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join(".orphan.tmp"))
            .unwrap();
        orphan.write_all(b"orphaned").unwrap();
        orphan.sync_all().unwrap();
        let limits = OutboxLimits {
            max_ready_records: 1,
            ..tiny_limits()
        };

        assert!(matches!(
            publish_delivery_with_limits(&root, &delivery_with_trace("next"), limits),
            Err(CheckpointOutboxError::ReadyCapacityExceeded {
                ready_records: 1,
                max_records: 1,
                ..
            })
        ));
        assert_eq!(entries(&root), vec![root.join(".orphan.tmp")]);
    }

    #[test]
    fn colliding_filename_with_different_or_corrupt_bytes_is_unsafe() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let delivery = delivery_with_trace("original");
        let first = publish_delivery_with_limits(&root, &delivery, tiny_limits()).unwrap();
        let original = fs::read(&first.path).unwrap();
        let mut duplicate = delivery.clone();
        duplicate.request.trace_id = "sensitive-replacement".to_string();

        let error = publish_delivery_with_limits(&root, &duplicate, tiny_limits()).unwrap_err();

        assert!(matches!(error, CheckpointOutboxError::UnsafeReadyRecord));
        assert_eq!(fs::read(&first.path).unwrap(), original);
        assert_eq!(entries(&root), vec![first.path.clone()]);
        assert!(!error.to_string().contains("sensitive-replacement"));

        fs::write(&first.path, b"corrupt").unwrap();
        let error = publish_delivery_with_limits(&root, &delivery, tiny_limits()).unwrap_err();
        assert!(matches!(error, CheckpointOutboxError::UnsafeReadyRecord));
    }

    #[test]
    fn byte_identical_secure_record_is_already_published_at_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let delivery = delivery_with_trace("same");
        let first = publish_delivery_with_limits(&root, &delivery, tiny_limits()).unwrap();
        let original = fs::read(&first.path).unwrap();

        let error = publish_delivery_with_limits(&root, &delivery, tiny_limits()).unwrap_err();

        assert!(matches!(error, CheckpointOutboxError::AlreadyPublished));
        assert_eq!(fs::read(&first.path).unwrap(), original);
    }

    #[test]
    fn concurrent_duplicate_publication_leaves_one_complete_ready_record() {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(temp.path().join("outbox"));
        let delivery = Arc::new(delivery_with_trace("shared"));
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let root = Arc::clone(&root);
                let delivery = Arc::clone(&delivery);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    publish_delivery_with_limits(&root, &delivery, tiny_limits())
                })
            })
            .collect();
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .all(|error| matches!(
                    error,
                    CheckpointOutboxError::AlreadyPublished | CheckpointOutboxError::LockBusy
                ))
        );
        let paths = entries(&root);
        assert_eq!(paths.len(), 1);
        assert!(paths[0].extension().is_some_and(|value| value == "ready"));
        decode_delivery(&fs::read(&paths[0]).unwrap()).unwrap();
    }

    #[test]
    fn symlink_ready_record_is_rejected_and_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let target = temp.path().join("target");
        fs::write(&target, b"keep").unwrap();
        symlink(&target, root.join("attacker.ready")).unwrap();

        assert!(matches!(
            publish_delivery_with_limits(&root, &delivery_with_trace("trace"), tiny_limits()),
            Err(CheckpointOutboxError::UnsafeReadyRecord)
        ));
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert_eq!(entries(&root).len(), 1);
    }

    #[test]
    fn ownership_validation_rejects_a_foreign_uid() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = fs::metadata(&root).unwrap();

        assert!(matches!(
            unix::validate_root_metadata(&metadata, metadata.uid().saturating_add(1)),
            Err(CheckpointOutboxError::RootOwnerMismatch { .. })
        ));
    }

    #[test]
    fn held_root_lock_fails_fast_instead_of_blocking_the_hook() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let held = fs::OpenOptions::new().read(true).open(&root).unwrap();
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);

        let (result_tx, result_rx) = mpsc::channel();
        let publish_root = root.clone();
        std::thread::spawn(move || {
            let result = publish_delivery_with_limits(
                &publish_root,
                &delivery_with_trace("contended"),
                tiny_limits(),
            );
            let _ = result_tx.send(result);
        });

        let result = result_rx.recv_timeout(Duration::from_millis(250));
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        let result = match result {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = result_rx.recv_timeout(Duration::from_secs(1));
                panic!("checkpoint publication blocked on a held outbox lock")
            }
            Err(error) => panic!("checkpoint publication thread failed: {error}"),
        };
        assert!(matches!(result, Err(CheckpointOutboxError::LockBusy)));
    }

    #[test]
    fn capacity_scan_stays_on_opened_root_when_path_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let parked_root = temp.path().join("parked-outbox");
        let first =
            publish_delivery_with_limits(&root, &delivery_with_trace("first"), tiny_limits())
                .unwrap();
        let directory = unix::test_open_secure_root(&root).unwrap();
        fs::rename(&root, &parked_root).unwrap();
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let limits = OutboxLimits {
            max_ready_records: 1,
            ..tiny_limits()
        };

        assert!(matches!(
            unix::test_enforce_capacity_for_open_root(&directory, &root, 1, limits),
            Err(CheckpointOutboxError::ReadyCapacityExceeded {
                ready_records: 1,
                max_records: 1,
                ..
            })
        ));
        assert_eq!(
            entries(&parked_root),
            vec![parked_root.join(first.path.file_name().unwrap())]
        );
        assert!(entries(&root).is_empty());
    }

    #[test]
    fn acknowledgement_rejects_replaced_root_and_same_named_record() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let parked_root = temp.path().join("parked-outbox");
        let published =
            publish_delivery_with_limits(&root, &delivery_with_trace("original"), tiny_limits())
                .unwrap();
        let ready_name = published.path.file_name().unwrap().to_str().unwrap();
        let original_bytes = fs::read(&published.path).unwrap();
        let directory = unix::test_open_secure_root(&root).unwrap();
        fs::rename(&root, &parked_root).unwrap();
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let replacement = root.join(ready_name);
        fs::write(&replacement, b"replacement").unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(matches!(
            unix::test_validate_acknowledged_path(&root, &directory, ready_name),
            Err(CheckpointOutboxError::UnsafeReadyRecord)
        ));
        assert_eq!(
            fs::read(parked_root.join(ready_name)).unwrap(),
            original_bytes
        );
        assert_eq!(fs::read(replacement).unwrap(), b"replacement");
    }
}
