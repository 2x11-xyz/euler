//! Durable checkpoints for streamed root-assistant text.
//!
//! Provider events remain semantic-only. This module folds only canonical
//! session events and owns the response checkpoint grammar shared by live
//! recording, resume validation, and crash recovery.

use euler_event::{EventEnvelope, EventKind};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use thiserror::Error;

/// One checkpoint event never owns more than this many UTF-8 bytes.
pub const MAX_RESPONSE_CHUNK_BYTES: usize = 16 * 1024;

/// An active stream opportunistically checkpoints on the first text delta,
/// whenever the pending suffix reaches the byte bound, and on the first later
/// text delta after this interval. The interval is not a timer: a stream
/// blocked inside `next()` cannot flush until it yields or terminalizes.
pub const RESPONSE_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssistantResponseStatus {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl AssistantResponseStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResponseChunk {
    pub(crate) sequence: u64,
    pub(crate) content: String,
    pub(crate) observed_output_bytes: u64,
    pub(crate) retained_content_bytes: u64,
}

/// Process-local accumulator for one root `model.call`.
///
/// It contains text only. Reasoning and tool-call events never enter this
/// state. A checkpoint producer must persist returned chunks before forwarding
/// the corresponding text delta to a live UI.
pub(crate) struct ResponseCheckpoint {
    response_id: String,
    pending: String,
    next_sequence: u64,
    persisted_bytes: u64,
    observed_bytes: u64,
    last_checkpoint: Option<Instant>,
}

impl ResponseCheckpoint {
    pub(crate) fn new(response_id: String) -> Self {
        Self {
            response_id,
            pending: String::new(),
            next_sequence: 0,
            persisted_bytes: 0,
            observed_bytes: 0,
            last_checkpoint: None,
        }
    }

    pub(crate) fn response_id(&self) -> &str {
        &self.response_id
    }

    pub(crate) fn observed_bytes(&self) -> u64 {
        self.observed_bytes
    }

    pub(crate) fn has_text(&self) -> bool {
        self.observed_bytes > 0
    }

    pub(crate) fn observe_text(
        &mut self,
        delta: &str,
        now: Instant,
    ) -> Result<Vec<ResponseChunk>, ResponseCheckpointError> {
        if delta.is_empty() {
            return Ok(Vec::new());
        }
        let delta_bytes = u64::try_from(delta.len()).map_err(|_| ResponseCheckpointError)?;
        self.observed_bytes = self
            .observed_bytes
            .checked_add(delta_bytes)
            .ok_or(ResponseCheckpointError)?;
        self.pending.push_str(delta);

        let first = self.next_sequence == 0;
        let interval_elapsed = self.last_checkpoint.is_some_and(|last| {
            now.saturating_duration_since(last) >= RESPONSE_CHECKPOINT_INTERVAL
        });
        let force_suffix = first || interval_elapsed;
        let chunks = self.drain_chunks(force_suffix)?;
        if !chunks.is_empty() {
            self.last_checkpoint = Some(now);
        }
        Ok(chunks)
    }

    /// Flush every observed text byte before a handled terminal event.
    pub(crate) fn flush(
        &mut self,
        now: Instant,
    ) -> Result<Vec<ResponseChunk>, ResponseCheckpointError> {
        let chunks = self.drain_chunks(true)?;
        if !chunks.is_empty() {
            self.last_checkpoint = Some(now);
        }
        Ok(chunks)
    }

    fn drain_chunks(
        &mut self,
        force_suffix: bool,
    ) -> Result<Vec<ResponseChunk>, ResponseCheckpointError> {
        let mut chunks = Vec::new();
        let pending = std::mem::take(&mut self.pending);
        let mut offset = 0;
        while pending.len().saturating_sub(offset) >= MAX_RESPONSE_CHUNK_BYTES
            || (force_suffix && offset < pending.len())
        {
            let remaining = &pending[offset..];
            let take = if remaining.len() > MAX_RESPONSE_CHUNK_BYTES {
                utf8_prefix_boundary(remaining, MAX_RESPONSE_CHUNK_BYTES)
            } else {
                remaining.len()
            };
            if take == 0 {
                return Err(ResponseCheckpointError);
            }
            let content = remaining[..take].to_owned();
            let chunk_bytes = u64::try_from(content.len()).map_err(|_| ResponseCheckpointError)?;
            self.persisted_bytes = self
                .persisted_bytes
                .checked_add(chunk_bytes)
                .ok_or(ResponseCheckpointError)?;
            chunks.push(ResponseChunk {
                sequence: self.next_sequence,
                content,
                observed_output_bytes: self.persisted_bytes,
                retained_content_bytes: self.persisted_bytes,
            });
            self.next_sequence = self
                .next_sequence
                .checked_add(1)
                .ok_or(ResponseCheckpointError)?;
            offset += take;
            if !force_suffix && pending.len() - offset < MAX_RESPONSE_CHUNK_BYTES {
                break;
            }
        }
        self.pending.push_str(&pending[offset..]);
        Ok(chunks)
    }
}

fn utf8_prefix_boundary(value: &str, max_bytes: usize) -> usize {
    let mut boundary = max_bytes.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

#[derive(Clone, Copy, Debug, Error)]
#[error("assistant response checkpoint byte accounting overflow")]
pub(crate) struct ResponseCheckpointError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OpenResponseDraft {
    pub(crate) response_id: String,
    pub(crate) observed_output_bytes: u64,
    pub(crate) retained_content_bytes: u64,
}

#[derive(Debug, Error)]
pub enum AssistantResponseProtocolError {
    #[error("response projection encountered duplicate event id {event_id}")]
    DuplicateEventId { event_id: String },
    #[error("response chunk {event_id} has invalid field {field}")]
    InvalidField {
        event_id: String,
        field: &'static str,
    },
    #[error("response chunk {event_id} references unknown or ineligible model call {response_id}")]
    UnknownResponse {
        event_id: String,
        response_id: String,
    },
    #[error("response {response_id} has non-contiguous chunk sequence at {event_id}")]
    Sequence {
        event_id: String,
        response_id: String,
    },
    #[error("response {response_id} has inconsistent byte accounting at {event_id}")]
    ByteCount {
        event_id: String,
        response_id: String,
    },
    #[error("response {response_id} contains a chunk after its canonical terminal")]
    ChunkAfterTerminal { response_id: String },
    #[error("response terminal {event_id} has invalid status or association")]
    InvalidTerminal { event_id: String },
    #[error("response {response_id} has more than one canonical terminal")]
    DuplicateTerminal { response_id: String },
}

#[derive(Clone, Debug)]
struct CallIdentity {
    session: String,
    agent: String,
    root_driver: bool,
}

#[derive(Clone, Debug)]
struct DriverSnapshotIdentity {
    event_id: String,
    canvas_items: Option<u64>,
}

#[derive(Clone, Debug, Default)]
struct DraftFold {
    next_sequence: u64,
    observed_output_bytes: u64,
    retained_content_bytes: u64,
    content: String,
    terminal: Option<AssistantResponseStatus>,
}

/// Canonical replay projection for a terminalized root-assistant draft.
///
/// The text is never model canvas input. UI readers may present failed,
/// cancelled, or interrupted values as recoverable transcript history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssistantResponseTerminal {
    pub response_id: String,
    pub status: AssistantResponseStatus,
    pub content: String,
    pub observed_output_bytes: u64,
    pub retained_content_bytes: u64,
    pub source: String,
    pub message: String,
}

/// One owner for the streamed-response event grammar.
///
/// Both resume and UI replay feed events through this state. That prevents a
/// permissive transcript-only fold from rendering malformed, crossed-actor,
/// or companion output that resume would reject.
#[derive(Clone, Debug)]
pub struct AssistantResponseProjection {
    seen_event_ids: HashSet<String>,
    latest_driver_snapshots: HashMap<(String, String), DriverSnapshotIdentity>,
    calls: HashMap<String, CallIdentity>,
    drafts: HashMap<String, DraftFold>,
    retain_content: bool,
}

impl Default for AssistantResponseProjection {
    fn default() -> Self {
        Self {
            seen_event_ids: HashSet::new(),
            latest_driver_snapshots: HashMap::new(),
            calls: HashMap::new(),
            drafts: HashMap::new(),
            retain_content: true,
        }
    }
}

impl AssistantResponseProjection {
    pub(crate) fn validation_only() -> Self {
        Self {
            retain_content: false,
            ..Self::default()
        }
    }

    /// Fold one canonical event, returning a response only at its semantic
    /// terminal. An error means the accepted prefix is incompatible; callers
    /// must not continue projecting response text from that prefix.
    pub fn ingest(
        &mut self,
        event: &EventEnvelope,
    ) -> Result<Option<AssistantResponseTerminal>, AssistantResponseProtocolError> {
        if !self.seen_event_ids.insert(event.id.clone()) {
            return Err(AssistantResponseProtocolError::DuplicateEventId {
                event_id: event.id.clone(),
            });
        }
        if event.kind.as_str() == EventKind::CANVAS_SNAPSHOT {
            if !event.payload.contains_key("purpose") {
                self.latest_driver_snapshots.insert(
                    (event.session.clone(), event.agent.clone()),
                    DriverSnapshotIdentity {
                        event_id: event.id.clone(),
                        canvas_items: crate::canvas::driver_snapshot_selection(event)
                            .map(|(canvas_items, _)| canvas_items),
                    },
                );
            }
            return Ok(None);
        }
        if event.kind.as_str() == EventKind::MODEL_CALL {
            let root_driver = self.model_call_has_driver_authority(event);
            self.calls.insert(
                event.id.clone(),
                CallIdentity {
                    session: event.session.clone(),
                    agent: event.agent.clone(),
                    root_driver,
                },
            );
            return Ok(None);
        }
        if event.kind.as_str() == EventKind::ASSISTANT_RESPONSE_CHUNK {
            self.fold_chunk(event)?;
            return Ok(None);
        }
        if matches!(
            event.kind.as_str(),
            EventKind::MODEL_RESULT | EventKind::ERROR
        ) {
            if event.payload.get("response_id").is_some() {
                return self.fold_terminal(event).map(Some);
            }
            if event
                .parent
                .as_deref()
                .is_some_and(|parent| self.drafts.contains_key(parent))
            {
                return Err(AssistantResponseProtocolError::InvalidTerminal {
                    event_id: event.id.clone(),
                });
            }
        }
        Ok(None)
    }

    fn model_call_has_driver_authority(&self, event: &EventEnvelope) -> bool {
        if event.payload.contains_key("purpose") {
            return false;
        }
        let Some(snapshot) = self
            .latest_driver_snapshots
            .get(&(event.session.clone(), event.agent.clone()))
        else {
            return false;
        };
        let Some(canvas_items) = snapshot.canvas_items else {
            return false;
        };
        event
            .payload
            .get("canvas_snapshot_id")
            .and_then(Value::as_str)
            == Some(snapshot.event_id.as_str())
            && event.payload.get("canvas_items").and_then(Value::as_u64) == Some(canvas_items)
    }

    fn fold_chunk(&mut self, event: &EventEnvelope) -> Result<(), AssistantResponseProtocolError> {
        let response_id = payload_nonempty(event, "response_id")?;
        let call = self.calls.get(response_id).filter(|call| {
            call.root_driver && call.session == event.session && call.agent == event.agent
        });
        if call.is_none() {
            return Err(AssistantResponseProtocolError::UnknownResponse {
                event_id: event.id.clone(),
                response_id: response_id.to_owned(),
            });
        }
        let sequence = event
            .payload
            .get("sequence")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_field(event, "sequence"))?;
        let content = payload_nonempty(event, "content")?;
        if content.len() > MAX_RESPONSE_CHUNK_BYTES {
            return Err(invalid_field(event, "content"));
        }
        let observed = event
            .payload
            .get("observed_output_bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_field(event, "observed_output_bytes"))?;
        let retained = event
            .payload
            .get("retained_content_bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_field(event, "retained_content_bytes"))?;
        let draft = self.drafts.entry(response_id.to_owned()).or_default();
        if draft.terminal.is_some() {
            return Err(AssistantResponseProtocolError::ChunkAfterTerminal {
                response_id: response_id.to_owned(),
            });
        }
        if sequence != draft.next_sequence {
            return Err(AssistantResponseProtocolError::Sequence {
                event_id: event.id.clone(),
                response_id: response_id.to_owned(),
            });
        }
        if observed <= draft.observed_output_bytes {
            return Err(AssistantResponseProtocolError::ByteCount {
                event_id: event.id.clone(),
                response_id: response_id.to_owned(),
            });
        }
        let expected_retained = draft
            .retained_content_bytes
            .checked_add(u64::try_from(content.len()).map_err(|_| {
                AssistantResponseProtocolError::ByteCount {
                    event_id: event.id.clone(),
                    response_id: response_id.to_owned(),
                }
            })?)
            .ok_or_else(|| AssistantResponseProtocolError::ByteCount {
                event_id: event.id.clone(),
                response_id: response_id.to_owned(),
            })?;
        if retained != expected_retained {
            return Err(AssistantResponseProtocolError::ByteCount {
                event_id: event.id.clone(),
                response_id: response_id.to_owned(),
            });
        }
        if self.retain_content {
            draft.content.push_str(content);
        }
        draft.next_sequence = draft
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| invalid_field(event, "sequence"))?;
        draft.observed_output_bytes = observed;
        draft.retained_content_bytes = retained;
        Ok(())
    }

    fn fold_terminal(
        &mut self,
        event: &EventEnvelope,
    ) -> Result<AssistantResponseTerminal, AssistantResponseProtocolError> {
        let response_id = payload_nonempty(event, "response_id")?;
        let Some(call) = self.calls.get(response_id) else {
            return Err(AssistantResponseProtocolError::InvalidTerminal {
                event_id: event.id.clone(),
            });
        };
        if !call.root_driver
            || call.session != event.session
            || call.agent != event.agent
            || event.parent.as_deref() != Some(response_id)
        {
            return Err(AssistantResponseProtocolError::InvalidTerminal {
                event_id: event.id.clone(),
            });
        }
        let Some(draft) = self.drafts.get_mut(response_id) else {
            return Err(AssistantResponseProtocolError::InvalidTerminal {
                event_id: event.id.clone(),
            });
        };
        if draft.terminal.is_some() {
            return Err(AssistantResponseProtocolError::DuplicateTerminal {
                response_id: response_id.to_owned(),
            });
        }
        let status = terminal_status(event).ok_or_else(|| {
            AssistantResponseProtocolError::InvalidTerminal {
                event_id: event.id.clone(),
            }
        })?;
        let observed = event
            .payload
            .get("observed_output_bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_field(event, "observed_output_bytes"))?;
        let retained = event
            .payload
            .get("retained_content_bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_field(event, "retained_content_bytes"))?;
        if observed != draft.observed_output_bytes || retained != draft.retained_content_bytes {
            return Err(AssistantResponseProtocolError::ByteCount {
                event_id: event.id.clone(),
                response_id: response_id.to_owned(),
            });
        }
        draft.terminal = Some(status);
        Ok(AssistantResponseTerminal {
            response_id: response_id.to_owned(),
            status,
            content: std::mem::take(&mut draft.content),
            observed_output_bytes: observed,
            retained_content_bytes: retained,
            source: event
                .payload
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            message: event
                .payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }

    fn open_drafts(&self) -> Vec<OpenResponseDraft> {
        let mut open = self
            .drafts
            .iter()
            .filter(|(_, draft)| draft.terminal.is_none())
            .map(|(response_id, draft)| OpenResponseDraft {
                response_id: response_id.clone(),
                observed_output_bytes: draft.observed_output_bytes,
                retained_content_bytes: draft.retained_content_bytes,
            })
            .collect::<Vec<_>>();
        open.sort_by(|left, right| left.response_id.cmp(&right.response_id));
        open
    }
}

/// Validate a complete event slice and index every canonical response
/// terminal by its terminal event id. No partial result is returned for an
/// incompatible slice.
pub fn project_assistant_response_terminals(
    events: &[EventEnvelope],
) -> Result<HashMap<String, AssistantResponseTerminal>, AssistantResponseProtocolError> {
    let mut projection = AssistantResponseProjection::default();
    let mut terminals = HashMap::new();
    for event in events {
        if let Some(terminal) = projection.ingest(event)? {
            terminals.insert(event.id.clone(), terminal);
        }
    }
    Ok(terminals)
}

/// Validate the checkpoint protocol and return every draft whose root model
/// call lacks a canonical `model.result` or terminal `error`.
pub(crate) fn validate_and_find_open_drafts(
    events: &[EventEnvelope],
) -> Result<Vec<OpenResponseDraft>, AssistantResponseProtocolError> {
    let mut projection = AssistantResponseProjection::validation_only();
    for event in events {
        projection.ingest(event)?;
    }
    Ok(projection.open_drafts())
}

fn terminal_status(event: &EventEnvelope) -> Option<AssistantResponseStatus> {
    let recorded = event.payload.get("response_status")?.as_str()?;
    let expected = match event.kind.as_str() {
        EventKind::MODEL_RESULT => AssistantResponseStatus::Completed,
        EventKind::ERROR
            if event.payload.get("cancelled").and_then(Value::as_bool) == Some(true) =>
        {
            AssistantResponseStatus::Cancelled
        }
        EventKind::ERROR
            if event
                .payload
                .get("recovery_closure")
                .and_then(Value::as_bool)
                == Some(true) =>
        {
            AssistantResponseStatus::Interrupted
        }
        EventKind::ERROR
            if event.payload.get("source").and_then(Value::as_str) == Some("provider") =>
        {
            AssistantResponseStatus::Failed
        }
        _ => return None,
    };
    (recorded == expected.as_str()).then_some(expected)
}

fn payload_nonempty<'a>(
    event: &'a EventEnvelope,
    field: &'static str,
) -> Result<&'a str, AssistantResponseProtocolError> {
    event
        .payload
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_field(event, field))
}

fn invalid_field(event: &EventEnvelope, field: &'static str) -> AssistantResponseProtocolError {
    AssistantResponseProtocolError::InvalidField {
        event_id: event.id.clone(),
        field,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_event::{object, JsonObject};

    fn event(
        id: &str,
        agent: &str,
        parent: Option<&str>,
        kind: &'static str,
        payload: JsonObject,
    ) -> EventEnvelope {
        let mut event =
            EventEnvelope::new("session", agent, parent.map(str::to_owned), kind, payload);
        event.id = id.to_owned();
        event
    }

    fn response_prefix(agent: &str) -> Vec<EventEnvelope> {
        vec![
            event("start", "root", None, EventKind::SESSION_START, object([])),
            event(
                "snapshot",
                "root",
                Some("start"),
                EventKind::CANVAS_SNAPSHOT,
                object([
                    ("selected_event_ids", serde_json::json!([])),
                    ("counts", serde_json::json!({"items": 0})),
                ]),
            ),
            event(
                "call",
                agent,
                Some("snapshot"),
                EventKind::MODEL_CALL,
                object([
                    ("canvas_snapshot_id", "snapshot".into()),
                    ("canvas_items", 0.into()),
                ]),
            ),
            event(
                "chunk",
                agent,
                Some("call"),
                EventKind::ASSISTANT_RESPONSE_CHUNK,
                object([
                    ("response_id", "call".into()),
                    ("sequence", 0.into()),
                    ("content", "kept text".into()),
                    ("observed_output_bytes", 9.into()),
                    ("retained_content_bytes", 9.into()),
                ]),
            ),
        ]
    }

    fn failed_terminal(agent: &str) -> EventEnvelope {
        event(
            "error",
            agent,
            Some("call"),
            EventKind::ERROR,
            object([
                ("source", "provider".into()),
                ("message", "stream closed".into()),
                ("response_id", "call".into()),
                ("response_status", "failed".into()),
                ("observed_output_bytes", 9.into()),
                ("retained_content_bytes", 9.into()),
            ]),
        )
    }

    #[test]
    fn utf8_chunks_are_bounded_without_splitting_codepoints() {
        let mut checkpoint = ResponseCheckpoint::new("response".to_owned());
        let text = "a".repeat(MAX_RESPONSE_CHUNK_BYTES - 1) + "🦀";
        let chunks = checkpoint
            .observe_text(&text, Instant::now())
            .expect("checkpoint");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].content.len(), MAX_RESPONSE_CHUNK_BYTES - 1);
        assert_eq!(chunks[1].content, "🦀");
        assert_eq!(chunks[1].observed_output_bytes, text.len() as u64);
    }

    #[test]
    fn first_text_is_immediate_and_later_text_is_opportunistically_coalesced() {
        let start = Instant::now();
        let mut checkpoint = ResponseCheckpoint::new("response".to_owned());
        assert_eq!(
            checkpoint
                .observe_text("first", start)
                .expect("first checkpoint")
                .len(),
            1
        );
        assert!(checkpoint
            .observe_text(" pending", start + Duration::from_millis(999))
            .expect("pending suffix")
            .is_empty());
        let chunks = checkpoint
            .observe_text(" trigger", start + Duration::from_secs(1))
            .expect("interval checkpoint");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, " pending trigger");
    }

    #[test]
    fn shared_projection_returns_failed_root_text_only_at_terminal() {
        let mut events = response_prefix("root");
        events.insert(
            4,
            event(
                "reasoning",
                "root",
                Some("chunk"),
                EventKind::MODEL_REASONING,
                object([("content", "private reasoning".into())]),
            ),
        );
        events.push(failed_terminal("root"));

        let projected = project_assistant_response_terminals(&events).expect("valid response");
        assert_eq!(
            projected.get("error"),
            Some(&AssistantResponseTerminal {
                response_id: "call".to_owned(),
                status: AssistantResponseStatus::Failed,
                content: "kept text".to_owned(),
                observed_output_bytes: 9,
                retained_content_bytes: 9,
                source: "provider".to_owned(),
                message: "stream closed".to_owned(),
            })
        );
        assert!(!projected["error"].content.contains("private reasoning"));
    }

    #[test]
    fn shared_projection_rejects_non_driver_or_cross_agent_chunks() {
        let mut child = response_prefix("child");
        child.push(failed_terminal("child"));
        assert!(matches!(
            project_assistant_response_terminals(&child),
            Err(AssistantResponseProtocolError::UnknownResponse { .. })
        ));

        let mut crossed = response_prefix("root");
        crossed[3].agent = "other".to_owned();
        crossed.push(failed_terminal("root"));
        assert!(matches!(
            project_assistant_response_terminals(&crossed),
            Err(AssistantResponseProtocolError::UnknownResponse { .. })
        ));
    }

    #[test]
    fn request_backed_snapshot_authority_survives_a_legacy_root_actor_change() {
        let mut events = response_prefix("root");
        events[0].agent = "legacy-root".to_owned();
        events.push(failed_terminal("root"));

        let projected =
            project_assistant_response_terminals(&events).expect("resumed root response");
        assert_eq!(projected["error"].content, "kept text");
    }

    #[test]
    fn shared_projection_rejects_noncontiguous_or_oversized_chunks() {
        let mut noncontiguous = response_prefix("root");
        noncontiguous[3]
            .payload
            .insert("sequence".to_owned(), 1.into());
        assert!(matches!(
            project_assistant_response_terminals(&noncontiguous),
            Err(AssistantResponseProtocolError::Sequence { .. })
        ));

        let mut oversized = response_prefix("root");
        let content = "x".repeat(MAX_RESPONSE_CHUNK_BYTES + 1);
        oversized[3]
            .payload
            .insert("content".to_owned(), content.into());
        assert!(matches!(
            project_assistant_response_terminals(&oversized),
            Err(AssistantResponseProtocolError::InvalidField {
                field: "content",
                ..
            })
        ));
    }

    #[test]
    fn shared_projection_rejects_future_stale_and_forged_driver_snapshots() {
        let mut future = response_prefix("root");
        let snapshot = future.remove(1);
        future.insert(2, snapshot);
        future.push(failed_terminal("root"));
        assert!(matches!(
            project_assistant_response_terminals(&future),
            Err(AssistantResponseProtocolError::UnknownResponse { .. })
        ));

        let mut stale = response_prefix("root");
        stale.insert(
            2,
            event(
                "newer-snapshot",
                "root",
                Some("snapshot"),
                EventKind::CANVAS_SNAPSHOT,
                object([
                    ("selected_event_ids", serde_json::json!([])),
                    ("counts", serde_json::json!({"items": 0})),
                ]),
            ),
        );
        stale.push(failed_terminal("root"));
        assert!(matches!(
            project_assistant_response_terminals(&stale),
            Err(AssistantResponseProtocolError::UnknownResponse { .. })
        ));

        let mut forged_child = response_prefix("child");
        forged_child.push(failed_terminal("child"));
        assert!(matches!(
            project_assistant_response_terminals(&forged_child),
            Err(AssistantResponseProtocolError::UnknownResponse { .. })
        ));
    }

    #[test]
    fn shared_projection_rejects_duplicate_event_ids_even_after_a_terminal() {
        let mut events = response_prefix("root");
        events.push(failed_terminal("root"));
        let mut duplicate = event(
            "call",
            "root",
            None,
            EventKind::ASSISTANT_ACTIVITY,
            object([]),
        );
        duplicate.id = "call".to_owned();
        events.push(duplicate);

        assert!(matches!(
            project_assistant_response_terminals(&events),
            Err(AssistantResponseProtocolError::DuplicateEventId { .. })
        ));
    }
}
