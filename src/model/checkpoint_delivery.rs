use crate::model::checkpoint_request::CheckpointRequest;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const CHECKPOINT_DELIVERY_SCHEMA_VERSION: u16 = 1;
pub const CHECKPOINT_DELIVERY_MAX_FILES: usize = 1_000;
pub const CHECKPOINT_DELIVERY_MAX_PATH_BYTES: usize = 16 * 1024;
pub const CHECKPOINT_DELIVERY_MAX_METADATA_ENTRIES: usize = 256;
pub const CHECKPOINT_DELIVERY_MAX_METADATA_KEY_BYTES: usize = 1024;
pub const CHECKPOINT_DELIVERY_MAX_METADATA_VALUE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointDelivery {
    pub schema_version: u16,
    pub delivery_id: String,
    pub batch_id: String,
    pub batch_ordinal: u32,
    pub captured_at_unix_ms: u64,
    pub producer_version: String,
    pub request: CheckpointRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointDeliveryError {
    UnsupportedSchema {
        found: u16,
        supported: u16,
    },
    EmptyIdentifier {
        field: &'static str,
    },
    UnsafeIdentifier {
        field: &'static str,
    },
    PathMustBeAbsolute {
        field: &'static str,
    },
    NonUtf8Path {
        field: &'static str,
    },
    LimitExceeded {
        field: &'static str,
        limit: usize,
        actual: usize,
    },
}

impl fmt::Display for CheckpointDeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema { found, supported } => write!(
                f,
                "checkpoint delivery schema {} is newer than supported schema {}",
                found, supported
            ),
            Self::EmptyIdentifier { field } => {
                write!(f, "checkpoint delivery {} must not be empty", field)
            }
            Self::UnsafeIdentifier { field } => {
                write!(
                    f,
                    "checkpoint delivery {} contains unsafe characters",
                    field
                )
            }
            Self::PathMustBeAbsolute { field } => {
                write!(f, "checkpoint delivery {} must be absolute", field)
            }
            Self::NonUtf8Path { field } => {
                write!(
                    f,
                    "checkpoint delivery {} uses an unsupported path encoding",
                    field
                )
            }
            Self::LimitExceeded {
                field,
                limit,
                actual,
            } => write!(
                f,
                "checkpoint delivery {} exceeds limit {} (actual {})",
                field, limit, actual
            ),
        }
    }
}

impl std::error::Error for CheckpointDeliveryError {}

impl CheckpointDelivery {
    pub fn from_requests(requests: Vec<CheckpointRequest>) -> Vec<Self> {
        let captured_at_unix_ms =
            u64::try_from(crate::model::clock::now_millis()).unwrap_or(u64::MAX);
        Self::from_requests_at(requests, captured_at_unix_ms)
    }

    pub fn from_requests_at(
        requests: Vec<CheckpointRequest>,
        captured_at_unix_ms: u64,
    ) -> Vec<Self> {
        let batch_id = crate::uuid::generate_v4();
        requests
            .into_iter()
            .enumerate()
            .map(|(batch_ordinal, request)| Self {
                schema_version: CHECKPOINT_DELIVERY_SCHEMA_VERSION,
                delivery_id: crate::uuid::generate_v4(),
                batch_id: batch_id.clone(),
                batch_ordinal: u32::try_from(batch_ordinal).unwrap_or(u32::MAX),
                captured_at_unix_ms,
                producer_version: env!("CARGO_PKG_VERSION").to_string(),
                request,
            })
            .collect()
    }

    pub fn validate(&self) -> Result<(), CheckpointDeliveryError> {
        if self.schema_version != CHECKPOINT_DELIVERY_SCHEMA_VERSION {
            return Err(CheckpointDeliveryError::UnsupportedSchema {
                found: self.schema_version,
                supported: CHECKPOINT_DELIVERY_SCHEMA_VERSION,
            });
        }
        validate_identifier("delivery_id", &self.delivery_id)?;
        validate_identifier("batch_id", &self.batch_id)?;
        if self.producer_version.is_empty() {
            return Err(CheckpointDeliveryError::EmptyIdentifier {
                field: "producer_version",
            });
        }
        validate_checkpoint_request(&self.request, true)
    }

    pub fn captured_at_unix_ns(&self) -> u128 {
        u128::from(self.captured_at_unix_ms)
            .checked_mul(1_000_000)
            .expect("u64 milliseconds always fit in u128 nanoseconds")
    }
}

pub(crate) fn validate_checkpoint_request_bounds(
    request: &CheckpointRequest,
) -> Result<(), CheckpointDeliveryError> {
    validate_checkpoint_request(request, false)
}

fn validate_checkpoint_request(
    request: &CheckpointRequest,
    require_absolute_paths: bool,
) -> Result<(), CheckpointDeliveryError> {
    validate_limit(
        "request.files",
        request.files.len(),
        CHECKPOINT_DELIVERY_MAX_FILES,
    )?;
    for file in &request.files {
        validate_path("file.path", &file.path, require_absolute_paths)?;
        validate_path(
            "file.repo_work_dir",
            &file.repo_work_dir,
            require_absolute_paths,
        )?;
    }
    if let Some(stream_source) = &request.stream_source {
        validate_path(
            "stream_source.path",
            &stream_source.path,
            require_absolute_paths,
        )?;
    }
    validate_limit(
        "request.metadata",
        request.metadata.len(),
        CHECKPOINT_DELIVERY_MAX_METADATA_ENTRIES,
    )?;
    let max_key_bytes = request
        .metadata
        .keys()
        .map(|key| key.len())
        .max()
        .unwrap_or(0);
    validate_limit(
        "metadata.key",
        max_key_bytes,
        CHECKPOINT_DELIVERY_MAX_METADATA_KEY_BYTES,
    )?;
    let max_value_bytes = request
        .metadata
        .values()
        .map(|value| value.len())
        .max()
        .unwrap_or(0);
    validate_limit(
        "metadata.value",
        max_value_bytes,
        CHECKPOINT_DELIVERY_MAX_METADATA_VALUE_BYTES,
    )?;
    Ok(())
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), CheckpointDeliveryError> {
    if value.is_empty() {
        return Err(CheckpointDeliveryError::EmptyIdentifier { field });
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(CheckpointDeliveryError::UnsafeIdentifier { field });
    }
    Ok(())
}

fn validate_path(
    field: &'static str,
    path: &std::path::Path,
    require_absolute: bool,
) -> Result<(), CheckpointDeliveryError> {
    if require_absolute && !path.is_absolute() {
        return Err(CheckpointDeliveryError::PathMustBeAbsolute { field });
    }
    if path.to_str().is_none() {
        return Err(CheckpointDeliveryError::NonUtf8Path { field });
    }
    validate_limit(
        field,
        path.as_os_str().len(),
        CHECKPOINT_DELIVERY_MAX_PATH_BYTES,
    )
}

fn validate_limit(
    field: &'static str,
    actual: usize,
    limit: usize,
) -> Result<(), CheckpointDeliveryError> {
    if actual > limit {
        return Err(CheckpointDeliveryError::LimitExceeded {
            field,
            limit,
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::checkpoint_request::{
        BaseCommit, CheckpointFile, CheckpointRequest, PreparedPathRole, StreamFormat, StreamSource,
    };
    use crate::model::working_log::CheckpointKind;
    use std::collections::HashMap;
    use std::path::PathBuf;

    const TEST_MAX_PATH_BYTES: usize = 16 * 1024;
    const TEST_MAX_METADATA_ENTRIES: usize = 256;
    const TEST_MAX_METADATA_KEY_BYTES: usize = 1024;
    const TEST_MAX_METADATA_VALUE_BYTES: usize = 1024 * 1024;

    fn request(trace_id: &str) -> CheckpointRequest {
        CheckpointRequest {
            trace_id: trace_id.to_string(),
            checkpoint_kind: CheckpointKind::Human,
            agent_id: None,
            files: Vec::new(),
            path_role: PreparedPathRole::Edited,
            stream_source: None,
            metadata: HashMap::new(),
        }
    }

    fn delivery(request: CheckpointRequest) -> CheckpointDelivery {
        CheckpointDelivery::from_requests_at(vec![request], 1).remove(0)
    }

    fn absolute_path_with_len(len: usize) -> PathBuf {
        #[cfg(windows)]
        let prefix = r"C:\";
        #[cfg(not(windows))]
        let prefix = "/";

        assert!(len >= prefix.len());
        PathBuf::from(format!("{prefix}{}", "a".repeat(len - prefix.len())))
    }

    #[test]
    #[cfg(unix)]
    fn non_utf8_paths_fail_closed_before_wire_encoding() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut value = delivery(request("trace"));
        value.request.files.push(checkpoint_file(
            PathBuf::from(OsString::from_vec(b"/repo/file-\x80".to_vec())),
            PathBuf::from("/repo"),
        ));

        assert!(matches!(
            value.validate(),
            Err(CheckpointDeliveryError::NonUtf8Path { field: "file.path" })
        ));
    }

    fn checkpoint_file(path: PathBuf, repo_work_dir: PathBuf) -> CheckpointFile {
        CheckpointFile {
            path,
            content: None,
            repo_work_dir,
            base_commit: BaseCommit::Initial,
        }
    }

    fn stream_source(path: PathBuf) -> StreamSource {
        StreamSource {
            path,
            format: StreamFormat::ClaudeJsonl,
            session_id: "session".to_string(),
            external_session_id: "external".to_string(),
            external_parent_session_id: None,
        }
    }

    #[test]
    fn batch_assigns_stable_unique_delivery_identity() {
        let deliveries = CheckpointDelivery::from_requests_at(
            vec![request("a"), request("b")],
            1_725_000_123_456,
        );

        assert_eq!(deliveries.len(), 2);
        assert_eq!(
            deliveries[0].schema_version,
            CHECKPOINT_DELIVERY_SCHEMA_VERSION
        );
        assert_eq!(deliveries[0].batch_id, deliveries[1].batch_id);
        assert_ne!(deliveries[0].delivery_id, deliveries[1].delivery_id);
        assert_eq!(deliveries[0].batch_ordinal, 0);
        assert_eq!(deliveries[1].batch_ordinal, 1);
        assert_eq!(deliveries[0].captured_at_unix_ms, 1_725_000_123_456);
        assert!(!deliveries[0].producer_version.is_empty());
    }

    #[test]
    fn cbor_round_trip_preserves_delivery_id_and_request() {
        let delivery =
            CheckpointDelivery::from_requests_at(vec![request("trace-1")], 1_725_000_123_456)
                .remove(0);
        let mut encoded = Vec::new();
        ciborium::into_writer(&delivery, &mut encoded).unwrap();
        let decoded: CheckpointDelivery = ciborium::from_reader(encoded.as_slice()).unwrap();

        decoded.validate().unwrap();
        assert_eq!(decoded.delivery_id, delivery.delivery_id);
        assert_eq!(decoded.request.trace_id, "trace-1");
    }

    #[test]
    fn forward_schema_is_rejected() {
        let mut delivery =
            CheckpointDelivery::from_requests_at(vec![request("trace-1")], 1).remove(0);
        delivery.schema_version = CHECKPOINT_DELIVERY_SCHEMA_VERSION + 1;

        assert_eq!(
            delivery.validate(),
            Err(CheckpointDeliveryError::UnsupportedSchema {
                found: CHECKPOINT_DELIVERY_SCHEMA_VERSION + 1,
                supported: CHECKPOINT_DELIVERY_SCHEMA_VERSION,
            })
        );
    }

    #[test]
    fn capture_milliseconds_convert_to_nanoseconds_explicitly() {
        let delivery =
            CheckpointDelivery::from_requests_at(vec![request("trace-1")], 1_725_000_123_456)
                .remove(0);

        assert_eq!(delivery.captured_at_unix_ns(), 1_725_000_123_456_000_000);
    }

    #[test]
    fn unsafe_identifier_is_rejected_before_it_can_become_a_filename() {
        let mut delivery =
            CheckpointDelivery::from_requests_at(vec![request("trace-1")], 1).remove(0);
        delivery.delivery_id = "../escape".to_string();

        assert_eq!(
            delivery.validate(),
            Err(CheckpointDeliveryError::UnsafeIdentifier {
                field: "delivery_id",
            })
        );
    }

    #[test]
    fn relative_snapshot_paths_are_rejected() {
        let mut value = request("trace-1");
        value.files.push(CheckpointFile {
            path: PathBuf::from("relative.rs"),
            content: Some("content".to_string()),
            repo_work_dir: PathBuf::from("/repo"),
            base_commit: BaseCommit::Initial,
        });
        let delivery = CheckpointDelivery::from_requests_at(vec![value], 1).remove(0);

        assert_eq!(
            delivery.validate(),
            Err(CheckpointDeliveryError::PathMustBeAbsolute { field: "file.path" })
        );
    }

    #[test]
    fn request_bounds_preserve_legacy_relative_snapshot_paths() {
        let mut value = request("trace-1");
        value.files.push(CheckpointFile {
            path: PathBuf::from("relative.rs"),
            content: Some("content".to_string()),
            repo_work_dir: PathBuf::from("/repo"),
            base_commit: BaseCommit::Initial,
        });

        validate_checkpoint_request_bounds(&value).unwrap();
        assert_eq!(
            delivery(value).validate(),
            Err(CheckpointDeliveryError::PathMustBeAbsolute { field: "file.path" })
        );
    }

    #[test]
    fn file_count_is_accepted_at_limit_and_rejected_above_it() {
        let valid_file = checkpoint_file(absolute_path_with_len(8), absolute_path_with_len(8));
        let mut at_limit = request("trace-1");
        at_limit.files = vec![valid_file.clone(); CHECKPOINT_DELIVERY_MAX_FILES];
        delivery(at_limit).validate().unwrap();

        let mut above_limit = request("trace-1");
        above_limit.files = vec![valid_file; CHECKPOINT_DELIVERY_MAX_FILES + 1];
        let request_error = validate_checkpoint_request_bounds(&above_limit).unwrap_err();
        let error = delivery(above_limit).validate().unwrap_err();

        assert_eq!(
            error.to_string(),
            "checkpoint delivery request.files exceeds limit 1000 (actual 1001)"
        );
        assert_eq!(request_error, error);
        assert_eq!(
            format!("{error:?}"),
            "LimitExceeded { field: \"request.files\", limit: 1000, actual: 1001 }"
        );
    }

    #[test]
    fn each_path_representation_is_accepted_at_limit() {
        let max_path = absolute_path_with_len(TEST_MAX_PATH_BYTES);
        let mut value = request("trace-1");
        value
            .files
            .push(checkpoint_file(max_path.clone(), max_path.clone()));
        value.stream_source = Some(stream_source(max_path));

        delivery(value).validate().unwrap();
    }

    #[test]
    fn file_path_representation_is_rejected_above_limit() {
        let mut value = request("trace-1");
        value.files.push(checkpoint_file(
            absolute_path_with_len(TEST_MAX_PATH_BYTES + 1),
            absolute_path_with_len(8),
        ));

        let error = delivery(value).validate().unwrap_err();
        assert_eq!(
            error.to_string(),
            "checkpoint delivery file.path exceeds limit 16384 (actual 16385)"
        );
    }

    #[test]
    fn repository_work_dir_representation_is_rejected_above_limit() {
        let mut value = request("trace-1");
        value.files.push(checkpoint_file(
            absolute_path_with_len(8),
            absolute_path_with_len(TEST_MAX_PATH_BYTES + 1),
        ));

        let error = delivery(value).validate().unwrap_err();
        assert_eq!(
            error.to_string(),
            "checkpoint delivery file.repo_work_dir exceeds limit 16384 (actual 16385)"
        );
    }

    #[test]
    fn stream_path_representation_is_rejected_above_limit() {
        let mut value = request("trace-1");
        value.stream_source = Some(stream_source(absolute_path_with_len(
            TEST_MAX_PATH_BYTES + 1,
        )));

        let error = delivery(value).validate().unwrap_err();
        assert_eq!(
            error.to_string(),
            "checkpoint delivery stream_source.path exceeds limit 16384 (actual 16385)"
        );
    }

    #[test]
    fn metadata_count_is_accepted_at_limit_and_rejected_above_it() {
        let mut at_limit = request("trace-1");
        at_limit.metadata = (0..TEST_MAX_METADATA_ENTRIES)
            .map(|index| (format!("key-{index}"), "value".to_string()))
            .collect();
        delivery(at_limit).validate().unwrap();

        let mut above_limit = request("trace-1");
        above_limit.metadata = (0..=TEST_MAX_METADATA_ENTRIES)
            .map(|index| (format!("key-{index}"), "value".to_string()))
            .collect();
        let error = delivery(above_limit).validate().unwrap_err();

        assert_eq!(
            error.to_string(),
            "checkpoint delivery request.metadata exceeds limit 256 (actual 257)"
        );
    }

    #[test]
    fn metadata_key_is_accepted_at_limit_and_rejected_above_it() {
        let mut at_limit = request("trace-1");
        at_limit
            .metadata
            .insert("k".repeat(TEST_MAX_METADATA_KEY_BYTES), "value".to_string());
        delivery(at_limit).validate().unwrap();

        let mut above_limit = request("trace-1");
        above_limit.metadata.insert(
            "k".repeat(TEST_MAX_METADATA_KEY_BYTES + 1),
            "value".to_string(),
        );
        let error = delivery(above_limit).validate().unwrap_err();

        assert_eq!(
            error.to_string(),
            "checkpoint delivery metadata.key exceeds limit 1024 (actual 1025)"
        );
    }

    #[test]
    fn metadata_value_is_accepted_at_limit_and_rejected_above_it() {
        let mut at_limit = request("trace-1");
        at_limit
            .metadata
            .insert("key".to_string(), "v".repeat(TEST_MAX_METADATA_VALUE_BYTES));
        delivery(at_limit).validate().unwrap();

        let mut above_limit = request("trace-1");
        above_limit.metadata.insert(
            "key".to_string(),
            "v".repeat(TEST_MAX_METADATA_VALUE_BYTES + 1),
        );
        let error = delivery(above_limit).validate().unwrap_err();

        assert_eq!(
            error.to_string(),
            "checkpoint delivery metadata.value exceeds limit 1048576 (actual 1048577)"
        );
    }

    #[test]
    fn limit_errors_do_not_expose_sensitive_values() {
        let sensitive = "DO-NOT-LOG-checkpoint-metadata";
        let mut value = request("trace-1");
        value.metadata.insert(
            "key".to_string(),
            format!("{sensitive}{}", "v".repeat(TEST_MAX_METADATA_VALUE_BYTES)),
        );

        let error = delivery(value).validate().unwrap_err();
        let display = error.to_string();
        let debug = format!("{error:?}");

        assert!(!display.contains(sensitive));
        assert!(!debug.contains(sensitive));
        assert_eq!(
            display,
            "checkpoint delivery metadata.value exceeds limit 1048576 (actual 1048606)"
        );
    }
}
