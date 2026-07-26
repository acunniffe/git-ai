use crate::clients::api::client::ApiClient;
use crate::clients::api::first_non_hex;
use crate::clients::api::response::ResponseEnvelope;
use crate::error::GitAiError;
use crate::model::api_types::{CAPromptStoreReadResponse, CasUploadRequest, CasUploadResponse};

fn decode_cas_read_response(
    response: ResponseEnvelope<'_>,
    requested_hashes: usize,
) -> Result<CAPromptStoreReadResponse, GitAiError> {
    match response.status_code() {
        200 => response.parse_json(),
        // All hashes not found — return empty response gracefully.
        404 => Ok(CAPromptStoreReadResponse {
            results: Vec::new(),
            success_count: 0,
            failure_count: requested_hashes,
        }),
        _ => Err(response.into_error("CAS read", "unexpected error")),
    }
}

/// CAS API endpoints
impl ApiClient {
    /// Upload CAS objects to the server
    ///
    /// # Arguments
    /// * `request` - The CAS upload request containing objects to upload
    ///
    /// # Returns
    /// * `Ok(CasUploadResponse)` - Success response
    /// * `Err(GitAiError)` - Error response
    pub fn upload_cas(&self, request: CasUploadRequest) -> Result<CasUploadResponse, GitAiError> {
        let response = self.context().post_json("/worker/cas/upload", &request)?;
        ResponseEnvelope::read(&response)?.decode_json(200, "CAS upload", "unexpected error")
    }

    /// Read CAS objects by hash from the server
    ///
    /// # Arguments
    /// * `hashes` - Slice of CAS hashes to fetch (max 100 per call)
    ///
    /// # Returns
    /// * `Ok(CAPromptStoreReadResponse)` - Response with results for each hash
    /// * `Err(GitAiError)` - On network or server errors
    pub fn read_ca_prompt_store(
        &self,
        hashes: &[&str],
    ) -> Result<CAPromptStoreReadResponse, GitAiError> {
        // Validate all hashes are hex-only before building the URL to prevent
        // injection via crafted hash values in the query string.
        if let Some(hash) = first_non_hex(hashes) {
            // Retain this legacy Generic variant because its exact display is
            // part of the existing input-validation contract.
            return Err(GitAiError::Generic(format!(
                "CAS hash contains non-hex characters: {}",
                hash
            )));
        }

        let query = hashes.join(",");
        let endpoint = format!("/worker/cas/?hashes={}", query);
        let response = self.context().get(&endpoint)?;
        let response = ResponseEnvelope::read(&response)?;
        decode_cas_read_response(response, hashes.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{ResponseEnvelope, decode_cas_read_response};
    use crate::clients::api::{ApiClient, ApiContext};

    #[test]
    fn read_cas_preserves_empty_404_response_contract() {
        let response = ResponseEnvelope::from_utf8(404, Ok("")).expect("valid empty response body");
        let result = decode_cas_read_response(response, 1)
            .expect("404 remains a successful empty CAS response");

        assert!(result.results.is_empty());
        assert_eq!(result.success_count, 0);
        assert_eq!(result.failure_count, 1);
    }

    #[test]
    fn read_cas_preserves_invalid_hash_error_text() {
        let client = ApiClient::new(ApiContext::without_auth(
            Some("https://example.com".to_string()),
            || None,
        ));

        let error = client
            .read_ca_prompt_store(&["not-hex"])
            .expect_err("invalid hash should fail before the request");

        assert_eq!(
            error.to_string(),
            "Generic error: CAS hash contains non-hex characters: not-hex"
        );
    }
}
