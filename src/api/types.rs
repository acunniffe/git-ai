use crate::authorship::authorship_log::{LineRange, PromptRecord};
use crate::commands::diff::FileDiffJson;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// File record for API - converts LineRange annotations to API format
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiFileRecord {
    /// Maps prompt_hash to line numbers/ranges
    /// Example: { "prompt_abc123": [[1, 5], 10] } means lines 1-5 and line 10 attributed to prompt_abc123
    pub annotations: HashMap<String, Vec<serde_json::Value>>,
    /// Git diff output
    pub diff: String,
    /// Original file content before changes
    #[serde(rename = "base_content")]
    pub base_content: String,
}

impl From<&FileDiffJson> for ApiFileRecord {
    fn from(file_diff: &FileDiffJson) -> Self {
        let annotations: HashMap<String, Vec<serde_json::Value>> = file_diff
            .annotations
            .iter()
            .map(|(key, ranges)| {
                let json_ranges: Vec<serde_json::Value> = ranges
                    .iter()
                    .map(|range| match range {
                        LineRange::Single(line) => serde_json::Value::Number((*line as u64).into()),
                        LineRange::Range(start, end) => serde_json::Value::Array(vec![
                            serde_json::Value::Number((*start as u64).into()),
                            serde_json::Value::Number((*end as u64).into()),
                        ]),
                    })
                    .collect();
                (key.clone(), json_ranges)
            })
            .collect();

        Self {
            annotations,
            diff: file_diff.diff.clone(),
            base_content: file_diff.base_content.clone(),
        }
    }
}

/// Bundle data containing prompts and optional files
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BundleData {
    /// REQUIRED: At least one prompt
    pub prompts: HashMap<String, PromptRecord>,
    /// OPTIONAL: File diffs and annotations
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub files: HashMap<String, ApiFileRecord>,
}

/// Request body for creating a bundle
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateBundleRequest {
    /// Bundle title (min 1 character)
    pub title: String,
    /// Bundle data containing prompts and optional files
    pub data: BundleData,
    // TODO PR Metadata if linked to PR
}

/// Success response from bundle creation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateBundleResponse {
    pub success: bool,
    pub id: String,
    pub url: String,
}

/// Error response from API
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiErrorResponse {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Single CAS object for upload
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CasObject {
    pub content: serde_json::Value,
    pub hash: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

/// Request body for CAS upload
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CasUploadRequest {
    pub objects: Vec<CasObject>,
}

/// Result for a single CAS object upload
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CasUploadResult {
    pub hash: String,
    pub status: String, // "ok" or "error"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response from CAS upload
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CasUploadResponse {
    pub results: Vec<CasUploadResult>,
    pub success_count: usize,
    pub failure_count: usize,
}

/// Wrapper for messages stored in CAS
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CasMessagesObject {
    pub messages: Vec<crate::authorship::transcript::Message>,
}

/// A single authorship note entry (commit SHA + content).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NoteEntry {
    pub commit_sha: String,
    pub content: String,
}

/// Request body for uploading notes to the HTTP backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NotesUploadRequest {
    pub entries: Vec<NoteEntry>,
}

/// Response from a notes upload request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotesUploadResponse {
    pub success_count: usize,
    pub failure_count: usize,
}

/// Response from a notes read request — maps commit_sha → note content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NotesReadResponse {
    pub notes: std::collections::HashMap<String, String>,
}

/// Single result from CA prompt store batch read
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CAPromptStoreReadResult {
    pub hash: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response from CA prompt store batch read
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CAPromptStoreReadResponse {
    pub results: Vec<CAPromptStoreReadResult>,
    pub success_count: usize,
    pub failure_count: usize,
}

/// Daemon diagnostics upload protocol version.
pub const DAEMON_LOGS_UPLOAD_VERSION: u8 = 1;

/// Kind of daemon diagnostic event accepted by `/worker/logs/upload`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonLogKind {
    Log,
    Heartbeat,
}

/// Log level accepted by `/worker/logs/upload`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DaemonLogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Primitive daemon log field value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum DaemonLogFieldValue {
    String(String),
    Number(serde_json::Number),
    Bool(bool),
    Null,
}

impl From<String> for DaemonLogFieldValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for DaemonLogFieldValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<u64> for DaemonLogFieldValue {
    fn from(value: u64) -> Self {
        Self::Number(serde_json::Number::from(value))
    }
}

impl From<i64> for DaemonLogFieldValue {
    fn from(value: i64) -> Self {
        Self::Number(serde_json::Number::from(value))
    }
}

impl From<bool> for DaemonLogFieldValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl DaemonLogFieldValue {
    /// Create a numeric field from an `f64`.
    ///
    /// Returns `Null` for non-finite values (NaN / Infinity) since those
    /// are not representable in JSON.
    pub fn from_f64(value: f64) -> Self {
        serde_json::Number::from_f64(value)
            .map(Self::Number)
            .unwrap_or(Self::Null)
    }
}

/// Single daemon diagnostic event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonLogEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub kind: DaemonLogKind,
    pub timestamp: String,
    pub level: DaemonLogLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, DaemonLogFieldValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ai_version: Option<String>,
}

/// Request body for uploading daemon diagnostics to the HTTP backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonLogsUploadRequest {
    pub version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ai_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    pub events: Vec<DaemonLogEvent>,
}

/// Error entry returned from daemon log upload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonLogsUploadError {
    pub index: Option<usize>,
    pub error: String,
}

/// Response from daemon log upload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonLogsUploadResponse {
    pub accepted: usize,
    pub dropped: usize,
    pub enqueued: bool,
    #[serde(default)]
    pub errors: Vec<DaemonLogsUploadError>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::authorship_log::LineRange;
    use crate::commands::diff::FileDiffJson;
    use std::collections::BTreeMap;

    #[test]
    fn test_api_file_record_from_file_diff_empty() {
        let file_diff = FileDiffJson {
            annotations: BTreeMap::new(),
            diff: "".to_string(),
            base_content: "".to_string(),
        };

        let api_record = ApiFileRecord::from(&file_diff);
        assert_eq!(api_record.annotations.len(), 0);
        assert_eq!(api_record.diff, "");
        assert_eq!(api_record.base_content, "");
    }

    #[test]
    fn test_api_file_record_from_file_diff_single_lines() {
        let mut annotations = BTreeMap::new();
        annotations.insert(
            "prompt_hash_1".to_string(),
            vec![LineRange::Single(5), LineRange::Single(10)],
        );

        let file_diff = FileDiffJson {
            annotations,
            diff: "diff content".to_string(),
            base_content: "base content".to_string(),
        };

        let api_record = ApiFileRecord::from(&file_diff);
        assert_eq!(api_record.annotations.len(), 1);

        let ranges = &api_record.annotations["prompt_hash_1"];
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], serde_json::Value::Number(5.into()));
        assert_eq!(ranges[1], serde_json::Value::Number(10.into()));
        assert_eq!(api_record.diff, "diff content");
        assert_eq!(api_record.base_content, "base content");
    }

    #[test]
    fn test_api_file_record_from_file_diff_ranges() {
        let mut annotations = BTreeMap::new();
        annotations.insert(
            "prompt_hash_2".to_string(),
            vec![LineRange::Range(1, 5), LineRange::Range(10, 15)],
        );

        let file_diff = FileDiffJson {
            annotations,
            diff: "diff".to_string(),
            base_content: "base".to_string(),
        };

        let api_record = ApiFileRecord::from(&file_diff);
        let ranges = &api_record.annotations["prompt_hash_2"];
        assert_eq!(ranges.len(), 2);

        match &ranges[0] {
            serde_json::Value::Array(arr) => {
                assert_eq!(arr.len(), 2);
                assert_eq!(arr[0], serde_json::Value::Number(1.into()));
                assert_eq!(arr[1], serde_json::Value::Number(5.into()));
            }
            _ => panic!("Expected array"),
        }

        match &ranges[1] {
            serde_json::Value::Array(arr) => {
                assert_eq!(arr.len(), 2);
                assert_eq!(arr[0], serde_json::Value::Number(10.into()));
                assert_eq!(arr[1], serde_json::Value::Number(15.into()));
            }
            _ => panic!("Expected array"),
        }
    }

    #[test]
    fn test_api_file_record_from_file_diff_mixed() {
        let mut annotations = BTreeMap::new();
        annotations.insert(
            "prompt_hash".to_string(),
            vec![
                LineRange::Single(1),
                LineRange::Range(5, 10),
                LineRange::Single(20),
            ],
        );

        let file_diff = FileDiffJson {
            annotations,
            diff: String::new(),
            base_content: String::new(),
        };

        let api_record = ApiFileRecord::from(&file_diff);
        let ranges = &api_record.annotations["prompt_hash"];
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[0], serde_json::Value::Number(1.into()));

        match &ranges[1] {
            serde_json::Value::Array(arr) => {
                assert_eq!(arr[0], serde_json::Value::Number(5.into()));
                assert_eq!(arr[1], serde_json::Value::Number(10.into()));
            }
            _ => panic!("Expected array"),
        }

        assert_eq!(ranges[2], serde_json::Value::Number(20.into()));
    }

    #[test]
    fn test_create_bundle_response_deserialization() {
        let json = r#"{
            "success": true,
            "id": "bundle123",
            "url": "https://example.com/bundle123"
        }"#;

        let response: CreateBundleResponse = serde_json::from_str(json).unwrap();
        assert!(response.success);
        assert_eq!(response.id, "bundle123");
        assert_eq!(response.url, "https://example.com/bundle123");
    }

    #[test]
    fn test_api_error_response_serialization() {
        let error = ApiErrorResponse {
            error: "Invalid request".to_string(),
            details: Some(serde_json::json!({"field": "title"})),
        };

        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains("Invalid request"));
        assert!(json.contains("field"));
    }

    #[test]
    fn test_api_error_response_without_details() {
        let error = ApiErrorResponse {
            error: "Error".to_string(),
            details: None,
        };

        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains("Error"));
        assert!(!json.contains("details"));
    }

    #[test]
    fn test_cas_object_serialization() {
        let mut metadata = HashMap::new();
        metadata.insert("key1".to_string(), "value1".to_string());

        let cas_object = CasObject {
            content: serde_json::json!({"data": "test"}),
            hash: "abc123".to_string(),
            metadata,
        };

        let json = serde_json::to_string(&cas_object).unwrap();
        assert!(json.contains("abc123"));
        assert!(json.contains("key1"));
    }

    #[test]
    fn test_cas_object_empty_metadata() {
        let cas_object = CasObject {
            content: serde_json::json!({}),
            hash: "hash".to_string(),
            metadata: HashMap::new(),
        };

        let json = serde_json::to_string(&cas_object).unwrap();
        assert!(!json.contains("metadata"));
    }

    #[test]
    fn test_cas_upload_request() {
        let objects = vec![
            CasObject {
                content: serde_json::json!({"test": 1}),
                hash: "h1".to_string(),
                metadata: HashMap::new(),
            },
            CasObject {
                content: serde_json::json!({"test": 2}),
                hash: "h2".to_string(),
                metadata: HashMap::new(),
            },
        ];

        let request = CasUploadRequest { objects };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("h1"));
        assert!(json.contains("h2"));
    }

    #[test]
    fn test_cas_upload_result() {
        let result = CasUploadResult {
            hash: "hash1".to_string(),
            status: "ok".to_string(),
            error: None,
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("ok"));
        assert!(!json.contains("error"));
    }

    #[test]
    fn test_cas_upload_result_with_error() {
        let result = CasUploadResult {
            hash: "hash2".to_string(),
            status: "error".to_string(),
            error: Some("Upload failed".to_string()),
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("error"));
        assert!(json.contains("Upload failed"));
    }

    #[test]
    fn test_cas_upload_response() {
        let response = CasUploadResponse {
            results: vec![
                CasUploadResult {
                    hash: "h1".to_string(),
                    status: "ok".to_string(),
                    error: None,
                },
                CasUploadResult {
                    hash: "h2".to_string(),
                    status: "error".to_string(),
                    error: Some("Failed".to_string()),
                },
            ],
            success_count: 1,
            failure_count: 1,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("success_count"));
        assert!(json.contains("failure_count"));
    }

    #[test]
    fn test_api_file_record_clone() {
        let record = ApiFileRecord {
            annotations: HashMap::new(),
            diff: "test".to_string(),
            base_content: "base".to_string(),
        };

        let cloned = record.clone();
        assert_eq!(record, cloned);
    }

    #[test]
    fn test_cas_messages_object() {
        use crate::authorship::transcript::Message;

        let messages = vec![Message::user("test".to_string(), None)];

        let cas_msg = CasMessagesObject {
            messages: messages.clone(),
        };

        let json = serde_json::to_string(&cas_msg).unwrap();
        assert!(json.contains("test"));

        let deserialized: CasMessagesObject = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.messages.len(), 1);
    }

    #[test]
    fn daemon_logs_upload_request_serializes_contract_fields() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "uptime_seconds".to_string(),
            DaemonLogFieldValue::from(900_u64),
        );
        fields.insert("healthy".to_string(), DaemonLogFieldValue::from(true));

        let request = DaemonLogsUploadRequest {
            version: DAEMON_LOGS_UPLOAD_VERSION,
            git_ai_version: Some("1.2.3".to_string()),
            daemon_id: Some("daemon-1".to_string()),
            install_id: Some("install-1".to_string()),
            repo_url: None,
            events: vec![DaemonLogEvent {
                id: Some("event-1".to_string()),
                kind: DaemonLogKind::Heartbeat,
                timestamp: "2026-06-26T12:00:00.000Z".to_string(),
                level: DaemonLogLevel::Info,
                target: Some("git_ai::daemon".to_string()),
                message: "alive".to_string(),
                fields,
                repo_url: None,
                git_ai_version: None,
            }],
        };

        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["git_ai_version"], "1.2.3");
        assert_eq!(value["daemon_id"], "daemon-1");
        assert_eq!(value["events"][0]["kind"], "heartbeat");
        assert_eq!(value["events"][0]["level"], "info");
        assert_eq!(value["events"][0]["fields"]["uptime_seconds"], 900);
        assert_eq!(value["events"][0]["fields"]["healthy"], true);
    }

    #[test]
    fn daemon_log_field_value_from_f64_finite() {
        let val = DaemonLogFieldValue::from_f64(12.5);
        assert_eq!(
            val,
            DaemonLogFieldValue::Number(serde_json::Number::from_f64(12.5).unwrap())
        );
    }

    #[test]
    fn daemon_log_field_value_from_f64_nan_is_null() {
        let val = DaemonLogFieldValue::from_f64(f64::NAN);
        assert_eq!(val, DaemonLogFieldValue::Null);
    }

    #[test]
    fn daemon_log_field_value_from_f64_infinity_is_null() {
        let val = DaemonLogFieldValue::from_f64(f64::INFINITY);
        assert_eq!(val, DaemonLogFieldValue::Null);
    }

    #[test]
    fn daemon_logs_upload_response_accepts_null_error_index() {
        let response: DaemonLogsUploadResponse = serde_json::from_str(
            r#"{"accepted":0,"dropped":0,"enqueued":false,"errors":[{"index":null,"error":"bad"}]}"#,
        )
        .unwrap();

        assert_eq!(response.errors[0].index, None);
    }
}
