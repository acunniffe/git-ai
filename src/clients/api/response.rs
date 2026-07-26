use crate::clients::api::error::http_status_error;
use crate::clients::http;
use crate::error::GitAiError;
use serde::de::DeserializeOwned;

/// UTF-8 API response body paired with its HTTP status.
///
/// Reading the body remains a separate first step so endpoint-specific
/// handling (notably the CAS and notes 404 contracts) can inspect the status
/// without duplicating body and JSON error conversion.
#[derive(Debug)]
pub(super) struct ResponseEnvelope<'a> {
    status_code: u16,
    body: &'a str,
}

impl<'a> ResponseEnvelope<'a> {
    pub(super) fn read(response: &'a http::Response) -> Result<Self, GitAiError> {
        Self::from_utf8(response.status_code, response.as_str())
    }

    pub(super) fn from_utf8(
        status_code: u16,
        body: Result<&'a str, std::str::Utf8Error>,
    ) -> Result<Self, GitAiError> {
        // Keep the legacy Generic wrapper because callers observe its exact
        // "Generic error: Failed to read response body: ..." Display text.
        let body = body.map_err(|error| {
            GitAiError::Generic(format!("Failed to read response body: {error}"))
        })?;
        Ok(Self { status_code, body })
    }

    pub(super) fn status_code(&self) -> u16 {
        self.status_code
    }

    pub(super) fn parse_json<T>(&self) -> Result<T, GitAiError>
    where
        T: DeserializeOwned,
    {
        serde_json::from_str(self.body).map_err(GitAiError::JsonError)
    }

    pub(super) fn into_error(self, operation: &'static str, default_message: &str) -> GitAiError {
        http_status_error(operation, self.status_code, self.body, default_message).into()
    }

    pub(super) fn decode_json<T>(
        self,
        success_status: u16,
        operation: &'static str,
        default_message: &str,
    ) -> Result<T, GitAiError>
    where
        T: DeserializeOwned,
    {
        if self.status_code == success_status {
            self.parse_json()
        } else {
            Err(self.into_error(operation, default_message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct TestResponse {
        value: u8,
    }

    #[test]
    fn decodes_json_only_for_the_expected_success_status() {
        let response =
            ResponseEnvelope::from_utf8(200, Ok(r#"{"value":7}"#)).expect("valid response body");

        assert_eq!(
            response
                .decode_json::<TestResponse>(200, "test operation", "unexpected error")
                .expect("decode success response"),
            TestResponse { value: 7 }
        );
    }

    #[test]
    fn preserves_json_error_variant_for_malformed_success_body() {
        let response =
            ResponseEnvelope::from_utf8(200, Ok("{")).expect("valid UTF-8 response body");

        let error = response
            .decode_json::<TestResponse>(200, "test operation", "unexpected error")
            .expect_err("malformed JSON should fail");

        assert!(matches!(error, GitAiError::JsonError(_)));
        assert_eq!(
            error.to_string(),
            "JSON error: EOF while parsing an object at line 1 column 1"
        );
    }

    #[test]
    fn preserves_structured_api_error_for_non_success_status() {
        let response = ResponseEnvelope::from_utf8(
            503,
            Ok(r#"{"error":"temporarily unavailable","details":null}"#),
        )
        .expect("valid response body");

        let error = response
            .decode_json::<TestResponse>(200, "test operation", "unexpected error")
            .expect_err("non-success status should fail");

        assert_eq!(
            error.to_string(),
            "test operation failed with status 503: temporarily unavailable"
        );
        assert!(matches!(error, GitAiError::Api(_)));
    }

    #[test]
    fn preserves_default_message_for_empty_error_body() {
        let response = ResponseEnvelope::from_utf8(500, Ok("")).expect("valid response body");

        let error = response
            .decode_json::<TestResponse>(200, "test operation", "unexpected error")
            .expect_err("non-success status should fail");

        assert_eq!(
            error.to_string(),
            "test operation failed with status 500: unexpected error"
        );
    }

    #[test]
    fn preserves_generic_display_for_non_utf8_response_body() {
        let mut invalid_body = b"x".to_vec();
        invalid_body[0] = 0xff;
        let error = ResponseEnvelope::from_utf8(200, std::str::from_utf8(&invalid_body))
            .expect_err("non-UTF-8 response body should fail");

        assert_eq!(
            error.to_string(),
            "Generic error: Failed to read response body: invalid utf-8 sequence of 1 bytes from index 0"
        );
    }
}
