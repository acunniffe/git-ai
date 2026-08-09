use crate::model::checkpoint_delivery::{CheckpointDelivery, CheckpointDeliveryError};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

mod diagnostics;
mod publication;

pub use diagnostics::{
    OutboxFailureClass, OutboxRootState, OutboxRootStatus, RedactedOutboxFailure,
    inspect_outbox_root, record_publication_failure,
};
pub use publication::{
    DEFAULT_MAX_ENCODED_RECORD_BYTES, DEFAULT_MAX_READY_BYTES, DEFAULT_MAX_READY_RECORDS,
    OutboxLimits, PublishedRecord, publish_delivery, ready_filename,
};

pub const CHECKPOINT_OUTBOX_VERSION: &str = "checkpoint-outbox-v1";

#[derive(Debug)]
pub enum CheckpointOutboxError {
    Delivery(CheckpointDeliveryError),
    Encode(String),
    Decode(String),
    OverrideMustBeAbsolute(PathBuf),
    UnsupportedPlatform,
    RootIsSymlink,
    RootIsNotDirectory,
    RootOwnerMismatch {
        expected: u32,
        actual: u32,
    },
    RootModeMismatch {
        expected: u32,
        actual: u32,
    },
    UnsafeReadyRecord,
    RecordTooLarge {
        encoded_bytes: u64,
        max_bytes: u64,
    },
    ReadyCapacityExceeded {
        ready_records: usize,
        max_records: usize,
        ready_bytes: u64,
        max_bytes: u64,
    },
    AlreadyPublished,
    LockBusy,
    Io {
        operation: &'static str,
        kind: io::ErrorKind,
    },
}

impl CheckpointOutboxError {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn from_io(operation: &'static str, error: io::Error) -> Self {
        Self::Io {
            operation,
            kind: error.kind(),
        }
    }
}

impl fmt::Display for CheckpointOutboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Delivery(error) => error.fmt(f),
            Self::Encode(error) => write!(f, "failed to encode checkpoint delivery: {}", error),
            Self::Decode(error) => write!(f, "failed to decode checkpoint delivery: {}", error),
            Self::OverrideMustBeAbsolute(path) => write!(
                f,
                "checkpoint outbox override must be absolute: {}",
                path.display()
            ),
            Self::UnsupportedPlatform => {
                write!(f, "durable checkpoint outbox publication is unsupported")
            }
            Self::RootIsSymlink => write!(f, "checkpoint outbox root must not be a symlink"),
            Self::RootIsNotDirectory => {
                write!(f, "checkpoint outbox root must be a directory")
            }
            Self::RootOwnerMismatch { expected, actual } => write!(
                f,
                "checkpoint outbox root owner mismatch (expected uid {}, found {})",
                expected, actual
            ),
            Self::RootModeMismatch { expected, actual } => write!(
                f,
                "checkpoint outbox root mode mismatch (expected {:04o}, found {:04o})",
                expected, actual
            ),
            Self::UnsafeReadyRecord => {
                write!(f, "checkpoint outbox contains an unsafe ready record")
            }
            Self::RecordTooLarge {
                encoded_bytes,
                max_bytes,
            } => write!(
                f,
                "checkpoint outbox record is too large ({} bytes, maximum {})",
                encoded_bytes, max_bytes
            ),
            Self::ReadyCapacityExceeded {
                ready_records,
                max_records,
                ready_bytes,
                max_bytes,
            } => write!(
                f,
                "checkpoint outbox capacity exceeded ({} of {} records, {} of {} bytes)",
                ready_records, max_records, ready_bytes, max_bytes
            ),
            Self::AlreadyPublished => {
                write!(f, "checkpoint outbox delivery is already published")
            }
            Self::LockBusy => {
                write!(f, "checkpoint outbox lock root failed (WouldBlock)")
            }
            Self::Io { operation, kind } => {
                write!(f, "checkpoint outbox {} failed ({:?})", operation, kind)
            }
        }
    }
}

impl std::error::Error for CheckpointOutboxError {}

impl From<CheckpointDeliveryError> for CheckpointOutboxError {
    fn from(error: CheckpointDeliveryError) -> Self {
        Self::Delivery(error)
    }
}

pub fn candidate_roots(
    internal_dir: &Path,
    explicit_override: Option<&Path>,
    temp_dir: &Path,
    effective_uid: u32,
) -> Result<Vec<PathBuf>, CheckpointOutboxError> {
    let mut roots = Vec::with_capacity(3);
    if let Some(path) = explicit_override {
        if !path.is_absolute() {
            return Err(CheckpointOutboxError::OverrideMustBeAbsolute(
                path.to_path_buf(),
            ));
        }
        push_unique_root(&mut roots, path.to_path_buf());
    }
    push_unique_root(
        &mut roots,
        internal_dir.join("daemon").join(CHECKPOINT_OUTBOX_VERSION),
    );
    push_unique_root(
        &mut roots,
        temp_dir.join(format!(
            "{}-{}-{}",
            CHECKPOINT_OUTBOX_VERSION,
            effective_uid,
            daemon_instance_key(internal_dir)
        )),
    );
    Ok(roots)
}

fn push_unique_root(roots: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !roots.contains(&candidate) {
        roots.push(candidate);
    }
}

pub fn encode_delivery(delivery: &CheckpointDelivery) -> Result<Vec<u8>, CheckpointOutboxError> {
    encode_delivery_with_limit(delivery, publication::DEFAULT_MAX_ENCODED_RECORD_BYTES)
}

fn encode_delivery_with_limit(
    delivery: &CheckpointDelivery,
    max_encoded_bytes: u64,
) -> Result<Vec<u8>, CheckpointOutboxError> {
    delivery.validate()?;
    let mut writer = CappedDeliveryWriter::new(max_encoded_bytes);
    if let Err(error) = ciborium::into_writer(delivery, &mut writer) {
        if writer.overflowed {
            return Err(CheckpointOutboxError::RecordTooLarge {
                encoded_bytes: max_encoded_bytes.saturating_add(1),
                max_bytes: max_encoded_bytes,
            });
        }
        return Err(CheckpointOutboxError::Encode(error.to_string()));
    }
    Ok(writer.bytes)
}

struct CappedDeliveryWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    overflowed: bool,
}

impl CappedDeliveryWriter {
    fn new(max_bytes: u64) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes: usize::try_from(max_bytes).unwrap_or(usize::MAX),
            overflowed: false,
        }
    }
}

impl Write for CappedDeliveryWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
            self.overflowed = true;
            return Err(std::io::Error::other(
                "checkpoint delivery exceeds encoded size limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn decode_delivery(bytes: &[u8]) -> Result<CheckpointDelivery, CheckpointOutboxError> {
    let delivery: CheckpointDelivery = ciborium::from_reader(bytes)
        .map_err(|error| CheckpointOutboxError::Decode(error.to_string()))?;
    delivery.validate()?;
    Ok(delivery)
}

fn daemon_instance_key(internal_dir: &Path) -> String {
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(internal_dir.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in internal_dir.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(internal_dir.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::checkpoint_delivery::CheckpointDelivery;
    use crate::model::checkpoint_request::{CheckpointRequest, PreparedPathRole};
    use crate::model::working_log::CheckpointKind;
    use std::collections::HashMap;

    fn delivery() -> CheckpointDelivery {
        let request = CheckpointRequest {
            trace_id: "trace-1".to_string(),
            checkpoint_kind: CheckpointKind::Human,
            agent_id: None,
            files: Vec::new(),
            path_role: PreparedPathRole::Edited,
            stream_source: None,
            metadata: HashMap::new(),
        };
        CheckpointDelivery::from_requests_at(vec![request], 42).remove(0)
    }

    #[test]
    fn roots_are_stable_per_daemon_home_and_override_is_first() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let override_root = temp.path().join("managed-outbox");
        let internal_dir = home.join(".git-ai").join("internal");

        let first = candidate_roots(&internal_dir, Some(&override_root), temp.path(), 501).unwrap();
        let second =
            candidate_roots(&internal_dir, Some(&override_root), temp.path(), 501).unwrap();

        assert_eq!(first, second);
        assert_eq!(first[0], override_root);
        assert_eq!(
            first[1],
            internal_dir.join("daemon").join(CHECKPOINT_OUTBOX_VERSION)
        );
        assert!(first[2].starts_with(temp.path()));
    }

    #[test]
    fn different_daemon_homes_use_different_fallback_roots() {
        let temp = tempfile::tempdir().unwrap();
        let first = candidate_roots(
            &temp.path().join("a/.git-ai/internal"),
            None,
            temp.path(),
            501,
        )
        .unwrap();
        let second = candidate_roots(
            &temp.path().join("b/.git-ai/internal"),
            None,
            temp.path(),
            501,
        )
        .unwrap();

        assert_ne!(first[0], second[0]);
        assert_ne!(first[1], second[1]);
    }

    #[test]
    fn override_equal_to_temporary_fallback_is_returned_once_and_first() {
        let temp = tempfile::tempdir().unwrap();
        let internal_dir = temp.path().join("home/.git-ai/internal");
        let fallbacks = candidate_roots(&internal_dir, None, temp.path(), 501).unwrap();
        let temporary_fallback = fallbacks[1].clone();

        let roots =
            candidate_roots(&internal_dir, Some(&temporary_fallback), temp.path(), 501).unwrap();

        assert_eq!(roots, vec![temporary_fallback, fallbacks[0].clone()]);
    }

    #[test]
    fn lock_busy_error_has_stable_display_and_failure_class() {
        let error = CheckpointOutboxError::LockBusy;

        assert_eq!(
            error.to_string(),
            "checkpoint outbox lock root failed (WouldBlock)"
        );
        assert_eq!(
            OutboxFailureClass::from_error(&error),
            OutboxFailureClass::LockBusy
        );
    }

    #[test]
    fn relative_override_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let internal_dir = temp.path().join("home/.git-ai/internal");
        let relative = Path::new("relative/outbox");

        assert!(matches!(
            candidate_roots(&internal_dir, Some(relative), temp.path(), 501),
            Err(CheckpointOutboxError::OverrideMustBeAbsolute(path)) if path == relative
        ));
    }

    #[test]
    fn cbor_decode_rejects_forward_schema() {
        let mut value = delivery();
        value.schema_version += 1;
        let mut bytes = Vec::new();
        ciborium::into_writer(&value, &mut bytes).unwrap();

        assert!(matches!(
            decode_delivery(&bytes),
            Err(CheckpointOutboxError::Delivery(
                CheckpointDeliveryError::UnsupportedSchema { .. }
            ))
        ));
    }

    #[test]
    fn encode_decode_round_trip_keeps_identity() {
        let value = delivery();
        let bytes = encode_delivery(&value).unwrap();
        let decoded = decode_delivery(&bytes).unwrap();

        assert_eq!(decoded.delivery_id, value.delivery_id);
        assert_eq!(decoded.batch_id, value.batch_id);
    }

    #[test]
    fn capped_encoder_stops_without_buffering_past_the_limit() {
        let mut value = delivery();
        value
            .request
            .metadata
            .insert("bounded".to_string(), "x".repeat(4_096));

        assert!(matches!(
            encode_delivery_with_limit(&value, 128),
            Err(CheckpointOutboxError::RecordTooLarge {
                encoded_bytes: 129,
                max_bytes: 128
            })
        ));

        let mut writer = CappedDeliveryWriter::new(4);
        assert_eq!(writer.write(b"1234").unwrap(), 4);
        assert!(writer.write(b"5").is_err());
        assert_eq!(writer.bytes.len(), 4);
    }

    #[test]
    fn daemon_instance_key_is_stable_and_path_specific() {
        assert_eq!(
            daemon_instance_key(Path::new("/one")),
            daemon_instance_key(Path::new("/one"))
        );
        assert_ne!(
            daemon_instance_key(Path::new("/one")),
            daemon_instance_key(Path::new("/two"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn daemon_instance_key_distinguishes_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let first = PathBuf::from(OsString::from_vec(b"/daemon-\x80".to_vec()));
        let second = PathBuf::from(OsString::from_vec(b"/daemon-\x81".to_vec()));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());

        assert_ne!(
            daemon_instance_key(&first),
            daemon_instance_key(&second),
            "distinct native paths must never share a fallback outbox"
        );
    }
}
