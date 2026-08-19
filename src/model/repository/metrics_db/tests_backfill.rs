use super::test_support::*;
use super::*;
use rusqlite::params;

#[test]
fn test_backfill_event_metadata_batch_updates_valid_legacy_rows_only() {
    let (mut db, _temp_dir) = create_test_db();
    let ts1 = days_ago(3);
    let ts2 = days_ago(2);
    db.conn
        .execute(
            "INSERT INTO metrics (event_json) VALUES (?1), (?2), (?3)",
            params![
                event_json_with_all_common_metadata(ts1, 1),
                format!(
                    r#"{{"t":{ts2},"e":5,"v":{{"1":"legacy-event","2":"legacy-parent","3":"legacy-tool"}},"a":{{"1":"https://github.com/acme/project"}}}}"#
                ),
                "not-json",
            ],
        )
        .unwrap();

    let summary = db.backfill_event_metadata_batch(100).unwrap();

    assert_eq!(summary.scanned, 3);
    assert_eq!(summary.updated, 2);
    assert_eq!(
        metric_metadata_rows(&db),
        vec![
            (Some(ts1 as i64), Some(1)),
            (Some(ts2 as i64), Some(5)),
            (None, None),
        ]
    );
    assert_eq!(
        metric_identifier_rows(&db),
        vec![
            MetricIdentifierRow {
                trace_id: Some("trace-1".to_string()),
                session_id: Some("session-1".to_string()),
                parent_session_id: Some("parent-session-1".to_string()),
                tool: Some("codex".to_string()),
                external_session_id: Some("external-session-1".to_string()),
                external_parent_session_id: Some("external-parent-session-1".to_string()),
                external_event_id: None,
                external_parent_event_id: None,
                external_tool_use_id: None,
            },
            MetricIdentifierRow {
                trace_id: None,
                session_id: None,
                parent_session_id: None,
                tool: None,
                external_session_id: None,
                external_parent_session_id: None,
                external_event_id: Some("legacy-event".to_string()),
                external_parent_event_id: Some("legacy-parent".to_string()),
                external_tool_use_id: Some("legacy-tool".to_string()),
            },
            MetricIdentifierRow {
                trace_id: None,
                session_id: None,
                parent_session_id: None,
                tool: None,
                external_session_id: None,
                external_parent_session_id: None,
                external_event_id: None,
                external_parent_event_id: None,
                external_tool_use_id: None,
            },
        ]
    );
}

#[test]
fn test_backfill_event_metadata_batch_after_advances_cursor() {
    let (mut db, _temp_dir) = create_test_db();
    let ts1 = days_ago(3);
    let ts2 = days_ago(2);
    let ts3 = days_ago(1);
    db.conn
        .execute(
            "INSERT INTO metrics (event_json) VALUES (?1), (?2), (?3)",
            params![event_json(ts1), event_json(ts2), event_json(ts3)],
        )
        .unwrap();

    let (first_summary, first_last_id) = db.backfill_event_metadata_batch_after(0, 2).unwrap();

    assert_eq!(
        first_summary,
        MetricMetadataBackfillSummary {
            scanned: 2,
            updated: 2,
        }
    );
    assert_eq!(
        metric_metadata_rows(&db),
        vec![
            (Some(ts1 as i64), Some(1)),
            (Some(ts2 as i64), Some(1)),
            (None, None),
        ]
    );

    let first_last_id = first_last_id.unwrap();
    let (second_summary, second_last_id) = db
        .backfill_event_metadata_batch_after(first_last_id, 2)
        .unwrap();

    assert_eq!(
        second_summary,
        MetricMetadataBackfillSummary {
            scanned: 1,
            updated: 1,
        }
    );
    assert!(second_last_id.is_some_and(|id| id > first_last_id));
    assert_eq!(
        metric_metadata_rows(&db),
        vec![
            (Some(ts1 as i64), Some(1)),
            (Some(ts2 as i64), Some(1)),
            (Some(ts3 as i64), Some(1)),
        ]
    );

    let (empty_summary, empty_last_id) = db
        .backfill_event_metadata_batch_after(second_last_id.unwrap(), 2)
        .unwrap();
    assert_eq!(empty_summary, MetricMetadataBackfillSummary::default());
    assert_eq!(empty_last_id, None);
}

#[test]
fn test_backfill_event_metadata_batch_once_marks_completion() {
    let (mut db, temp_dir) = create_test_db();
    db.conn
        .execute(
            "INSERT INTO metrics (event_json) VALUES (?1), (?2)",
            params![event_json(days_ago(2)), event_json(days_ago(1))],
        )
        .unwrap();

    // A full batch leaves the migration incomplete; the short batch that
    // reaches the end of the table marks it complete.
    let (first_summary, last_id) = db.backfill_event_metadata_batch_once(0, 1).unwrap();
    assert_eq!(first_summary.scanned, 1);
    assert!(!db.event_metadata_backfill_completed().unwrap());

    let (second_summary, _) = db
        .backfill_event_metadata_batch_once(last_id.unwrap(), 2)
        .unwrap();
    assert_eq!(second_summary.scanned, 1);
    assert!(db.event_metadata_backfill_completed().unwrap());

    // Completion survives reopening the database.
    drop(db);
    let mut db = MetricsDatabase::open_at_path(&temp_dir.path().join("test-metrics.db")).unwrap();
    assert!(db.event_metadata_backfill_completed().unwrap());

    // Rows inserted after completion are never re-scanned by the one-time
    // backfill; live writes populate their own metadata.
    db.conn
        .execute(
            "INSERT INTO metrics (event_json) VALUES (?1)",
            params![event_json(days_ago(0))],
        )
        .unwrap();

    let skipped = db.backfill_event_metadata_batch_once(0, 100).unwrap();
    assert_eq!(skipped.0.scanned, 0);
    assert_eq!(skipped.1, None);
}
