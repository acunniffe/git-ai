use crate::error::GitAiError;
use rusqlite::{OptionalExtension, params};

use super::MetricsDatabase;
use super::event_writes::{METADATA_BACKFILL_BATCH_SIZE, extract_metric_event_metadata};
use super::types::MetricMetadataBackfillSummary;

const EVENT_METADATA_BACKFILL_COMPLETED_KEY: &str = "event_metadata_backfill_completed";

impl MetricsDatabase {
    /// Whether the one-time event-metadata backfill has already run to completion.
    pub(crate) fn event_metadata_backfill_completed(&self) -> Result<bool, GitAiError> {
        let completed: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = ?1",
                params![EVENT_METADATA_BACKFILL_COMPLETED_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(completed.as_deref() == Some("1"))
    }

    /// Backfill one bounded batch, permanently marking the one-time migration
    /// complete once a scan reaches the end of the table. Completed databases
    /// skip all further scanning.
    pub(crate) fn backfill_event_metadata_batch_once(
        &mut self,
        after_id: i64,
        limit: usize,
    ) -> Result<(MetricMetadataBackfillSummary, Option<i64>), GitAiError> {
        if limit == 0 || self.event_metadata_backfill_completed()? {
            return Ok((MetricMetadataBackfillSummary::default(), None));
        }

        let result = self.backfill_event_metadata_batch_after(after_id, limit)?;
        if result.0.scanned < limit {
            self.conn.execute(
                "INSERT OR REPLACE INTO schema_metadata (key, value) VALUES (?1, '1')",
                params![EVENT_METADATA_BACKFILL_COMPLETED_KEY],
            )?;
        }
        Ok(result)
    }

    /// Backfill cached event metadata for one bounded batch of legacy rows.
    pub fn backfill_event_metadata_batch(
        &mut self,
        limit: usize,
    ) -> Result<MetricMetadataBackfillSummary, GitAiError> {
        self.backfill_event_metadata_batch_after(0, limit)
            .map(|(summary, _)| summary)
    }

    /// Backfill cached event metadata for all currently eligible legacy rows.
    pub fn backfill_event_metadata(&mut self) -> Result<MetricMetadataBackfillSummary, GitAiError> {
        let mut total = MetricMetadataBackfillSummary::default();
        let mut after_id = 0;

        loop {
            let (summary, last_id) =
                self.backfill_event_metadata_batch_after(after_id, METADATA_BACKFILL_BATCH_SIZE)?;
            total.scanned += summary.scanned;
            total.updated += summary.updated;

            let Some(id) = last_id else {
                break;
            };
            after_id = id;

            if summary.scanned < METADATA_BACKFILL_BATCH_SIZE {
                break;
            }
        }

        Ok(total)
    }

    pub(crate) fn backfill_event_metadata_batch_after(
        &mut self,
        after_id: i64,
        limit: usize,
    ) -> Result<(MetricMetadataBackfillSummary, Option<i64>), GitAiError> {
        if limit == 0 {
            return Ok((MetricMetadataBackfillSummary::default(), None));
        }

        let rows = {
            let mut stmt = self.conn.prepare(
                "SELECT id, event_json FROM metrics \
                 WHERE id > ?1 AND (event_ts IS NULL OR event_kind IS NULL) \
                 ORDER BY id ASC \
                 LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![after_id, limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };

        let mut summary = MetricMetadataBackfillSummary {
            scanned: rows.len(),
            updated: 0,
        };
        let last_id = rows.last().map(|(id, _)| *id);
        if rows.is_empty() {
            return Ok((summary, last_id));
        }

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                r#"
                UPDATE metrics
                SET event_ts = ?1,
                    event_kind = ?2,
                    trace_id = ?3,
                    session_id = ?4,
                    parent_session_id = ?5,
                    tool = ?6,
                    external_session_id = ?7,
                    external_parent_session_id = ?8,
                    external_event_id = ?9,
                    external_parent_event_id = ?10,
                    external_tool_use_id = ?11
                WHERE id = ?12
                "#,
            )?;

            for (id, event_json) in rows {
                let Some(metadata) = extract_metric_event_metadata(&event_json) else {
                    continue;
                };

                stmt.execute(params![
                    i64::from(metadata.event_ts),
                    i64::from(metadata.event_kind),
                    metadata.trace_id.as_deref(),
                    metadata.session_id.as_deref(),
                    metadata.parent_session_id.as_deref(),
                    metadata.tool.as_deref(),
                    metadata.external_session_id.as_deref(),
                    metadata.external_parent_session_id.as_deref(),
                    metadata.external_event_id.as_deref(),
                    metadata.external_parent_event_id.as_deref(),
                    metadata.external_tool_use_id.as_deref(),
                    id,
                ])?;
                summary.updated += 1;
            }
        }
        tx.commit()?;

        Ok((summary, last_id))
    }
}
