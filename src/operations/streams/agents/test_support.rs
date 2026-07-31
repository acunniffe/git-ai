//! Shared test-only contract coverage for incremental stream adapters.

use crate::model::stream_watermark::WatermarkStrategy;
use crate::operations::streams::agent::Agent;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

const FULL_DRAIN_RECORD_COUNT: usize = 5;
const EXISTING_RECORD_COUNT: usize = 3;
const APPEND_MANY_RECORD_COUNT: usize = 3;

pub(crate) struct StreamAdapterFixture<'a> {
    path: &'a Path,
    reset_records: Box<dyn FnMut(usize) + 'a>,
    append_records: Box<dyn FnMut(usize, usize) + 'a>,
}

impl<'a> StreamAdapterFixture<'a> {
    pub(crate) fn new(
        path: &'a Path,
        reset_records: impl FnMut(usize) + 'a,
        append_records: impl FnMut(usize, usize) + 'a,
    ) -> Self {
        Self {
            path,
            reset_records: Box::new(reset_records),
            append_records: Box::new(append_records),
        }
    }

    fn path(&self) -> &Path {
        self.path
    }

    fn reset_records(&mut self, record_count: usize) {
        (self.reset_records)(record_count);
    }

    fn append_records(&mut self, first_new_record: usize, record_count: usize) {
        (self.append_records)(first_new_record, record_count);
    }
}

pub(crate) fn jsonl_fixture<'a>(
    path: &'a Path,
    record: fn(usize) -> String,
) -> StreamAdapterFixture<'a> {
    let reset_path = path.to_path_buf();
    let append_path = path.to_path_buf();
    StreamAdapterFixture::new(
        path,
        move |record_count| {
            let mut file = fs::File::create(&reset_path).unwrap();
            for record_index in 0..record_count {
                writeln!(file, "{}", record(record_index)).unwrap();
            }
        },
        move |first_new_record, record_count| {
            let mut file = OpenOptions::new().append(true).open(&append_path).unwrap();
            for record_index in first_new_record..first_new_record + record_count {
                writeln!(file, "{}", record(record_index)).unwrap();
            }
        },
    )
}

pub(crate) fn rewritten_file_fixture<'a>(
    path: &'a Path,
    records: fn(usize) -> String,
) -> StreamAdapterFixture<'a> {
    let reset_path = path.to_path_buf();
    let append_path = path.to_path_buf();
    StreamAdapterFixture::new(
        path,
        move |record_count| {
            fs::write(&reset_path, records(record_count)).unwrap();
        },
        move |first_new_record, record_count| {
            fs::write(&append_path, records(first_new_record + record_count)).unwrap();
        },
    )
}

#[derive(Clone, Copy)]
pub(crate) struct StreamAdapterContractCapabilities {
    pub(crate) append_one: bool,
    pub(crate) append_many: bool,
}

impl StreamAdapterContractCapabilities {
    pub(crate) const APPEND_ALL: Self = Self {
        append_one: true,
        append_many: true,
    };
}

pub(crate) fn assert_stream_adapter_contract<A, W, I>(
    agent: &A,
    fixture: &mut StreamAdapterFixture<'_>,
    make_watermark: W,
    event_identity: I,
    batch_size: usize,
    session_id: &str,
    capabilities: StreamAdapterContractCapabilities,
) where
    A: Agent,
    W: Fn() -> Box<dyn WatermarkStrategy>,
    I: Fn(&Value) -> String,
{
    fixture.reset_records(FULL_DRAIN_RECORD_COUNT);
    let (full_drain, _) = drain_stream(
        agent,
        fixture.path(),
        make_watermark(),
        batch_size,
        session_id,
    );
    assert_eq!(
        full_drain.len(),
        FULL_DRAIN_RECORD_COUNT,
        "full drain must return every record"
    );
    let full_drain_ids = event_ids(&full_drain, &event_identity);
    assert_unique(&full_drain_ids, "full drain must not duplicate records");

    fixture.reset_records(FULL_DRAIN_RECORD_COUNT);
    let first_batch = agent
        .read_incremental(fixture.path(), make_watermark(), session_id)
        .unwrap();
    assert!(
        !first_batch.events.is_empty(),
        "a populated stream must produce an initial batch"
    );
    assert!(
        first_batch.events.len() <= batch_size,
        "a batch must not exceed its configured limit"
    );
    let mut resumed_events = first_batch.events;
    let (remaining_events, _) = drain_stream(
        agent,
        fixture.path(),
        first_batch.new_watermark,
        batch_size,
        session_id,
    );
    resumed_events.extend(remaining_events);
    assert_eq!(
        event_ids(&resumed_events, &event_identity),
        full_drain_ids,
        "resuming from a watermark must neither lose nor duplicate records"
    );

    if capabilities.append_one {
        assert_append_one(
            agent,
            fixture,
            &make_watermark,
            &event_identity,
            batch_size,
            session_id,
        );
    }
    if capabilities.append_many {
        assert_append_many(
            agent,
            fixture,
            &make_watermark,
            &event_identity,
            batch_size,
            session_id,
        );
    }
}

fn assert_append_one<A, W, I>(
    agent: &A,
    fixture: &mut StreamAdapterFixture<'_>,
    make_watermark: &W,
    event_identity: &I,
    batch_size: usize,
    session_id: &str,
) where
    A: Agent,
    W: Fn() -> Box<dyn WatermarkStrategy>,
    I: Fn(&Value) -> String,
{
    fixture.reset_records(EXISTING_RECORD_COUNT);
    let (existing_events, watermark) = drain_stream(
        agent,
        fixture.path(),
        make_watermark(),
        batch_size,
        session_id,
    );
    fixture.append_records(EXISTING_RECORD_COUNT, 1);
    let expected_appended_ids = expected_appended_ids(
        agent,
        fixture,
        make_watermark,
        event_identity,
        batch_size,
        session_id,
        existing_events.len(),
        1,
    );

    let appended_batch = agent
        .read_incremental(fixture.path(), watermark, session_id)
        .unwrap();
    assert_eq!(
        appended_batch.events.len(),
        1,
        "appending one record must produce exactly one unread record"
    );
    assert_eq!(
        event_ids(&appended_batch.events, event_identity),
        expected_appended_ids,
        "the appended record must match the tail of a fresh full drain"
    );

    assert_new_ids(
        &existing_events,
        &appended_batch.events,
        event_identity,
        "the appended record must not repeat an already drained record",
    );
}

fn assert_append_many<A, W, I>(
    agent: &A,
    fixture: &mut StreamAdapterFixture<'_>,
    make_watermark: &W,
    event_identity: &I,
    batch_size: usize,
    session_id: &str,
) where
    A: Agent,
    W: Fn() -> Box<dyn WatermarkStrategy>,
    I: Fn(&Value) -> String,
{
    fixture.reset_records(EXISTING_RECORD_COUNT);
    let (existing_events, watermark) = drain_stream(
        agent,
        fixture.path(),
        make_watermark(),
        batch_size,
        session_id,
    );
    fixture.append_records(EXISTING_RECORD_COUNT, APPEND_MANY_RECORD_COUNT);
    let expected_appended_ids = expected_appended_ids(
        agent,
        fixture,
        make_watermark,
        event_identity,
        batch_size,
        session_id,
        existing_events.len(),
        APPEND_MANY_RECORD_COUNT,
    );
    let (appended_events, _) =
        drain_stream(agent, fixture.path(), watermark, batch_size, session_id);
    assert_eq!(
        appended_events.len(),
        APPEND_MANY_RECORD_COUNT,
        "all appended records must be drained across bounded batches"
    );
    assert_eq!(
        event_ids(&appended_events, event_identity),
        expected_appended_ids,
        "appended records must match the tail of a fresh full drain"
    );
    assert_new_ids(
        &existing_events,
        &appended_events,
        event_identity,
        "appended records must not repeat already drained records",
    );
}

pub(crate) fn drain_stream<A: Agent>(
    agent: &A,
    path: &Path,
    mut watermark: Box<dyn WatermarkStrategy>,
    batch_size: usize,
    session_id: &str,
) -> (Vec<Value>, Box<dyn WatermarkStrategy>) {
    let mut events = Vec::new();
    loop {
        let batch = agent.read_incremental(path, watermark, session_id).unwrap();
        assert!(
            batch.events.len() <= batch_size,
            "a batch must not exceed its configured limit"
        );

        let batch_len = batch.events.len();
        watermark = batch.new_watermark;
        if batch_len == 0 {
            return (events, watermark);
        }
        events.extend(batch.events);
    }
}

fn event_ids<I: Fn(&Value) -> String>(events: &[Value], event_identity: &I) -> Vec<String> {
    events.iter().map(event_identity).collect()
}

#[allow(clippy::too_many_arguments)]
fn expected_appended_ids<A, W, I>(
    agent: &A,
    fixture: &StreamAdapterFixture<'_>,
    make_watermark: &W,
    event_identity: &I,
    batch_size: usize,
    session_id: &str,
    existing_record_count: usize,
    appended_record_count: usize,
) -> Vec<String>
where
    A: Agent,
    W: Fn() -> Box<dyn WatermarkStrategy>,
    I: Fn(&Value) -> String,
{
    let (full_drain, _) = drain_stream(
        agent,
        fixture.path(),
        make_watermark(),
        batch_size,
        session_id,
    );
    assert_eq!(
        full_drain.len(),
        existing_record_count + appended_record_count,
        "a fresh full drain must include the existing and appended records"
    );
    event_ids(&full_drain, event_identity)
        .into_iter()
        .skip(existing_record_count)
        .collect()
}

fn assert_new_ids<I: Fn(&Value) -> String>(
    existing_events: &[Value],
    appended_events: &[Value],
    event_identity: &I,
    message: &str,
) {
    let existing_ids = event_ids(existing_events, event_identity);
    let appended_ids = event_ids(appended_events, event_identity);
    assert_unique(&appended_ids, message);
    assert!(
        appended_ids.iter().all(|id| !existing_ids.contains(id)),
        "{message}"
    );
}

fn assert_unique(ids: &[String], message: &str) {
    let unique_ids: HashSet<&str> = ids.iter().map(String::as_str).collect();
    assert_eq!(unique_ids.len(), ids.len(), "{message}");
}
