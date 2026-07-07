#![allow(clippy::too_many_lines)] // integration-test exemption for integration test modules

use euler_core::permissions::ScriptedDecider;
use euler_core::{
    query_provenance, EventWakeError, EventWakePoll, EventWakeRecv, ProvenanceQuery,
    ProvenanceWriter, Session, SessionConfig, SessionError, SessionEventWake,
    MAX_EVENT_WAKE_RECEIVERS,
};
use euler_event::{object, EventEnvelope, EventKind};
use euler_provider::ScriptedProvider;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[test]
fn durable_event_wakes_once_and_is_queryable() {
    let fixture = Fixture::new();
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;
    let event = durable_event("sentinel payload");

    fixture
        .writer
        .append(std::slice::from_ref(&event))
        .expect("append");

    assert_eq!(wake.try_recv(), EventWakePoll::Advanced);
    assert_eq!(wake.try_recv(), EventWakePoll::Empty);
    let events = query_after(fixture.log(), None, 10);
    assert_eq!(event_ids(&events), vec![event.id.as_str()]);
}

#[test]
fn multiple_durable_events_coalesce_until_polled() {
    let fixture = Fixture::new();
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;
    let first = durable_event("one");
    let second = durable_event("two");

    fixture
        .writer
        .append(std::slice::from_ref(&first))
        .expect("append first");
    fixture
        .writer
        .append(std::slice::from_ref(&second))
        .expect("append second");

    assert_eq!(wake.try_recv(), EventWakePoll::Advanced);
    assert_eq!(wake.try_recv(), EventWakePoll::Empty);
    let events = query_after(fixture.log(), None, 10);
    assert_eq!(
        event_ids(&events),
        vec![first.id.as_str(), second.id.as_str()]
    );
}

#[test]
fn coalescing_resets_after_poll() {
    let fixture = Fixture::new();
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;

    fixture
        .writer
        .append(&[durable_event("one")])
        .expect("append");
    assert_eq!(wake.try_recv(), EventWakePoll::Advanced);
    assert_eq!(wake.try_recv(), EventWakePoll::Empty);
    fixture
        .writer
        .append(&[durable_event("two")])
        .expect("append");
    assert_eq!(wake.try_recv(), EventWakePoll::Advanced);
}

#[test]
fn runtime_only_events_do_not_wake_or_advance_baseline() {
    let fixture = Fixture::new();
    fixture
        .writer
        .append(&[runtime_delta("streaming only")])
        .expect("append runtime-only");

    let registration = fixture.writer.open_event_wake().expect("open wake");
    let mut wake = registration.wake;
    assert_eq!(registration.baseline_event_id, None);
    assert_eq!(wake.try_recv(), EventWakePoll::Empty);

    fixture
        .writer
        .append(&[runtime_delta("still streaming")])
        .expect("append runtime-only again");
    assert_eq!(wake.try_recv(), EventWakePoll::Empty);
}

#[test]
fn mixed_durable_and_runtime_only_batch_wakes_once() {
    let fixture = Fixture::new();
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;
    let durable = durable_event("persist me");

    fixture
        .writer
        .append(&[runtime_delta("skip me"), durable.clone()])
        .expect("append mixed batch");

    assert_eq!(wake.try_recv(), EventWakePoll::Advanced);
    assert_eq!(wake.try_recv(), EventWakePoll::Empty);
    let events = query_after(fixture.log(), None, 10);
    assert_eq!(event_ids(&events), vec![durable.id.as_str()]);
}

#[test]
fn append_failure_does_not_wake() {
    let fixture = Fixture::new();
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;
    std::fs::create_dir(fixture.log()).expect("turn log path into directory");

    fixture
        .writer
        .append(&[durable_event("cannot write")])
        .expect_err("append fails");

    assert_eq!(wake.try_recv(), EventWakePoll::Empty);
}

#[test]
fn dropped_receivers_are_pruned_without_blocking_remaining_receivers() {
    let fixture = Fixture::new();
    let dropped = fixture.writer.open_event_wake().expect("open dropped").wake;
    let mut kept = fixture.writer.open_event_wake().expect("open kept").wake;
    drop(dropped);

    fixture
        .writer
        .append(&[durable_event("wake")])
        .expect("append");

    assert_eq!(kept.try_recv(), EventWakePoll::Advanced);
}

#[test]
fn multiple_receivers_are_independent() {
    let fixture = Fixture::new();
    let mut first = fixture.writer.open_event_wake().expect("open first").wake;
    let mut second = fixture.writer.open_event_wake().expect("open second").wake;

    fixture
        .writer
        .append(&[durable_event("wake")])
        .expect("append");

    assert_eq!(first.try_recv(), EventWakePoll::Advanced);
    assert_eq!(first.try_recv(), EventWakePoll::Empty);
    assert_eq!(second.try_recv(), EventWakePoll::Advanced);
}

#[test]
fn recv_blocks_thread_and_wakes_after_durable_event() {
    let fixture = Fixture::new();
    let wake = fixture.writer.open_event_wake().expect("open wake").wake;
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut wake = wake;
        tx.send(wake.recv()).expect("send recv result");
        wake
    });

    thread::sleep(Duration::from_millis(20));
    fixture
        .writer
        .append(&[durable_event("wake")])
        .expect("append");

    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1))
            .expect("recv result"),
        EventWakeRecv::Advanced
    );
    let mut wake = handle.join().expect("join receiver");
    assert_eq!(wake.try_recv(), EventWakePoll::Empty);
}

#[test]
fn open_after_history_returns_baseline_without_replay_wake() {
    let fixture = Fixture::new();
    let first = durable_event("first");
    fixture
        .writer
        .append(std::slice::from_ref(&first))
        .expect("append");

    let registration = fixture.writer.open_event_wake().expect("open wake");
    let mut wake = registration.wake;

    assert_eq!(
        registration.baseline_event_id.as_deref(),
        Some(first.id.as_str())
    );
    assert_eq!(wake.try_recv(), EventWakePoll::Empty);
}

#[test]
fn open_ignores_torn_tail_when_computing_baseline() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let first = durable_event("accepted");
    let accepted = first.to_json_line().expect("serialize event");
    std::fs::write(&log, format!("{accepted}\n{{\"torn\"")).expect("write torn log");
    let writer = ProvenanceWriter::new(log).expect("writer");

    let registration = writer.open_event_wake().expect("open wake");

    assert_eq!(
        registration.baseline_event_id.as_deref(),
        Some(first.id.as_str())
    );
}

#[test]
fn open_after_large_history_uses_latest_accepted_event_id() {
    let fixture = Fixture::new();
    let history = (0..512)
        .map(|index| durable_event(&format!("history {index}")))
        .collect::<Vec<_>>();
    let baseline = history.last().expect("history event").id.clone();
    fixture.writer.append(&history).expect("append history");

    let registration = fixture.writer.open_event_wake().expect("open wake");
    let mut wake = registration.wake;

    assert_eq!(
        registration.baseline_event_id.as_deref(),
        Some(baseline.as_str())
    );
    fixture
        .writer
        .append(&[durable_event("after")])
        .expect("append after");
    assert_eq!(wake.try_recv(), EventWakePoll::Advanced);
}

#[test]
fn baseline_event_id_is_valid_query_cursor() {
    let fixture = Fixture::new();
    let first = durable_event("first");
    let second = durable_event("second");
    fixture
        .writer
        .append(std::slice::from_ref(&first))
        .expect("append first");
    let registration = fixture.writer.open_event_wake().expect("open wake");
    fixture
        .writer
        .append(std::slice::from_ref(&second))
        .expect("append second");

    let events = query_after(fixture.log(), registration.baseline_event_id, 10);

    assert_eq!(event_ids(&events), vec![second.id.as_str()]);
}

#[test]
fn open_before_initial_query_catches_subsequent_event() {
    let fixture = Fixture::new();
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;

    fixture
        .writer
        .append(&[durable_event("later")])
        .expect("append");

    assert_eq!(wake.try_recv(), EventWakePoll::Advanced);
}

#[test]
fn empty_append_emits_no_wake() {
    let fixture = Fixture::new();
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;

    fixture.writer.append(&[]).expect("empty append");

    assert_eq!(wake.try_recv(), EventWakePoll::Empty);
}

#[test]
fn receiver_limit_rejects_receiver_sixty_five() {
    let fixture = Fixture::new();
    let mut wakes = Vec::new();
    for _ in 0..MAX_EVENT_WAKE_RECEIVERS {
        wakes.push(fixture.writer.open_event_wake().expect("open wake").wake);
    }

    let error = fixture
        .writer
        .open_event_wake()
        .expect_err("receiver 65 fails");

    assert!(matches!(error, EventWakeError::ReceiverLimit));
}

#[test]
fn receiver_slots_reuse_after_drop_without_fanout() {
    let fixture = Fixture::new();
    let mut wakes = Vec::new();
    for _ in 0..MAX_EVENT_WAKE_RECEIVERS {
        wakes.push(fixture.writer.open_event_wake().expect("open wake").wake);
    }
    drop(wakes);

    for _ in 0..MAX_EVENT_WAKE_RECEIVERS {
        fixture.writer.open_event_wake().expect("reopen wake");
    }
}

#[test]
fn single_dropped_receiver_slot_reuses_immediately_without_fanout() {
    let fixture = Fixture::new();
    let mut wakes = Vec::new();
    for _ in 0..MAX_EVENT_WAKE_RECEIVERS {
        wakes.push(fixture.writer.open_event_wake().expect("open wake").wake);
    }
    drop(wakes.pop());

    fixture
        .writer
        .open_event_wake()
        .expect("reuse one dropped slot");
}

#[test]
fn pending_advanced_is_observed_before_closed() {
    let fixture = Fixture::new();
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;

    fixture
        .writer
        .append(&[durable_event("pending")])
        .expect("append");
    drop(fixture.writer);

    assert_eq!(wake.try_recv(), EventWakePoll::Advanced);
    assert_eq!(wake.try_recv(), EventWakePoll::Closed);
    assert_eq!(wake.recv(), EventWakeRecv::Closed);
}

#[test]
fn writer_drop_wakes_blocked_recv_with_closed() {
    let fixture = Fixture::new();
    let wake = fixture.writer.open_event_wake().expect("open wake").wake;
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut wake = wake;
        tx.send(wake.recv()).expect("send recv result");
        wake
    });

    thread::sleep(Duration::from_millis(20));
    drop(fixture.writer);

    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1))
            .expect("recv result"),
        EventWakeRecv::Closed
    );
    let mut wake = handle.join().expect("join receiver");
    assert_eq!(wake.try_recv(), EventWakePoll::Closed);
}

#[test]
fn multiple_blocked_receivers_wake_on_writer_drop() {
    let fixture = Fixture::new();
    let mut receivers = Vec::new();
    for _ in 0..4 {
        receivers.push(fixture.writer.open_event_wake().expect("open wake").wake);
    }
    let (tx, rx) = mpsc::channel();
    let handles = receivers
        .into_iter()
        .map(|mut wake| {
            let tx = tx.clone();
            thread::spawn(move || tx.send(wake.recv()).expect("send result"))
        })
        .collect::<Vec<_>>();
    drop(tx);

    thread::sleep(Duration::from_millis(20));
    drop(fixture.writer);

    let mut results = Vec::new();
    for _ in 0..4 {
        results.push(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("recv close result"),
        );
    }
    assert_eq!(results, vec![EventWakeRecv::Closed; 4]);
    for handle in handles {
        handle.join().expect("join receiver");
    }
}

#[test]
fn closed_state_is_terminal_for_try_recv_and_recv() {
    let fixture = Fixture::new();
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;
    drop(fixture.writer);

    assert_eq!(wake.try_recv(), EventWakePoll::Closed);
    assert_eq!(wake.try_recv(), EventWakePoll::Closed);
    assert_eq!(wake.recv(), EventWakeRecv::Closed);
}

#[test]
fn spurious_advanced_can_lead_to_zero_result_query() {
    let fixture = Fixture::new();
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;
    let event = durable_event("one");
    fixture
        .writer
        .append(std::slice::from_ref(&event))
        .expect("append");
    let cursor = query_after(fixture.log(), None, 10)
        .last()
        .expect("head event")
        .id
        .clone();

    assert_eq!(wake.try_recv(), EventWakePoll::Advanced);
    let events = query_after(fixture.log(), Some(cursor), 10);
    assert!(events.is_empty());
}

#[test]
fn query_to_head_then_append_then_recv_returns_advanced() {
    let fixture = Fixture::new();
    let first = durable_event("first");
    fixture
        .writer
        .append(std::slice::from_ref(&first))
        .expect("append first");
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;
    assert!(query_after(fixture.log(), Some(first.id.clone()), 10).is_empty());

    fixture
        .writer
        .append(&[durable_event("second")])
        .expect("append");

    assert_eq!(wake.recv(), EventWakeRecv::Advanced);
}

#[test]
fn catch_up_loop_after_wake_reaches_new_events_with_small_pages() {
    let fixture = Fixture::new();
    let initial = durable_event("initial");
    fixture
        .writer
        .append(std::slice::from_ref(&initial))
        .expect("append initial");
    let mut wake = fixture.writer.open_event_wake().expect("open wake").wake;
    let later = [durable_event("later one"), durable_event("later two")];
    fixture.writer.append(&later).expect("append later");

    assert_eq!(wake.try_recv(), EventWakePoll::Advanced);
    let events = catch_up_ids(fixture.log(), Some(initial.id), 1);

    assert_eq!(events, vec![later[0].id.as_str(), later[1].id.as_str()]);
}

#[test]
fn registration_racing_with_persist_has_baseline_or_wake_coverage() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(log).expect("writer"));
    let event = durable_event("racing append");
    let event_id = event.id.clone();
    let appender = {
        let writer = Arc::clone(&writer);
        thread::spawn(move || writer.append(&[event]).expect("append"))
    };

    let registration = writer.open_event_wake().expect("open wake");
    let mut wake = registration.wake;
    appender.join().expect("join append");

    let covered_by_baseline = registration.baseline_event_id.as_deref() == Some(event_id.as_str());
    let covered_by_wake = wake.try_recv() == EventWakePoll::Advanced;
    assert!(covered_by_baseline || covered_by_wake);
}

#[test]
fn runtime_only_registration_race_does_not_advance_durable_head() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(log).expect("writer"));
    let appender = {
        let writer = Arc::clone(&writer);
        thread::spawn(move || writer.append(&[runtime_delta("skip")]).expect("append"))
    };

    let registration = writer.open_event_wake().expect("open wake");
    let mut wake = registration.wake;
    appender.join().expect("join append");

    assert_eq!(registration.baseline_event_id, None);
    assert_eq!(wake.try_recv(), EventWakePoll::Empty);
}

#[test]
fn open_drop_stress_against_concurrent_append() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("events.jsonl");
    let writer = Arc::new(ProvenanceWriter::new(log).expect("writer"));
    let appender = {
        let writer = Arc::clone(&writer);
        thread::spawn(move || {
            for index in 0..128 {
                writer
                    .append(&[durable_event(&format!("event {index}"))])
                    .expect("append");
            }
        })
    };

    for _ in 0..128 {
        let wake = writer.open_event_wake().expect("open wake").wake;
        drop(wake);
    }

    appender.join().expect("join appender");
}

#[test]
fn session_event_wake_is_send_and_moves_to_thread() {
    assert_send::<SessionEventWake>();
    let fixture = Fixture::new();
    let wake = fixture.writer.open_event_wake().expect("open wake").wake;
    let handle = thread::spawn(move || {
        let mut wake = wake;
        wake.try_recv()
    });

    assert_eq!(handle.join().expect("join"), EventWakePoll::Empty);
}

#[test]
fn session_without_provenance_writer_fails_clearly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session = Session::new(
        SessionConfig::new(temp.path()),
        ScriptedProvider::new(Vec::new()),
        ScriptedDecider::new(Vec::new()),
    );

    let error = session.open_event_wake().expect_err("no writer");

    assert!(matches!(error, SessionError::EventWakeUnavailable));
}

struct Fixture {
    _temp: tempfile::TempDir,
    log: PathBuf,
    writer: ProvenanceWriter,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let log = temp.path().join("events.jsonl");
        let writer = ProvenanceWriter::new(log.clone()).expect("writer");
        Self {
            _temp: temp,
            log,
            writer,
        }
    }

    fn log(&self) -> &Path {
        &self.log
    }
}

fn durable_event(content: &str) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::USER_MESSAGE,
        object([("content", content.into())]),
    )
}

fn runtime_delta(content: &str) -> EventEnvelope {
    EventEnvelope::new(
        "session",
        "agent",
        None,
        EventKind::MODEL_DELTA,
        object([("kind", "text".into()), ("delta", content.into())]),
    )
}

fn query_after(log: &Path, after_event_id: Option<String>, limit: usize) -> Vec<EventEnvelope> {
    let mut query = ProvenanceQuery::new(limit);
    query.after_event_id = after_event_id;
    query_provenance(log, query)
        .expect("query provenance")
        .events
}

fn catch_up_ids(log: &Path, mut after_event_id: Option<String>, limit: usize) -> Vec<String> {
    let mut ids = Vec::new();
    loop {
        let mut query = ProvenanceQuery::new(limit);
        query.after_event_id = after_event_id;
        let page = query_provenance(log, query).expect("query provenance");
        ids.extend(page.events.iter().map(|event| event.id.clone()));
        match page.next_after_event_id {
            Some(next) => after_event_id = Some(next),
            None => return ids,
        }
    }
}

fn event_ids(events: &[EventEnvelope]) -> Vec<&str> {
    events.iter().map(|event| event.id.as_str()).collect()
}

fn assert_send<T: Send>() {}
