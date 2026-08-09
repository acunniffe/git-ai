use super::CheckpointOutboxError;
use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const FAILURE_SENTINEL_FILENAME: &str = "last-publication-error.cbor";
const FAILURE_SENTINEL_SCHEMA_VERSION: u8 = 1;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAX_FAILURE_SENTINEL_BYTES: u64 = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxFailureClass {
    Capacity,
    InvalidDelivery,
    InvalidRoot,
    LockBusy,
    RecordTooLarge,
    Storage,
    UnsupportedPlatform,
}

impl OutboxFailureClass {
    pub fn from_error(error: &CheckpointOutboxError) -> Self {
        match error {
            CheckpointOutboxError::Delivery(_)
            | CheckpointOutboxError::Encode(_)
            | CheckpointOutboxError::Decode(_) => Self::InvalidDelivery,
            CheckpointOutboxError::OverrideMustBeAbsolute(_)
            | CheckpointOutboxError::RootIsSymlink
            | CheckpointOutboxError::RootIsNotDirectory
            | CheckpointOutboxError::RootOwnerMismatch { .. }
            | CheckpointOutboxError::RootModeMismatch { .. }
            | CheckpointOutboxError::UnsafeReadyRecord => Self::InvalidRoot,
            CheckpointOutboxError::RecordTooLarge { .. } => Self::RecordTooLarge,
            CheckpointOutboxError::ReadyCapacityExceeded { .. } => Self::Capacity,
            CheckpointOutboxError::UnsupportedPlatform => Self::UnsupportedPlatform,
            CheckpointOutboxError::LockBusy => Self::LockBusy,
            CheckpointOutboxError::Io { .. } | CheckpointOutboxError::AlreadyPublished => {
                Self::Storage
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Capacity => "capacity",
            Self::InvalidDelivery => "invalid_delivery",
            Self::InvalidRoot => "invalid_root",
            Self::LockBusy => "lock_busy",
            Self::RecordTooLarge => "record_too_large",
            Self::Storage => "storage",
            Self::UnsupportedPlatform => "unsupported_platform",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedOutboxFailure {
    schema_version: u8,
    pub recorded_at_unix_ms: u64,
    pub class: OutboxFailureClass,
}

impl RedactedOutboxFailure {
    #[cfg(test)]
    pub(crate) fn new_for_test(recorded_at_unix_ms: u64, class: OutboxFailureClass) -> Self {
        Self {
            schema_version: FAILURE_SENTINEL_SCHEMA_VERSION,
            recorded_at_unix_ms,
            class,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxRootState {
    Missing,
    Ready,
    Invalid,
    Unavailable,
}

impl OutboxRootState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Ready => "ready",
            Self::Invalid => "invalid",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRootStatus {
    pub state: OutboxRootState,
    pub pending_records: usize,
    pub pending_bytes: u64,
    pub oldest_ready_age_ms: Option<u64>,
    pub last_failure: Option<RedactedOutboxFailure>,
}

pub fn record_publication_failure(
    root: &Path,
    class: OutboxFailureClass,
) -> Result<(), CheckpointOutboxError> {
    record_publication_failure_at(root, class, unix_time_ms())
}

pub fn inspect_outbox_root(root: &Path) -> OutboxRootStatus {
    inspect_outbox_root_at(root, unix_time_ms())
}

fn record_publication_failure_at(
    root: &Path,
    class: OutboxFailureClass,
    recorded_at_unix_ms: u64,
) -> Result<(), CheckpointOutboxError> {
    let sentinel = RedactedOutboxFailure {
        schema_version: FAILURE_SENTINEL_SCHEMA_VERSION,
        recorded_at_unix_ms,
        class,
    };
    let mut bytes = Vec::new();
    ciborium::into_writer(&sentinel, &mut bytes)
        .map_err(|error| CheckpointOutboxError::Encode(error.to_string()))?;
    super::publication::write_private_marker(root, FAILURE_SENTINEL_FILENAME, &bytes)
}

fn inspect_outbox_root_at(root: &Path, now_unix_ms: u64) -> OutboxRootStatus {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        inspect_supported_outbox_root(root, now_unix_ms)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, now_unix_ms);
        unavailable_status()
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn unavailable_status() -> OutboxRootStatus {
    OutboxRootStatus {
        state: OutboxRootState::Unavailable,
        pending_records: 0,
        pending_bytes: 0,
        oldest_ready_age_ms: None,
        last_failure: None,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn inspect_supported_outbox_root(root: &Path, now_unix_ms: u64) -> OutboxRootStatus {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let initial = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return OutboxRootStatus {
                state: OutboxRootState::Missing,
                ..unavailable_status()
            };
        }
        Err(_) => return unavailable_status(),
    };
    if !root_metadata_is_private(&initial) {
        return invalid_status();
    }
    let root_directory = match super::publication::open_existing_root_path(root) {
        Ok(directory) => directory,
        Err(error) => {
            return match error {
                CheckpointOutboxError::Io { .. } | CheckpointOutboxError::UnsupportedPlatform => {
                    unavailable_status()
                }
                _ => invalid_status(),
            };
        }
    };
    let opened_root = match root_directory.metadata() {
        Ok(metadata) => metadata,
        Err(_) => return unavailable_status(),
    };
    if opened_root.dev() != initial.dev() || opened_root.ino() != initial.ino() {
        return invalid_status();
    }

    let record_error_status = |error| match error {
        CheckpointOutboxError::Io { .. } | CheckpointOutboxError::UnsupportedPlatform => {
            unavailable_status()
        }
        _ => invalid_status(),
    };

    let mut pending_records = 0usize;
    let mut pending_bytes = 0u64;
    let mut oldest_capture_ms: Option<u64> = None;
    let mut last_failure = None;
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return unavailable_status(),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return unavailable_status(),
        };
        let name = entry.file_name();
        let name_bytes = name.as_bytes();
        let is_ready = name_bytes.ends_with(b".ready");
        let is_temporary = name_bytes.starts_with(b".") && name_bytes.ends_with(b".tmp");
        if is_ready || is_temporary {
            let record = match super::publication::open_private_record_at(&root_directory, &name) {
                Ok(record) => record,
                Err(error) => return record_error_status(error),
            };
            let metadata = match record.metadata() {
                Ok(metadata) => metadata,
                Err(_) => return unavailable_status(),
            };
            pending_records = pending_records.saturating_add(1);
            pending_bytes = pending_bytes.saturating_add(metadata.len());
            if is_ready && let Some(captured_at) = captured_at_from_ready_name(name_bytes) {
                oldest_capture_ms =
                    Some(oldest_capture_ms.map_or(captured_at, |oldest| oldest.min(captured_at)));
            }
        } else if name_bytes == FAILURE_SENTINEL_FILENAME.as_bytes() {
            let record = match super::publication::open_private_record_at(&root_directory, &name) {
                Ok(record) => record,
                Err(error) => return record_error_status(error),
            };
            last_failure = match read_failure_sentinel(record) {
                Ok(value) => Some(value),
                Err(_) => return invalid_status(),
            };
        }
    }

    let current = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(_) => return unavailable_status(),
    };
    if !root_metadata_is_private(&current)
        || current.dev() != initial.dev()
        || current.ino() != initial.ino()
    {
        return invalid_status();
    }

    OutboxRootStatus {
        state: OutboxRootState::Ready,
        pending_records,
        pending_bytes,
        oldest_ready_age_ms: oldest_capture_ms
            .map(|captured_at| now_unix_ms.saturating_sub(captured_at)),
        last_failure,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn invalid_status() -> OutboxRootStatus {
    OutboxRootStatus {
        state: OutboxRootState::Invalid,
        ..unavailable_status()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn root_metadata_is_private(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.file_type().is_dir()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o777 == 0o700
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_failure_sentinel(file: fs::File) -> Result<RedactedOutboxFailure, ()> {
    read_failure_sentinel_with_after_metadata(file, || {})
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_failure_sentinel_with_after_metadata<AfterMetadata>(
    file: fs::File,
    after_metadata: AfterMetadata,
) -> Result<RedactedOutboxFailure, ()>
where
    AfterMetadata: FnOnce(),
{
    use std::io::Read;

    let metadata = file.metadata().map_err(|_| ())?;
    if metadata.len() > MAX_FAILURE_SENTINEL_BYTES {
        return Err(());
    }
    after_metadata();
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_FAILURE_SENTINEL_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > MAX_FAILURE_SENTINEL_BYTES {
        return Err(());
    }
    let mut cursor = std::io::Cursor::new(&bytes);
    let sentinel: RedactedOutboxFailure = ciborium::from_reader(&mut cursor).map_err(|_| ())?;
    if sentinel.schema_version != FAILURE_SENTINEL_SCHEMA_VERSION
        || cursor.position() != bytes.len() as u64
    {
        return Err(());
    }
    Ok(sentinel)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn captured_at_from_ready_name(name: &[u8]) -> Option<u64> {
    const TIMESTAMP_BYTES: usize = 20;
    if name.len() <= TIMESTAMP_BYTES || name.get(TIMESTAMP_BYTES) != Some(&b'-') {
        return None;
    }
    std::str::from_utf8(&name[..TIMESTAMP_BYTES])
        .ok()?
        .parse()
        .ok()
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::model::checkpoint_delivery::CheckpointDelivery;
    use crate::model::checkpoint_request::{CheckpointRequest, PreparedPathRole};
    use crate::model::repository::checkpoint_outbox::{
        encode_delivery, publish_delivery, ready_filename,
    };
    use crate::model::working_log::CheckpointKind;
    use std::collections::HashMap;
    use std::fs;
    use std::io::ErrorKind::{Other, PermissionDenied, StorageFull};

    fn delivery(trace_id: &str, captured_at_unix_ms: u64) -> CheckpointDelivery {
        CheckpointDelivery::from_requests_at(
            vec![CheckpointRequest {
                trace_id: trace_id.to_string(),
                checkpoint_kind: CheckpointKind::Human,
                agent_id: None,
                files: Vec::new(),
                path_role: PreparedPathRole::Edited,
                stream_source: None,
                metadata: HashMap::new(),
            }],
            captured_at_unix_ms,
        )
        .remove(0)
    }

    #[cfg(target_os = "macos")]
    fn add_everyone_read_acl(path: &Path) {
        let output = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "chmod +a failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn failure_sentinel_round_trips_without_delivery_data() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        publish_delivery(&root, &delivery("sensitive-trace", 42)).unwrap();

        record_publication_failure_at(&root, OutboxFailureClass::Capacity, 1_000).unwrap();

        let bytes = fs::read(root.join(FAILURE_SENTINEL_FILENAME)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("sensitive"));
        let status = inspect_outbox_root_at(&root, 2_000);
        assert_eq!(
            status.last_failure,
            Some(RedactedOutboxFailure {
                schema_version: FAILURE_SENTINEL_SCHEMA_VERSION,
                recorded_at_unix_ms: 1_000,
                class: OutboxFailureClass::Capacity,
            })
        );
    }

    #[test]
    fn failure_sentinel_growth_after_metadata_check_stays_bounded() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("failure.cbor");
        fs::write(&path, b"small").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let file = fs::File::open(&path).unwrap();

        let result = read_failure_sentinel_with_after_metadata(file, || {
            fs::write(&path, vec![b'x'; MAX_FAILURE_SENTINEL_BYTES as usize + 1]).unwrap();
        });

        assert!(result.is_err());
    }

    #[test]
    fn status_reports_only_bounded_backlog_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let first = delivery("first-secret", 100);
        let second = delivery("second-secret", 200);
        let expected_bytes = (encode_delivery(&first).unwrap().len()
            + encode_delivery(&second).unwrap().len()) as u64;
        publish_delivery(&root, &first).unwrap();
        publish_delivery(&root, &second).unwrap();

        let status = inspect_outbox_root_at(&root, 1_100);

        assert_eq!(status.state, OutboxRootState::Ready);
        assert_eq!(status.pending_records, 2);
        assert_eq!(status.pending_bytes, expected_bytes);
        assert_eq!(status.oldest_ready_age_ms, Some(1_000));
        assert_eq!(status.last_failure, None);
        assert!(!format!("{status:?}").contains("secret"));
    }

    #[test]
    fn status_includes_orphaned_temporary_capacity_usage() {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let ready = delivery("ready", 100);
        let encoded = encode_delivery(&ready).unwrap();
        let ready_bytes = encoded.len() as u64;
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let ready_path = root.join(ready_filename(&ready).unwrap());
        fs::write(&ready_path, encoded).unwrap();
        fs::set_permissions(&ready_path, fs::Permissions::from_mode(0o600)).unwrap();
        let orphan = b"orphaned-private-bytes";
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options.open(root.join(".orphan.tmp")).unwrap();
        file.write_all(orphan).unwrap();
        file.sync_all().unwrap();

        let status = inspect_outbox_root_at(&root, 1_100);

        assert_eq!(status.state, OutboxRootState::Ready);
        assert_eq!(status.pending_records, 2);
        assert_eq!(status.pending_bytes, ready_bytes + orphan.len() as u64);
        assert_eq!(status.oldest_ready_age_ms, Some(1_000));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn status_rejects_ready_record_with_allow_acl_as_redacted_invalid_state() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let published = publish_delivery(&root, &delivery("sensitive-trace", 100)).unwrap();
        add_everyone_read_acl(&published.path);
        assert_eq!(fs::metadata(&published.path).unwrap().mode() & 0o777, 0o600);

        let status = inspect_outbox_root_at(&root, 1_100);

        assert_eq!(status.state, OutboxRootState::Invalid);
        assert_eq!(status.pending_records, 0);
        assert_eq!(status.pending_bytes, 0);
        assert_eq!(status.oldest_ready_age_ms, None);
        assert!(!format!("{status:?}").contains("sensitive"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn status_rejects_temporary_and_marker_records_with_allow_acls() {
        use std::os::unix::fs::PermissionsExt;

        let temporary_scope = tempfile::tempdir().unwrap();
        let temporary_root = temporary_scope.path().join("outbox");
        fs::create_dir(&temporary_root).unwrap();
        fs::set_permissions(&temporary_root, fs::Permissions::from_mode(0o700)).unwrap();
        let temporary = temporary_root.join(".orphan.tmp");
        fs::write(&temporary, b"private").unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).unwrap();
        add_everyone_read_acl(&temporary);

        let marker_scope = tempfile::tempdir().unwrap();
        let marker_root = marker_scope.path().join("outbox");
        record_publication_failure_at(&marker_root, OutboxFailureClass::Storage, 100).unwrap();
        add_everyone_read_acl(&marker_root.join(FAILURE_SENTINEL_FILENAME));

        assert_eq!(
            inspect_outbox_root_at(&temporary_root, 1_100).state,
            OutboxRootState::Invalid
        );
        assert_eq!(
            inspect_outbox_root_at(&marker_root, 1_100).state,
            OutboxRootState::Invalid
        );
    }

    #[test]
    fn missing_and_unsafe_roots_have_redacted_states() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        assert_eq!(
            inspect_outbox_root_at(&missing, 1).state,
            OutboxRootState::Missing
        );

        let target = temp.path().join("target");
        let link = temp.path().join("outbox-link");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, &link).unwrap();
        assert_eq!(
            inspect_outbox_root_at(&link, 1).state,
            OutboxRootState::Invalid
        );

        let unsafe_parent = temp.path().join("unsafe-parent");
        let privately_modeled_but_replaceable = unsafe_parent.join("outbox");
        fs::create_dir(&unsafe_parent).unwrap();
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();
        fs::create_dir(&privately_modeled_but_replaceable).unwrap();
        fs::set_permissions(
            &privately_modeled_but_replaceable,
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert_eq!(
            inspect_outbox_root_at(&privately_modeled_but_replaceable, 1).state,
            OutboxRootState::Invalid,
            "diagnostics must report a private leaf under a replaceable ancestor as invalid"
        );
    }

    #[test]
    fn io_error_conversion_preserves_the_redacted_contract() {
        for kind in [PermissionDenied, StorageFull, Other] {
            let source = std::io::Error::new(kind, "sensitive detail");
            let error = CheckpointOutboxError::from_io("write temporary record", source);

            let expected = format!("checkpoint outbox write temporary record failed ({kind:?})");
            assert_eq!(error.to_string(), expected);
            let class = OutboxFailureClass::from_error(&error);
            assert_eq!(class, OutboxFailureClass::Storage);
            let actual = match error {
                CheckpointOutboxError::Io { operation, kind } => (operation, kind),
                _ => unreachable!(),
            };
            assert_eq!(actual, ("write temporary record", kind));
        }
    }
}
