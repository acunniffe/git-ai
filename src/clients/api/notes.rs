//! Notes API endpoints for the HTTP notes backend.
//!
//! Authentication is handled automatically by `ApiContext`: the existing
//! `X-API-Key` / Bearer token headers are attached on every request.
//! The daemon flusher should skip uploads when neither `is_logged_in()` nor
//! `has_api_key()` is true (matching the CAS pattern).

use crate::clients::api::client::ApiClient;
use crate::clients::api::first_non_hex;
use crate::clients::api::response::ResponseEnvelope;
use crate::error::GitAiError;
use crate::model::api_types::{NotesReadResponse, NotesUploadRequest, NotesUploadResponse};

impl ApiClient {
    /// Upload a batch of authorship notes to the remote backend.
    ///
    /// # Arguments
    /// * `request` - The notes upload request containing entries to upload
    ///
    /// # Returns
    /// * `Ok(NotesUploadResponse)` - Success response with counts
    /// * `Err(GitAiError)` - On network or server errors
    pub fn upload_notes(
        &self,
        request: NotesUploadRequest,
    ) -> Result<NotesUploadResponse, GitAiError> {
        let response = self.context().post_json("/worker/notes/upload", &request)?;
        ResponseEnvelope::read(&response)?.decode_json(200, "notes upload", "unexpected error")
    }

    /// Read authorship notes by commit SHAs. Max 100 per call.
    ///
    /// Returns an empty map for any SHAs not found (404 is treated as success).
    ///
    /// # Arguments
    /// * `commit_shas` - Slice of hex commit SHAs to fetch
    ///
    /// # Returns
    /// * `Ok(NotesReadResponse)` - Response mapping commit_sha → note content
    /// * `Err(GitAiError)` - On invalid input, network, or server errors
    pub fn read_notes(&self, commit_shas: &[&str]) -> Result<NotesReadResponse, GitAiError> {
        // Validate that all SHAs are hex strings before making the request
        if let Some(sha) = first_non_hex(commit_shas) {
            // Retain this legacy Generic variant because its exact display is
            // part of the existing input-validation contract.
            return Err(GitAiError::Generic(format!(
                "Commit SHA contains non-hex characters: {}",
                sha
            )));
        }

        let query = commit_shas.join(",");
        let endpoint = format!("/worker/notes/?commits={}", query);
        let response = self.context().get(&endpoint)?;
        let response = ResponseEnvelope::read(&response)?;

        match response.status_code() {
            200 => response.parse_json(),
            // All SHAs not found — return empty response gracefully
            404 => Ok(NotesReadResponse {
                notes: std::collections::HashMap::new(),
            }),
            _ => Err(response.into_error("notes read", "unexpected error")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::api::client::{ApiClient, ApiContext};
    use crate::model::api_types::NoteEntry;

    #[test]
    fn test_read_notes_rejects_non_hex_sha() {
        let ctx = ApiContext::without_auth(Some("https://example.com".to_string()), || None);
        let client = ApiClient::new(ctx);

        let result = client.read_notes(&["not-a-hex-sha"]);
        assert_eq!(
            result.expect_err("invalid SHA should fail").to_string(),
            "Generic error: Commit SHA contains non-hex characters: not-a-hex-sha"
        );
    }

    #[test]
    fn test_read_notes_accepts_valid_hex_sha() {
        // A valid hex SHA should pass validation (the actual HTTP call will fail
        // because there is no real server, but we are testing input validation only).
        let ctx = ApiContext::without_auth(Some("https://127.0.0.1:1".to_string()), || None);
        let client = ApiClient::new(ctx);

        let valid_sha = "abc123def456abc123def456abc123def456abc1";
        // This will fail on the HTTP call, not on validation
        let result = client.read_notes(&[valid_sha]);
        // The error should be network-related, not a validation error
        if let Err(e) = result {
            let msg = e.to_string();
            assert!(
                !msg.contains("non-hex"),
                "should not fail hex validation for valid SHA, got: {}",
                msg
            );
        }
    }

    #[test]
    fn test_notes_upload_request_serialization() {
        let request = NotesUploadRequest {
            entries: vec![NoteEntry {
                commit_sha: "abc123".to_string(),
                content: "authorship data".to_string(),
            }],
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("abc123"));
        assert!(json.contains("authorship data"));
        assert!(json.contains("entries"));
    }

    #[test]
    fn test_notes_read_response_deserialization() {
        let json = r#"{"notes": {"abc123": "content1", "def456": "content2"}}"#;
        let response: NotesReadResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.notes.get("abc123"), Some(&"content1".to_string()));
        assert_eq!(response.notes.get("def456"), Some(&"content2".to_string()));
    }

    #[test]
    fn test_notes_upload_response_deserialization() {
        let json = r#"{"success_count": 5, "failure_count": 1}"#;
        let response: NotesUploadResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.success_count, 5);
        assert_eq!(response.failure_count, 1);
    }
}
