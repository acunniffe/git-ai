//! Behavioral tests for `flush_pending_metric_records_with` in `telemetry_worker`.
//!
//! Kept in a sibling file so the tests do not push `telemetry_worker.rs` past
//! its file-length baseline ceiling.

use super::telemetry_worker::flush_pending_metric_records_with;
use crate::error::GitAiError;
use crate::model::repository::metrics_db::MetricRecord;
use std::cell::RefCell;
use std::rc::Rc;

fn make_record(id: i64) -> MetricRecord {
    // Valid MetricEvent JSON so the record is not treated as invalid.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u32;
    MetricRecord {
        id,
        event_json: format!(r#"{{"t":{ts},"e":1,"v":{{}},"a":{{}}}}"#),
        attempts: 0,
        next_retry_at: 0,
    }
}

fn api_err(status: u16) -> GitAiError {
    GitAiError::Api(crate::clients::api::error::ApiError {
        operation: "metrics upload",
        status: Some(status),
        message: "test".to_string(),
    })
}

/// Inject one record, return `(mark_failed_calls, mark_undeliverable_calls)`.
fn flush_with_error(upload_err: GitAiError) -> (usize, usize) {
    let batch = Rc::new(RefCell::new(Some(vec![make_record(1)])));
    let failed = Rc::new(RefCell::new(0usize));
    let undeliverable = Rc::new(RefCell::new(0usize));
    let _ = flush_pending_metric_records_with(
        {
            let b = Rc::clone(&batch);
            move |_| Ok(b.borrow_mut().take().unwrap_or_default())
        },
        |_| true,
        |_| Ok(()),
        {
            let f = Rc::clone(&failed);
            move |_, _| {
                *f.borrow_mut() += 1;
                Ok(())
            }
        },
        {
            let u = Rc::clone(&undeliverable);
            move |_| {
                *u.borrow_mut() += 1;
                Ok(())
            }
        },
        move |_| Err(upload_err.clone()),
        std::time::Instant::now() + std::time::Duration::from_secs(60),
        10,
    );
    (*failed.borrow(), *undeliverable.borrow())
}

/// A Terminal `ApiError` (e.g. 401 Unauthorized) must route records to
/// `mark_undeliverable` so they do not burn the 6-attempt retry budget.
#[test]
fn flush_pending_metric_records_terminal_api_error_marks_undeliverable() {
    let (failed, undeliverable) = flush_with_error(api_err(401));
    assert_eq!(failed, 0, "Terminal ApiError must not call mark_failed");
    assert_eq!(
        undeliverable, 1,
        "Terminal ApiError must call mark_undeliverable"
    );
}

/// A Retryable `ApiError` (e.g. 500 Internal Server Error) must continue to
/// call `mark_failed` so the existing backoff retry path is preserved.
#[test]
fn flush_pending_metric_records_retryable_api_error_marks_failed() {
    let (failed, undeliverable) = flush_with_error(api_err(500));
    assert_eq!(failed, 1, "Retryable ApiError must call mark_failed");
    assert_eq!(
        undeliverable, 0,
        "Retryable ApiError must not call mark_undeliverable"
    );
}
