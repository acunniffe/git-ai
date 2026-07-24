use crate::model::checkpoint_delivery::{CheckpointDelivery, CheckpointDeliveryError};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};

pub const CHECKPOINT_OUTBOX_VERSION: &str = "checkpoint-outbox-v1";

#[derive(Debug)]
pub enum CheckpointOutboxError {
    Delivery(CheckpointDeliveryError),
    Encode(String),
    Decode(String),
    OverrideMustBeAbsolute(PathBuf),
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
        roots.push(path.to_path_buf());
    }
    roots.push(internal_dir.join("daemon").join(CHECKPOINT_OUTBOX_VERSION));
    roots.push(temp_dir.join(format!(
        "{}-{}-{}",
        CHECKPOINT_OUTBOX_VERSION,
        effective_uid,
        daemon_instance_key(internal_dir)
    )));
    roots.dedup();
    Ok(roots)
}

pub fn encode_delivery(delivery: &CheckpointDelivery) -> Result<Vec<u8>, CheckpointOutboxError> {
    delivery.validate()?;
    let mut bytes = Vec::new();
    ciborium::into_writer(delivery, &mut bytes)
        .map_err(|error| CheckpointOutboxError::Encode(error.to_string()))?;
    Ok(bytes)
}

pub fn decode_delivery(bytes: &[u8]) -> Result<CheckpointDelivery, CheckpointOutboxError> {
    let delivery: CheckpointDelivery = ciborium::from_reader(bytes)
        .map_err(|error| CheckpointOutboxError::Decode(error.to_string()))?;
    delivery.validate()?;
    Ok(delivery)
}

fn daemon_instance_key(internal_dir: &Path) -> String {
    let mut hasher = Sha256::new();
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
}
