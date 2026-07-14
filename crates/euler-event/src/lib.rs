//! Canonical session event envelope and initial event kinds.
//!
//! This crate intentionally contains data types and narrow JSON/RFC3339/ULID
//! handling only. Runtime policy, rendering, and provenance writing belong in
//! higher layers.
#![cfg_attr(test, allow(clippy::too_many_lines))] // unit-test exemption for inline test modules

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use ulid::Ulid;

pub type JsonObject = Map<String, Value>;
pub type JsonValue = Value;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub struct EventKind(String);

impl EventKind {
    pub const USER_MESSAGE: &'static str = "user.message";
    pub const ASSISTANT_MESSAGE: &'static str = "assistant.message";
    pub const ASSISTANT_ACTIVITY: &'static str = "assistant.activity";
    pub const PLAN_UPDATE: &'static str = "plan.update";
    pub const TOOL_CALL: &'static str = "tool.call";
    pub const TOOL_RESULT: &'static str = "tool.result";
    pub const PERMISSION_PROMPT: &'static str = "permission.prompt";
    pub const PERMISSION_DECISION: &'static str = "permission.decision";
    pub const PATCH_PROPOSED: &'static str = "patch.proposed";
    pub const PATCH_APPLIED: &'static str = "patch.applied";
    pub const FILE_CHANGE: &'static str = "file.change";
    pub const FILE_DIFF: &'static str = "file.diff";
    pub const WORKSPACE_RESTORE: &'static str = "workspace.restore";
    pub const CHECK_STARTED: &'static str = "check.started";
    pub const CHECK_RESULT: &'static str = "check.result";
    pub const MODEL_CALL: &'static str = "model.call";
    pub const MODEL_RESULT: &'static str = "model.result";
    pub const MODEL_REASONING: &'static str = "model.reasoning";
    pub const MODEL_DELTA: &'static str = "model.delta";
    pub const MODEL_SWITCHED: &'static str = "model.switched";
    pub const MODEL_EFFORT_CHANGED: &'static str = "model.effort.changed";
    pub const CONTEXT_LIMIT: &'static str = "context.limit";
    pub const CONTEXT_SLOT_UPDATED: &'static str = "context.slot.updated";
    pub const CANVAS_SNAPSHOT: &'static str = "canvas.snapshot";
    pub const CANVAS_POLICY_CHANGED: &'static str = "canvas.policy.changed";
    pub const CANVAS_SWAP: &'static str = "canvas.swap";
    pub const CANVAS_CANDIDATE_DISCARDED: &'static str = "canvas.candidate.discarded";
    pub const SECRET_REDACTED: &'static str = "secret.redacted";
    /// A credential shape was detected in a faithful tool-call argument.
    /// Read-only marker: the payload is NOT modified.
    /// Carries shape labels + a pointer to the exposing event, never the value.
    pub const SECRET_EXPOSURE_DETECTED: &'static str = "secret.exposure.detected";
    /// A user-initiated scrub removed a value from every session-owned
    /// persistent surface. Audit only: carries counts, never the value.
    pub const SECRET_SCRUBBED: &'static str = "secret.scrubbed";
    pub const EXTENSION_ARTIFACT: &'static str = "extension.artifact";
    pub const AGENT_SPAWN: &'static str = "agent.spawn";
    pub const AGENT_MESSAGE: &'static str = "agent.message";
    pub const AGENT_RESULT: &'static str = "agent.result";
    pub const SESSION_START: &'static str = "session.start";
    /// A durable marker appended at a resume boundary (issue #6): records that
    /// the session lifetime was continued, against which provider/model, and
    /// from which tail event. Makes resumed lifetimes auditable in provenance.
    pub const SESSION_RESUMED: &'static str = "session.resumed";
    pub const SESSION_RENAMED: &'static str = "session.renamed";
    pub const SESSION_SUMMARY: &'static str = "session.summary";
    pub const ERROR: &'static str = "error";
    pub const ALL: &[&str] = &[
        Self::USER_MESSAGE,
        Self::ASSISTANT_MESSAGE,
        Self::ASSISTANT_ACTIVITY,
        Self::PLAN_UPDATE,
        Self::TOOL_CALL,
        Self::TOOL_RESULT,
        Self::PERMISSION_PROMPT,
        Self::PERMISSION_DECISION,
        Self::PATCH_PROPOSED,
        Self::PATCH_APPLIED,
        Self::FILE_CHANGE,
        Self::FILE_DIFF,
        Self::WORKSPACE_RESTORE,
        Self::CHECK_STARTED,
        Self::CHECK_RESULT,
        Self::MODEL_CALL,
        Self::MODEL_RESULT,
        Self::MODEL_REASONING,
        Self::MODEL_DELTA,
        Self::MODEL_SWITCHED,
        Self::MODEL_EFFORT_CHANGED,
        Self::CONTEXT_LIMIT,
        Self::CONTEXT_SLOT_UPDATED,
        Self::CANVAS_SNAPSHOT,
        Self::CANVAS_POLICY_CHANGED,
        Self::CANVAS_SWAP,
        Self::CANVAS_CANDIDATE_DISCARDED,
        Self::SECRET_REDACTED,
        Self::SECRET_EXPOSURE_DETECTED,
        Self::SECRET_SCRUBBED,
        Self::EXTENSION_ARTIFACT,
        Self::AGENT_SPAWN,
        Self::AGENT_MESSAGE,
        Self::AGENT_RESULT,
        Self::SESSION_START,
        Self::SESSION_RESUMED,
        Self::SESSION_RENAMED,
        Self::SESSION_SUMMARY,
        Self::ERROR,
    ];

    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for EventKind {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for EventKind {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct EventEnvelope {
    pub v: u16,
    pub id: String,
    pub ts: String,
    pub session: String,
    pub agent: String,
    pub parent: Option<String>,
    pub kind: EventKind,
    pub payload: JsonObject,
    pub blobs: BTreeMap<String, String>,
}

impl EventEnvelope {
    pub fn new(
        session: impl Into<String>,
        agent: impl Into<String>,
        parent: Option<String>,
        kind: impl Into<EventKind>,
        payload: JsonObject,
    ) -> Self {
        Self {
            v: 1,
            id: Ulid::new().to_string(),
            ts: now_rfc3339_millis(),
            session: session.into(),
            agent: agent.into(),
            parent,
            kind: kind.into(),
            payload,
            blobs: BTreeMap::new(),
        }
    }

    pub fn to_json_line(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    pub fn from_json_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

pub fn object(entries: impl IntoIterator<Item = (&'static str, JsonValue)>) -> JsonObject {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

pub fn now_rfc3339_millis() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_round_trip(kind: &'static str, payload: JsonObject) {
        let event = EventEnvelope::new("session", "agent", None, kind, payload);
        let json = event.to_json_line().expect("serialize event");
        let actual = EventEnvelope::from_json_line(&json).expect("deserialize event");
        assert_eq!(actual, event);
        assert_eq!(actual.v, 1);
        assert_eq!(actual.kind.as_str(), kind);
        assert_eq!(actual.id.len(), 26);
        assert!(actual.ts.ends_with('Z'));
    }

    #[test]
    fn round_trips_every_initial_event_kind() {
        assert_round_trip(
            EventKind::USER_MESSAGE,
            object([("content", "hello".into())]),
        );
        assert_round_trip(
            EventKind::ASSISTANT_MESSAGE,
            object([("content", "hi".into())]),
        );
        assert_round_trip(
            EventKind::ASSISTANT_ACTIVITY,
            object([("message", "working".into())]),
        );
        assert_round_trip(EventKind::PLAN_UPDATE, object([("summary", "plan".into())]));
        assert_round_trip(
            EventKind::TOOL_CALL,
            object([("name", "read".into()), ("input", "file".into())]),
        );
        assert_round_trip(
            EventKind::TOOL_RESULT,
            object([
                ("name", "read".into()),
                ("ok", true.into()),
                ("output", "contents".into()),
            ]),
        );
        assert_round_trip(
            EventKind::PERMISSION_PROMPT,
            object([("capability", "fs-write".into()), ("reason", "edit".into())]),
        );
        assert_round_trip(
            EventKind::PERMISSION_DECISION,
            object([("capability", "fs-write".into()), ("allowed", true.into())]),
        );
        assert_round_trip(
            EventKind::PATCH_PROPOSED,
            object([("path", "file".into()), ("diff", "---".into())]),
        );
        assert_round_trip(
            EventKind::PATCH_APPLIED,
            object([("path", "file".into()), ("diff", "+++".into())]),
        );
        assert_round_trip(
            EventKind::FILE_CHANGE,
            object([
                ("tool_call_id", "call-1".into()),
                ("origin", "edit_file".into()),
                ("action", "modify".into()),
                ("path", "file".into()),
                ("old_path", Value::Null),
                ("before_sha256", "sha-before".into()),
                ("after_sha256", "sha-after".into()),
                ("before_byte_len", 3.into()),
                ("after_byte_len", 4.into()),
                ("diff_redaction", "omitted".into()),
            ]),
        );
        assert_round_trip(
            EventKind::FILE_DIFF,
            object([
                ("tool_call_id", "call-1".into()),
                ("file_change_id", "evt-file-change".into()),
                ("path", "file".into()),
                ("old_path", Value::Null),
                ("action", "modify".into()),
                ("origin", "edit_file".into()),
                ("diff", "--- a/file\n+++ b/file\n".into()),
                ("truncated", false.into()),
                ("truncation", "none".into()),
                ("omitted_reason", Value::Null),
            ]),
        );
        assert_round_trip(
            EventKind::WORKSPACE_RESTORE,
            object([
                ("path", "file".into()),
                ("checkpoint_event_id", "evt-file-change".into()),
                ("blob_sha256", "sha-before".into()),
                ("restored", true.into()),
            ]),
        );
        assert_round_trip(
            EventKind::CHECK_STARTED,
            object([("name", "cargo test".into())]),
        );
        assert_round_trip(
            EventKind::CHECK_RESULT,
            object([
                ("name", "cargo test".into()),
                ("ok", true.into()),
                ("output", "ok".into()),
            ]),
        );
        assert_round_trip(
            EventKind::MODEL_CALL,
            object([
                ("provider", "fixture".into()),
                ("model", "echo".into()),
                ("prompt", "hello".into()),
            ]),
        );
        assert_round_trip(
            EventKind::MODEL_RESULT,
            object([
                ("provider", "fixture".into()),
                ("model", "echo".into()),
                ("content", "hello".into()),
            ]),
        );
        assert_round_trip(
            EventKind::CONTEXT_SLOT_UPDATED,
            object([
                ("extension_id", "slot-ext".into()),
                ("slot", "main".into()),
                ("content", "bounded context".into()),
            ]),
        );
        assert_round_trip(
            EventKind::CANVAS_SNAPSHOT,
            object([("summary", "selected".into())]),
        );
        assert_round_trip(
            EventKind::CANVAS_POLICY_CHANGED,
            object([
                ("automatic", true.into()),
                ("stubs", true.into()),
                ("budget_bytes", 640_000.into()),
            ]),
        );
        assert_round_trip(
            EventKind::CANVAS_SWAP,
            object([
                ("snapshot_start_id", "01J00000000000000000000001".into()),
                ("snapshot_end_id", "01J00000000000000000000002".into()),
                ("frontier_start_id", "01J00000000000000000000003".into()),
                ("policy_version", "1".into()),
                ("projection_schema_version", "1".into()),
                ("projection_blob", "summary".into()),
                ("validation_result", "pass".into()),
            ]),
        );
        assert_round_trip(
            EventKind::CANVAS_CANDIDATE_DISCARDED,
            object([
                ("reason", "snapshot end is not a safe boundary".into()),
                ("policy_version", "1".into()),
            ]),
        );
        assert_round_trip(
            EventKind::SECRET_REDACTED,
            object([("label", "token".into())]),
        );
        assert_round_trip(
            EventKind::EXTENSION_ARTIFACT,
            object([
                ("extension_id", "artifact-ext".into()),
                ("display_name", "Artifact".into()),
                ("media_type", "text/plain".into()),
                (
                    "path",
                    "sessions/session/extensions/artifact-ext/artifacts/abc".into(),
                ),
                ("sha256", "abc".into()),
                ("byte_len", 3.into()),
                ("source_event_ids", json!(["01J00000000000000000000000"])),
                ("metadata", json!({})),
            ]),
        );
        assert_round_trip(
            EventKind::AGENT_SPAWN,
            object([("agent", "child".into()), ("task", "review".into())]),
        );
        assert_round_trip(
            EventKind::AGENT_MESSAGE,
            object([
                ("from_agent_id", "child".into()),
                ("to_agent_id", "parent".into()),
                ("spawn_event_id", "01J00000000000000000000001".into()),
                ("queued_ts", "2026-06-29T21:44:00.000Z".into()),
                ("payload", json!({"status": "working"})),
            ]),
        );
        assert_round_trip(
            EventKind::AGENT_RESULT,
            object([("agent", "child".into()), ("result", "done".into())]),
        );
        assert_round_trip(
            EventKind::SESSION_START,
            object([("provider", "fixture".into()), ("model", "echo".into())]),
        );
        assert_round_trip(
            EventKind::SESSION_RENAMED,
            object([("name", "research branch".into())]),
        );
        assert_round_trip(
            EventKind::SESSION_SUMMARY,
            object([("summary", "done".into())]),
        );
        assert_round_trip(EventKind::ERROR, object([("message", "failed".into())]));
    }

    #[test]
    fn all_event_kinds_lists_every_kind_constant() {
        let constants = [
            EventKind::USER_MESSAGE,
            EventKind::ASSISTANT_MESSAGE,
            EventKind::ASSISTANT_ACTIVITY,
            EventKind::PLAN_UPDATE,
            EventKind::TOOL_CALL,
            EventKind::TOOL_RESULT,
            EventKind::PERMISSION_PROMPT,
            EventKind::PERMISSION_DECISION,
            EventKind::PATCH_PROPOSED,
            EventKind::PATCH_APPLIED,
            EventKind::FILE_CHANGE,
            EventKind::FILE_DIFF,
            EventKind::WORKSPACE_RESTORE,
            EventKind::CHECK_STARTED,
            EventKind::CHECK_RESULT,
            EventKind::MODEL_CALL,
            EventKind::MODEL_RESULT,
            EventKind::MODEL_REASONING,
            EventKind::MODEL_DELTA,
            EventKind::MODEL_SWITCHED,
            EventKind::MODEL_EFFORT_CHANGED,
            EventKind::CONTEXT_LIMIT,
            EventKind::CONTEXT_SLOT_UPDATED,
            EventKind::CANVAS_SNAPSHOT,
            EventKind::CANVAS_POLICY_CHANGED,
            EventKind::CANVAS_SWAP,
            EventKind::CANVAS_CANDIDATE_DISCARDED,
            EventKind::SECRET_REDACTED,
            EventKind::SECRET_EXPOSURE_DETECTED,
            EventKind::SECRET_SCRUBBED,
            EventKind::EXTENSION_ARTIFACT,
            EventKind::AGENT_SPAWN,
            EventKind::AGENT_MESSAGE,
            EventKind::AGENT_RESULT,
            EventKind::SESSION_START,
            EventKind::SESSION_RESUMED,
            EventKind::SESSION_RENAMED,
            EventKind::SESSION_SUMMARY,
            EventKind::ERROR,
        ];

        assert_eq!(EventKind::ALL.len(), constants.len());
        for kind in constants {
            assert!(EventKind::ALL.contains(&kind), "{kind} missing from ALL");
        }
    }

    #[test]
    fn unknown_event_kind_survives_round_trip() {
        let event = EventEnvelope::new(
            "session",
            "agent",
            None,
            "future.kind",
            object([("content", "kept".into())]),
        );
        let json = event.to_json_line().expect("serialize event");
        let actual = EventEnvelope::from_json_line(&json).expect("deserialize event");
        assert_eq!(actual, event);
        assert_eq!(actual.kind.as_str(), "future.kind");
    }

    #[test]
    fn ratified_event_fixtures_round_trip_exactly() {
        for line in ratified_fixture_lines() {
            let event = EventEnvelope::from_json_line(&line).expect("fixture parses");
            assert_eq!(event.to_json_line().expect("fixture serializes"), line);
            assert_ratified_fields_present(&event);
        }
    }

    fn ratified_fixture_lines() -> Vec<String> {
        let base = |kind: &str, payload: Value| {
            format!(
                "{{\"v\":1,\"id\":\"01J00000000000000000000000\",\"ts\":\"2026-06-11T00:00:00.000Z\",\"session\":\"session\",\"agent\":\"agent\",\"parent\":null,\"kind\":\"{kind}\",\"payload\":{payload},\"blobs\":{{}}}}"
            )
        };
        vec![
            base(EventKind::USER_MESSAGE, json!({"content": "hello"})),
            base(EventKind::ASSISTANT_MESSAGE, json!({"content": "hi"})),
            base(EventKind::ASSISTANT_ACTIVITY, json!({"message": "working"})),
            base(EventKind::PLAN_UPDATE, json!({"summary": "plan"})),
            base(
                EventKind::TOOL_CALL,
                json!({"id": "call-1", "name": "read_file", "input": {"path": "a.txt"}}),
            ),
            base(
                EventKind::TOOL_RESULT,
                json!({"id": "call-1", "name": "read_file", "ok": true, "output": "ok", "exit_code": 0}),
            ),
            base(
                EventKind::PERMISSION_PROMPT,
                json!({"capability": "fs-write", "reason": "tool edit_file"}),
            ),
            base(
                EventKind::PERMISSION_DECISION,
                json!({"capability": "fs-write", "mode": "ask", "allowed": true, "decision": "allowed"}),
            ),
            base(
                EventKind::PERMISSION_DECISION,
                json!({
                    "capability": "artifact-write",
                    "mode": "static-grant",
                    "allowed": true,
                    "decision": "allowed",
                    "source": "extension",
                    "extension_id": "fixture-ext",
                    "command": null
                }),
            ),
            base(
                EventKind::PATCH_PROPOSED,
                json!({"path": "a.txt", "old": "a", "new": "b"}),
            ),
            base(
                EventKind::PATCH_APPLIED,
                json!({"path": "a.txt", "old": "a", "new": "b"}),
            ),
            base(
                EventKind::FILE_CHANGE,
                json!({
                    "tool_call_id": "call-1",
                    "origin": "edit_file",
                    "action": "modify",
                    "path": "a.txt",
                    "old_path": null,
                    "before_sha256": "sha-before",
                    "after_sha256": "sha-after",
                    "before_byte_len": 3,
                    "after_byte_len": 4,
                    "diff_redaction": "omitted"
                }),
            ),
            base(
                EventKind::FILE_DIFF,
                json!({
                    "tool_call_id": "call-1",
                    "file_change_id": "evt-file-change",
                    "path": "a.txt",
                    "old_path": null,
                    "action": "modify",
                    "origin": "edit_file",
                    "diff": "--- a/a.txt\n+++ b/a.txt\n@@ -1,1 +1,1 @@\n-old\n+new\n",
                    "truncated": false,
                    "truncation": "none",
                    "omitted_reason": null
                }),
            ),
            base(
                EventKind::WORKSPACE_RESTORE,
                json!({
                    "path": "a.txt",
                    "checkpoint_event_id": "evt-file-change",
                    "blob_sha256": "sha-before",
                    "restored": true
                }),
            ),
            base(EventKind::CHECK_STARTED, json!({"name": "cargo test"})),
            base(
                EventKind::CHECK_RESULT,
                json!({"name": "cargo test", "ok": true, "output": "ok"}),
            ),
            base(
                EventKind::MODEL_CALL,
                json!({"provider": "fixture", "model": "echo", "canvas_items": 1}),
            ),
            base(
                EventKind::MODEL_CALL,
                json!({"provider": "fixture", "model": "echo", "canvas_items": 1, "reasoning_effort": "extra-high"}),
            ),
            base(
                EventKind::MODEL_RESULT,
                json!({
                    "provider": "fixture",
                    "model": "echo",
                    "content": "hi",
                    "tool_calls": [],
                    "stop_reason": "completed",
                    "usage": {"input_tokens": 1, "output_tokens": 1, "cached_tokens": 0, "reasoning_tokens": 0}
                }),
            ),
            base(
                EventKind::MODEL_REASONING,
                json!({"provider": "fixture", "model": "echo", "fidelity": "summary", "content": "because", "artifact": "opaque-ref"}),
            ),
            base(
                EventKind::MODEL_DELTA,
                json!({"kind": "text", "delta": "h"}),
            ),
            base(
                EventKind::MODEL_SWITCHED,
                json!({"from_provider": "fixture", "from_model": "echo", "to_provider": "chatgpt", "to_model": "gpt-5.5", "reason": "user"}),
            ),
            base(
                EventKind::CONTEXT_LIMIT,
                json!({"provider": "fixture", "model": "echo", "used_tokens": 900, "limit_tokens": 1000, "threshold": 0.9}),
            ),
            base(
                EventKind::CONTEXT_SLOT_UPDATED,
                json!({"extension_id": "slot-ext", "slot": "main", "content": "bounded context"}),
            ),
            base(
                EventKind::CANVAS_SNAPSHOT,
                json!({"selected_event_ids": ["01J00000000000000000000000"], "counts": {"items": 1}}),
            ),
            base(
                EventKind::CANVAS_POLICY_CHANGED,
                json!({"automatic": true, "stubs": true, "budget_bytes": 640000}),
            ),
            base(
                EventKind::CANVAS_SWAP,
                json!({
                    "snapshot_start_id": "01J00000000000000000000001",
                    "snapshot_end_id": "01J00000000000000000000002",
                    "frontier_start_id": "01J00000000000000000000003",
                    "policy_version": "1",
                    "projection_schema_version": "1",
                    "projection_blob": "compacted summary",
                    "validation_result": "pass"
                }),
            ),
            base(
                EventKind::CANVAS_CANDIDATE_DISCARDED,
                json!({"reason": "snapshot end is not a safe boundary", "policy_version": "1"}),
            ),
            base(EventKind::SECRET_REDACTED, json!({"label": "token"})),
            base(
                EventKind::EXTENSION_ARTIFACT,
                json!({
                    "extension_id": "artifact-ext",
                    "display_name": "Artifact",
                    "media_type": "text/plain",
                    "path": "sessions/session/extensions/artifact-ext/artifacts/abc",
                    "sha256": "abc",
                    "byte_len": 3,
                    "source_event_ids": ["01J00000000000000000000000"],
                    "metadata": {}
                }),
            ),
            base(
                EventKind::AGENT_SPAWN,
                json!({"agent": "child", "task": "review"}),
            ),
            base(
                EventKind::AGENT_MESSAGE,
                json!({
                    "from_agent_id": "child",
                    "to_agent_id": "parent",
                    "spawn_event_id": "01J00000000000000000000001",
                    "queued_ts": "2026-06-29T21:44:00.000Z",
                    "payload": {"status": "working"}
                }),
            ),
            base(
                EventKind::AGENT_RESULT,
                json!({"agent": "child", "result": "done"}),
            ),
            base(
                EventKind::SESSION_START,
                json!({"provider": "fixture", "model": "echo"}),
            ),
            base(
                EventKind::SESSION_RENAMED,
                json!({"name": "research branch"}),
            ),
            base(EventKind::SESSION_SUMMARY, json!({"summary": "done"})),
            base(
                EventKind::ERROR,
                json!({"source": "provider", "message": "failed", "category": "transport"}),
            ),
        ]
    }

    fn assert_ratified_fields_present(event: &EventEnvelope) {
        let required = match event.kind.as_str() {
            EventKind::USER_MESSAGE | EventKind::ASSISTANT_MESSAGE => vec!["content"],
            EventKind::TOOL_CALL => vec!["id", "name", "input"],
            EventKind::TOOL_RESULT => vec!["id", "name", "ok"],
            EventKind::PERMISSION_PROMPT => vec!["capability", "reason"],
            EventKind::PERMISSION_DECISION => {
                vec!["capability", "mode", "allowed", "decision"]
            }
            EventKind::PATCH_PROPOSED | EventKind::PATCH_APPLIED => vec!["path", "old", "new"],
            EventKind::FILE_CHANGE => {
                vec![
                    "tool_call_id",
                    "origin",
                    "action",
                    "path",
                    "old_path",
                    "before_sha256",
                    "after_sha256",
                    "before_byte_len",
                    "after_byte_len",
                    "diff_redaction",
                ]
            }
            EventKind::FILE_DIFF => {
                vec![
                    "tool_call_id",
                    "file_change_id",
                    "path",
                    "old_path",
                    "action",
                    "origin",
                    "diff",
                    "truncated",
                    "truncation",
                    "omitted_reason",
                ]
            }
            EventKind::WORKSPACE_RESTORE => {
                vec!["path", "checkpoint_event_id", "blob_sha256", "restored"]
            }
            EventKind::MODEL_CALL => vec!["provider", "model", "canvas_items"],
            EventKind::MODEL_RESULT => {
                vec![
                    "provider",
                    "model",
                    "content",
                    "tool_calls",
                    "stop_reason",
                    "usage",
                ]
            }
            EventKind::MODEL_REASONING => vec!["provider", "model", "fidelity", "content"],
            EventKind::MODEL_DELTA => vec!["kind", "delta"],
            EventKind::MODEL_SWITCHED => {
                vec![
                    "from_provider",
                    "from_model",
                    "to_provider",
                    "to_model",
                    "reason",
                ]
            }
            EventKind::CONTEXT_LIMIT => {
                vec![
                    "provider",
                    "model",
                    "used_tokens",
                    "limit_tokens",
                    "threshold",
                ]
            }
            EventKind::CONTEXT_SLOT_UPDATED => vec!["extension_id", "slot", "content"],
            EventKind::CANVAS_SNAPSHOT => vec!["selected_event_ids", "counts"],
            EventKind::CANVAS_POLICY_CHANGED => vec!["automatic", "stubs", "budget_bytes"],
            EventKind::CANVAS_SWAP => {
                vec![
                    "snapshot_start_id",
                    "snapshot_end_id",
                    "frontier_start_id",
                    "policy_version",
                    "projection_schema_version",
                    "projection_blob",
                    "validation_result",
                ]
            }
            EventKind::CANVAS_CANDIDATE_DISCARDED => vec!["reason", "policy_version"],
            EventKind::EXTENSION_ARTIFACT => {
                vec![
                    "extension_id",
                    "display_name",
                    "media_type",
                    "path",
                    "sha256",
                    "byte_len",
                    "source_event_ids",
                    "metadata",
                ]
            }
            EventKind::AGENT_MESSAGE => {
                vec![
                    "from_agent_id",
                    "to_agent_id",
                    "spawn_event_id",
                    "queued_ts",
                    "payload",
                ]
            }
            EventKind::MODEL_EFFORT_CHANGED => vec!["from_effort", "to_effort", "reason"],
            EventKind::SESSION_START => vec!["provider", "model"],
            EventKind::SESSION_RENAMED => vec!["name"],
            EventKind::ERROR => vec!["source", "message"],
            _ => Vec::new(),
        };

        for key in required {
            assert!(
                event.payload.contains_key(key),
                "{} missing {key}",
                event.kind
            );
        }
    }
}
