Review the supplied final Rust patches independently. You have no tools and must answer in one response using only the supplied source. Do not ask for more files. Find concrete correctness mistakes, especially overly broad recovery fences or missed retry paths. At most 4 actionable findings, each with severity, file/function, trigger, and minimal fix. Clearly label source-only concerns. If none, say so. Keep the response under 800 words.

Context: #213 formerly retried SemanticIdle as Transport in shadow compaction. A synthetic public Session probe confirmed 3 dispatches before and 1 after this fix; FirstByte remained 3 with retry budget 2.
#216 baseline had a one-shot checkpoint FileSync failure, then an unrelated turn could dispatch. The original test had only one fixture response, masking the second dispatch as fixture exhaustion. A two-response probe confirmed dispatches 1->2, events 5->12, and no terminal for the first call until reopen. Final patch should require reopen only while root response ownership is open, preserving post-terminal append and queued-admission retries. 244 session unit, 53 resume, 151 session-loop tests and Clippy pass; fresh Linux CI is green. Those checks are evidence, not a reason to accept the patch.
Known adjacent preexisting issue, outside this fix: a model.call sync failure before prepare_model_request returns can leave a bus call with no provider dispatch and no terminal. Do not repeat this as a new introduced bug. Review #213 and #216 separately; their stack still needs rebasing/integration.


#213 final patch
```diff
commit ff70bb67ff65ae3e4c0f10334889c16175f56d8a
Author:     Codex <codex@openai.com>
AuthorDate: Sat Sep 5 21:08:53 2026 -0700
Commit:     Codex <codex@openai.com>
CommitDate: Sat Sep 5 21:08:53 2026 -0700

    fix(provider): share retry policy with shadow compaction
    
    Shadow compaction classified semantic-idle timeouts as ordinary transport
    failures and replayed the request before any provider-neutral progress.
    Reuse the existing round-loop predicate so semantic-idle timeouts stop
    after one attempt while pre-first-byte retries retain their budget.
    
    Add worker-level stage coverage with a successful second response to prove
    that prohibited replays do not dispatch. Clarify the provider contract's
    scope. Compaction and round-loop tests, formatting, and core Clippy pass.
    
    Co-authored-by: Codex <codex@openai.com>

diff --git a/crates/euler-core/src/session/compaction_worker.rs b/crates/euler-core/src/session/compaction_worker.rs
index ba70923..c439724 100644
--- a/crates/euler-core/src/session/compaction_worker.rs
+++ b/crates/euler-core/src/session/compaction_worker.rs
@@ -1,9 +1,8 @@
-use super::round_loop::ModelRoundData;
+use super::round_loop::{provider_failure_is_retryable, ModelRoundData};
 use super::{provider_cancellation, push_reasoning_chunk, ModelTarget, ProviderRuntimeContext};
 use crate::{ProviderRuntimeEvent, ProviderRuntimeObserver, ProviderRuntimeScope};
 use euler_provider::{
-    ModelRequest, ModelStreamEvent, ProviderAttemptEvent, ProviderError, ProviderErrorCategory,
-    ProviderSet,
+    ModelRequest, ModelStreamEvent, ProviderAttemptEvent, ProviderError, ProviderSet,
 };
 use euler_sdk::{CancellationSource, CancellationToken};
 use std::sync::atomic::{AtomicBool, Ordering};
@@ -164,12 +163,12 @@ fn invoke_with_retries(
             Ok(Some(data)) => return WorkerOutcome::Finished(Ok(data)),
             Ok(None) => return WorkerOutcome::Cancelled,
             Err(error)
-                if !provider_neutral_progress
-                    && attempt < config.retries
-                    && matches!(
-                        error.category(),
-                        ProviderErrorCategory::Transport | ProviderErrorCategory::RateLimit
-                    ) =>
+                if provider_failure_is_retryable(
+                    &error,
+                    provider_neutral_progress,
+                    attempt,
+                    config.retries,
+                ) =>
             {
                 let delay = config
                     .retry_backoff_ms
@@ -446,6 +445,70 @@ mod tests {
         assert_eq!(round.usage.expect("usage").input_tokens, 8);
     }
 
+    #[test]
+    fn compaction_retries_inactivity_timeouts_by_stage() {
+        for (stage, should_retry) in [
+            (euler_provider::ProviderTimeoutStage::ResponseHeaders, true),
+            (euler_provider::ProviderTimeoutStage::FirstByte, true),
+            (euler_provider::ProviderTimeoutStage::SemanticIdle, false),
+        ] {
+            let invokes = Arc::new(AtomicUsize::new(0));
+            let providers = ProviderSet::single(QueuedStreamsProvider {
+                streams: Mutex::new(
+                    vec![
+                        vec![Err(ProviderError::timeout(stage, Duration::ZERO))],
+                        vec![
+                            Ok(ModelStreamEvent::TextDelta("projection".to_owned())),
+                            Ok(ModelStreamEvent::Finished {
+                                stop_reason: euler_provider::StopReason::Completed,
+                                usage: None,
+                            }),
+                        ],
+                    ]
+                    .into(),
+                ),
+                invokes: Arc::clone(&invokes),
+            });
+            let mut worker = spawn(
+                providers,
+                ModelTarget::new("fixture", "fixture"),
+                ModelRequest {
+                    model: "fixture".to_owned(),
+                    instructions: "compact".to_owned(),
+                    input: Vec::new(),
+                    tools: Vec::new(),
+                    reasoning_effort: ReasoningEffort::Medium,
+                    max_output_tokens: None,
+                },
+                ProviderRunConfig {
+                    session_id: "session".to_owned(),
+                    retries: 1,
+                    retry_backoff_ms: vec![0],
+                    liveness: ProviderLivenessConfig::default(),
+                    runtime_observer: ProviderRuntimeObserver::default(),
+                },
+            );
+
+            let outcome = worker
+                .recv_timeout(Duration::from_secs(1))
+                .expect("worker outcome");
+            worker.reap_after_terminal();
+            if should_retry {
+                let WorkerOutcome::Finished(Ok(round)) = outcome else {
+                    panic!("expected successful retry for {stage:?}");
+                };
+                assert_eq!(round.content, "projection");
+                assert_eq!(invokes.load(Ordering::Relaxed), 2, "{stage:?}");
+            } else {
+                let WorkerOutcome::Finished(Err(error)) = outcome else {
+                    panic!("semantic-idle timeout must stop compaction");
+                };
+                assert_eq!(error.timeout_stage(), Some(stage));
+                assert_eq!(invokes.load(Ordering::Relaxed), 1, "{stage:?}");
+            }
+        }
+    }
+
     #[test]
     fn compaction_retries_after_only_empty_and_opaque_events() {
         let invokes = Arc::new(AtomicUsize::new(0));
diff --git a/crates/euler-core/src/session/round_loop.rs b/crates/euler-core/src/session/round_loop.rs
index 3dc7b78..adad3d3 100644
--- a/crates/euler-core/src/session/round_loop.rs
+++ b/crates/euler-core/src/session/round_loop.rs
@@ -429,7 +429,7 @@ fn collect_stream_event(event: ModelStreamEvent, data: &mut ModelRoundData) {
 /// (for example a long silent reasoning phase). Replaying it would bill the
 /// user again for an attempt that already ran. `response_headers` and
 /// `first_byte` timeouts stay retryable because nothing was received.
-fn provider_failure_is_retryable(
+pub(super) fn provider_failure_is_retryable(
     error: &ProviderError,
     provider_neutral_progress: bool,
     attempt: usize,
diff --git a/docs/contracts/provider.md b/docs/contracts/provider.md
index 8c07dee..93c686a 100644
--- a/docs/contracts/provider.md
+++ b/docs/contracts/provider.md
@@ -144,7 +144,8 @@ automatically because its remote outcome and emitted prefix cannot be
 duplicated safely. Empty deltas, provider-opaque artifacts, and transport
 control observations do not suppress an otherwise safe pre-semantic retry.
 
-Inactivity timeouts retry by stage, not by category alone:
+Inactivity timeouts in both ordinary model rounds and shadow compaction retry
+by stage, not by category alone:
 
 - `response_headers` and `first_byte` timeouts are retryable (subject to the
   progress rule and the retry budget): nothing was received, so the attempt

```

#216 final patch
```diff
commit 67abc785240028040527605f30c4a27f0b3e11b4
Author:     Codex <codex@openai.com>
AuthorDate: Sat Sep 5 21:16:56 2026 -0700
Commit:     Codex <codex@openai.com>
CommitDate: Sat Sep 5 21:16:56 2026 -0700

    fix(session): require reopen after unresolved response persistence
    
    A failed response checkpoint sync could be reconciled by an unrelated turn,
    allowing another provider dispatch while the original call remained open.
    Retain the existing terminalization fence when root response ownership exits
    with unresolved persistence, and reject fresh admission before side effects.
    
    Release response ownership only after a durable semantic terminal so later
    tool-result and presentation appends retain their exact retry behavior.
    Strengthen fault-injection coverage with a valid second response, dispatch
    and event counts, control fences, recovery outcomes, and positive retry cases.
    Clarify the provenance contract's response ownership boundary.
    
    Validation: 244 session unit tests, 53 resume tests, 151 session-loop tests,
    core all-target Clippy, formatting, and diff checks passed.
    
    Co-authored-by: Codex <codex@openai.com>

diff --git a/crates/euler-core/src/session.rs b/crates/euler-core/src/session.rs
index 4192415..63d61a4 100644
--- a/crates/euler-core/src/session.rs
+++ b/crates/euler-core/src/session.rs
@@ -463,9 +463,10 @@ pub struct Session<D> {
     /// matching retry reuses its id and timestamp; every unrelated admission
     /// is fenced until the owning writer confirms this candidate.
     pending_admission: Option<PendingAdmission>,
-    /// A shadow worker was detached without an accepted terminal child for
-    /// its `model.call`. Further authoritative writes fail closed until the
-    /// durable log is reopened and its recovery closure is appended.
+    /// Root response ownership ended with unresolved persistence, or a shadow
+    /// worker detached without an accepted terminal child. Further
+    /// authoritative writes fail closed until lifecycle reopen reconciles
+    /// the durable prefix and closes any interrupted calls.
     terminalization_failed: bool,
     /// Shared edge-triggered request from an interactive surface. The active
     /// driver consumes it only at a round boundary, where a fixed shadow
@@ -693,8 +694,13 @@ where
         model_call_id: String,
         observed_output_bytes: Option<u64>,
     ) -> Result<String, SessionError> {
-        self.session
-            .emit_provider_error_with_response(error, model_call_id, observed_output_bytes)
+        let id = self.session.emit_provider_error_with_response(
+            error,
+            model_call_id,
+            observed_output_bytes,
+        )?;
+        self.response_checkpoint = None;
+        Ok(id)
     }
 
     fn emit_model_call_cancelled(
@@ -709,8 +715,11 @@ where
             observed_output_bytes,
             AssistantResponseStatus::Cancelled,
         );
-        self.session
-            .emit_with_parent(EventKind::ERROR, payload, Some(model_call_id))
+        let id = self
+            .session
+            .emit_with_parent(EventKind::ERROR, payload, Some(model_call_id))?;
+        self.response_checkpoint = None;
+        Ok(id)
     }
 
     fn flush_response_checkpoints(
@@ -785,6 +794,7 @@ where
             },
             observed_output_bytes,
         )?;
+        self.response_checkpoint = None;
         self.sink.flush(self.session.bus.events());
         self.session.record_latest_usage(data.usage.as_ref());
         self.session.service_compaction_request()?;
@@ -1320,11 +1330,12 @@ impl<D> Session<D> {
                 .is_some_and(|queue| queue.has_unresolved_admission())
     }
 
-    /// Whether a fresh user turn can be admitted before the active target's
-    /// context latch. TUI auto-flush must leave queued work untouched when
-    /// this is false.
+    /// Whether a fresh user turn can be admitted under the active target's
+    /// context latch and the response-persistence reopen fence. TUI auto-flush
+    /// must leave queued work untouched when this is false.
     pub fn can_accept_turn(&self) -> bool {
-        self.context_limit_emitted.as_ref() != Some(&self.active_target)
+        !self.terminalization_failed
+            && self.context_limit_emitted.as_ref() != Some(&self.active_target)
     }
 
     /// Wire the interactive surface's edge-triggered manual-compaction
@@ -2261,6 +2272,7 @@ impl<D: PermissionDecider> Session<D> {
     where
         F: FnMut(&EventEnvelope),
     {
+        self.ensure_terminalization_intact()?;
         if self.context_limit_emitted.as_ref() == Some(&self.active_target) {
             return Ok(Vec::new());
         }
@@ -2350,6 +2362,21 @@ impl<D: PermissionDecider> Session<D> {
             },
         )
         .run(&cancellation);
+        // A successful semantic terminal releases the checkpoint. If the
+        // round exits earlier, retrying an accepted backlog cannot restore
+        // response ownership. Fence checkpoint/reasoning/terminal failures;
+        // a terminal in the bus may still be unsynced.
+        // A queued user admission retains its own exact retry owner instead.
+        if result.is_err()
+            && io.response_checkpoint.is_some()
+            && io.session.pending_admission.is_none()
+            && io.session.provenance.as_ref().is_some_and(|writer| {
+                writer.has_unresolved_append()
+                    || io.session.persisted_events < io.session.bus.events().len()
+            })
+        {
+            io.session.terminalization_failed = true;
+        }
         if matches!(&result, Err(SessionError::Cancelled)) {
             io.session.interrupt_compaction("turn interrupted")?;
             io.sink.flush(io.session.bus.events());
@@ -3556,7 +3583,7 @@ fn pending_admission_error() -> SessionError {
 fn terminalization_failed_error() -> SessionError {
     std::io::Error::new(
         std::io::ErrorKind::InvalidData,
-        "a detached model call has no accepted terminal event; reopen the session to recover it",
+        "model response persistence is unresolved; reopen the session to recover it",
     )
     .into()
 }
diff --git a/crates/euler-core/src/session_test.rs b/crates/euler-core/src/session_test.rs
index 6c92f6f..43502f1 100644
--- a/crates/euler-core/src/session_test.rs
+++ b/crates/euler-core/src/session_test.rs
@@ -33,7 +33,7 @@ use euler_sdk::{
     HostAgentTask, HostApi, SpawnAgentTask,
 };
 use serde_json::Map;
-use std::sync::atomic::Ordering;
+use std::sync::atomic::{AtomicUsize, Ordering};
 use std::sync::{Arc, Condvar, Mutex};
 
 #[test]
@@ -68,12 +68,14 @@ fn ambiguous_checkpoint_append_is_not_shown_or_reused_until_reopen() {
     let log = temp.path().join("events.jsonl");
     let mut session = Session::new(
         SessionConfig::new(temp.path()),
-        ScriptedProvider::new(vec![FixtureResponse::Assistant(
-            "visible only if durable".to_owned(),
-        )]),
+        ScriptedProvider::new(vec![
+            FixtureResponse::Assistant("visible only if durable".to_owned()),
+            FixtureResponse::Assistant("must not dispatch a follow-up".to_owned()),
+        ]),
         ScriptedDecider::new(Vec::new()),
     )
     .with_provenance(ProvenanceWriter::new(log.clone()).expect("writer"));
+    let dispatches = count_response_fault_dispatches(&mut session);
     let matched_log = log.clone();
     let guard = arm_matching(Op::FileSync, move |path| {
         path == matched_log
@@ -98,12 +100,7 @@ fn ambiguous_checkpoint_append_is_not_shown_or_reused_until_reopen() {
         .iter()
         .any(|kind| kind == EventKind::ASSISTANT_RESPONSE_CHUNK));
     drop(guard);
-    assert!(
-        session
-            .run_turn("cannot continue on fenced writer")
-            .is_err(),
-        "a new run must not reinterpret the unresolved checkpoint append"
-    );
+    assert_response_persistence_fenced(&mut session, &dispatches, &log);
     drop(session);
 
     let resumed = crate::resume_session(
@@ -126,29 +123,83 @@ fn ambiguous_checkpoint_append_is_not_shown_or_reused_until_reopen() {
         })
         .expect("interrupted response closure");
     assert_eq!(closure.payload["observed_output_bytes"], json!(23));
+    assert!(resumed.can_accept_turn());
+}
+
+fn count_response_fault_dispatches(session: &mut Session<ScriptedDecider>) -> Arc<AtomicUsize> {
+    let dispatches = Arc::new(AtomicUsize::new(0));
+    let counter = Arc::clone(&dispatches);
+    session.set_provider_runtime_observer(ProviderRuntimeObserver::new(move |event| {
+        if matches!(
+            event,
+            ProviderRuntimeEvent::Attempt {
+                target: crate::ProviderRuntimeTarget {
+                    scope: ProviderRuntimeScope::Root,
+                    ..
+                },
+                event: euler_provider::ProviderAttemptEvent::Started { .. },
+            }
+        ) {
+            counter.fetch_add(1, Ordering::SeqCst);
+        }
+    }));
+    dispatches
+}
+
+fn assert_response_persistence_fenced(
+    session: &mut Session<ScriptedDecider>,
+    dispatches: &AtomicUsize,
+    log: &std::path::Path,
+) {
+    let event_count = session.events().len();
+    let bytes = std::fs::read(log).expect("physical response prefix");
+    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
+    assert!(!session.can_accept_turn());
+    for result in [
+        session
+            .run_turn("cannot continue on fenced writer")
+            .map(|_| ()),
+        session
+            .rename_session("cannot rename fenced writer")
+            .map(|_| ()),
+        session.begin_compaction().map(|_| ()),
+    ] {
+        assert!(matches!(
+            result,
+            Err(SessionError::Io(ref error)) if error.kind() == std::io::ErrorKind::InvalidData
+        ));
+    }
+    assert!(!session.has_unresolved_admission());
+    assert_eq!(session.events().len(), event_count);
+    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
+    assert_eq!(std::fs::read(log).expect("fenced log"), bytes);
 }
 
 #[test]
 fn reasoning_append_failure_cannot_lose_a_visible_checkpoint_suffix() {
     let temp = tempfile::tempdir().expect("temp dir");
     let log = temp.path().join("events.jsonl");
-    let provider = ScriptedProvider::new(vec![FixtureResponse::Stream(vec![
-        ScriptedStreamStep::Event(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
-            "final rationale",
-        ))),
-        ScriptedStreamStep::Event(ModelStreamEvent::TextDelta("durable".to_owned())),
-        ScriptedStreamStep::Event(ModelStreamEvent::TextDelta(" pending".to_owned())),
-        ScriptedStreamStep::Event(ModelStreamEvent::Finished {
-            stop_reason: StopReason::Completed,
-            usage: None,
-        }),
-    ])]);
+    let provider = ScriptedProvider::new(vec![
+        FixtureResponse::Stream(vec![
+            ScriptedStreamStep::Event(ModelStreamEvent::ReasoningDelta(ReasoningChunk::summary(
+                "final rationale",
+            ))),
+            ScriptedStreamStep::Event(ModelStreamEvent::TextDelta("durable".to_owned())),
+            ScriptedStreamStep::Event(ModelStreamEvent::TextDelta(" pending".to_owned())),
+            ScriptedStreamStep::Event(ModelStreamEvent::Finished {
+                stop_reason: StopReason::Completed,
+                usage: None,
+            }),
+        ]),
+        FixtureResponse::Assistant("must not dispatch a follow-up".to_owned()),
+    ]);
     let mut session = Session::new(
         SessionConfig::new(temp.path()),
         provider,
         ScriptedDecider::new(Vec::new()),
     )
     .with_provenance(ProvenanceWriter::new(log.clone()).expect("writer"));
+    let dispatches = count_response_fault_dispatches(&mut session);
     let matched_log = log.clone();
     let guard = arm_matching(Op::FileSync, move |path| {
         path == matched_log
@@ -165,6 +216,7 @@ fn reasoning_append_failure_cannot_lose_a_visible_checkpoint_suffix() {
     assert!(matches!(error, SessionError::Io(_)));
     assert!(guard.fired(), "reasoning sync fault must fire");
     drop(guard);
+    assert_response_persistence_fenced(&mut session, &dispatches, &log);
     drop(session);
 
     let resumed = crate::resume_session(
@@ -181,6 +233,207 @@ fn reasoning_append_failure_cannot_lose_a_visible_checkpoint_suffix() {
     assert_eq!(response.content, "durable pending");
 }
 
+#[test]
+fn response_flush_and_terminal_sync_failures_require_reopen() {
+    for (kind, finished, cancelled, status) in [
+        (
+            EventKind::ASSISTANT_RESPONSE_CHUNK,
+            true,
+            false,
+            AssistantResponseStatus::Interrupted,
+        ),
+        (
+            EventKind::MODEL_RESULT,
+            true,
+            false,
+            AssistantResponseStatus::Completed,
+        ),
+        (
+            EventKind::ERROR,
+            false,
+            false,
+            AssistantResponseStatus::Failed,
+        ),
+        (
+            EventKind::ERROR,
+            true,
+            true,
+            AssistantResponseStatus::Cancelled,
+        ),
+    ] {
+        assert_response_finalization_recovers(kind, finished, cancelled, status);
+    }
+}
+
+fn assert_response_finalization_recovers(
+    kind: &'static str,
+    finished: bool,
+    cancelled: bool,
+    status: AssistantResponseStatus,
+) {
+    let temp = tempfile::tempdir().expect("temp dir");
+    let log = temp.path().join("events.jsonl");
+    let mut steps = vec![
+        ScriptedStreamStep::Event(ModelStreamEvent::TextDelta("durable".to_owned())),
+        ScriptedStreamStep::Event(ModelStreamEvent::TextDelta(" pending".to_owned())),
+    ];
+    if finished {
+        steps.push(ScriptedStreamStep::Event(ModelStreamEvent::Finished {
+            stop_reason: StopReason::Completed,
+            usage: None,
+        }));
+    }
+    let mut session = Session::new(
+        SessionConfig::new(temp.path()),
+        ScriptedProvider::new(vec![
+            FixtureResponse::Stream(steps),
+            FixtureResponse::Assistant("must not dispatch a follow-up".to_owned()),
+        ]),
+        ScriptedDecider::new(Vec::new()),
+    )
+    .with_provenance(ProvenanceWriter::new(log.clone()).expect("writer"));
+    let dispatches = count_response_fault_dispatches(&mut session);
+    let matched_log = log.clone();
+    let guard = arm_matching(Op::FileSync, move |path| {
+        path == matched_log
+            && std::fs::read_to_string(path).is_ok_and(|raw| {
+                raw.lines().last().is_some_and(|line| {
+                    line.contains(kind)
+                        && (kind != EventKind::ASSISTANT_RESPONSE_CHUNK
+                            || line.contains("\"sequence\":1"))
+                })
+            })
+    });
+    let cancel = Arc::new(AtomicBool::new(false));
+    let sink_cancel = Arc::clone(&cancel);
+    let mut deltas = 0;
+    let error = session
+        .run_turn_with_sink("answer", cancel, |event| {
+            if event.kind.as_str() == EventKind::MODEL_DELTA {
+                deltas += 1;
+                if cancelled && deltas == 2 {
+                    sink_cancel.store(true, Ordering::Relaxed);
+                }
+            }
+        })
+        .expect_err("response append sync is ambiguous");
+    assert!(matches!(error, SessionError::Io(_)), "{kind}: {error}");
+    assert!(guard.fired(), "{kind} sync fault must fire");
+    drop(guard);
+    assert_response_persistence_fenced(&mut session, &dispatches, &log);
+    drop(session);
+
+    let resumed = crate::resume_session(
+        SessionConfig::new(temp.path()),
+        ProviderSet::single(ScriptedProvider::new(vec![])),
+        ScriptedDecider::new(Vec::new()),
+        &log,
+    )
+    .expect("reopen physical response prefix");
+    let projected = crate::project_assistant_response_terminals(resumed.events())
+        .expect("valid recovered response protocol");
+    assert_eq!(projected.len(), 1, "{kind}: exactly one response terminal");
+    let response = projected.values().next().expect("recovered response");
+    assert_eq!(response.status, status, "{kind}");
+    assert_eq!(response.content, "durable pending", "{kind}");
+    assert!(resumed.can_accept_turn());
+}
+
+#[test]
+fn durable_provider_failure_releases_response_ownership_for_follow_up() {
+    let temp = tempfile::tempdir().expect("temp dir");
+    let log = temp.path().join("events.jsonl");
+    let mut session = Session::new(
+        SessionConfig::new(temp.path()),
+        ScriptedProvider::new(vec![
+            FixtureResponse::Stream(vec![ScriptedStreamStep::Event(
+                ModelStreamEvent::TextDelta("partial response".to_owned()),
+            )]),
+            FixtureResponse::Assistant("follow-up succeeds".to_owned()),
+        ]),
+        ScriptedDecider::new(Vec::new()),
+    )
+    .with_provenance(ProvenanceWriter::new(log).expect("writer"));
+    let dispatches = count_response_fault_dispatches(&mut session);
+    let error = session.run_turn("first").expect_err("provider truncation");
+    assert!(matches!(error, SessionError::Provider(_)));
+    assert!(session.can_accept_turn());
+    session
+        .run_turn("follow-up")
+        .expect("durable failure permits another turn");
+    assert_eq!(dispatches.load(Ordering::SeqCst), 2);
+    let projected = crate::project_assistant_response_terminals(session.events())
+        .expect("valid response terminals");
+    assert_eq!(projected.len(), 2);
+    assert!(projected
+        .values()
+        .any(|response| response.status == AssistantResponseStatus::Failed));
+    assert!(projected
+        .values()
+        .any(|response| response.status == AssistantResponseStatus::Completed));
+}
+
+#[test]
+fn durable_model_terminal_preserves_later_append_retry_ownership() {
+    for kind in [EventKind::ASSISTANT_MESSAGE, EventKind::TOOL_RESULT] {
+        let temp = tempfile::tempdir().expect("temp dir");
+        let log = temp.path().join("events.jsonl");
+        std::fs::write(temp.path().join("note.txt"), "note").expect("fixture");
+        let first = if kind == EventKind::TOOL_RESULT {
+            FixtureResponse::ToolCalls(vec![euler_provider::ToolCall {
+                id: "read-note".to_owned(),
+                name: "read_file".to_owned(),
+                input: json!({"path": "note.txt"}),
+            }])
+        } else {
+            FixtureResponse::Assistant("completed response".to_owned())
+        };
+        let mut session = Session::new(
+            SessionConfig::new(temp.path()),
+            ScriptedProvider::new(vec![
+                first,
+                FixtureResponse::Assistant("follow-up succeeds".to_owned()),
+            ]),
+            ScriptedDecider::new(Vec::new()),
+        )
+        .with_provenance(ProvenanceWriter::new(log.clone()).expect("writer"));
+        let dispatches = count_response_fault_dispatches(&mut session);
+        let matched_log = log.clone();
+        let guard = arm_matching(Op::FileSync, move |path| {
+            path == matched_log
+                && std::fs::read_to_string(path)
+                    .is_ok_and(|raw| raw.lines().last().is_some_and(|line| line.contains(kind)))
+        });
+        let error = session
+            .run_turn("first")
+            .expect_err("post-terminal sync fault");
+        assert!(matches!(error, SessionError::Io(_)), "{kind}: {error}");
+        assert!(guard.fired(), "{kind} sync fault must fire");
+        drop(guard);
+        assert!(
+            session.can_accept_turn(),
+            "{kind}: response already terminal"
+        );
+        session
+            .run_turn("follow-up")
+            .expect("accepted backlog may reconcile");
+        assert_eq!(dispatches.load(Ordering::SeqCst), 2);
+        let durable = crate::read_resume_prefix(&log).expect("valid durable log");
+        assert_eq!(
+            durable
+                .iter()
+                .filter(|event| event.kind.as_str() == kind)
+                .count(),
+            session
+                .events()
+                .iter()
+                .filter(|event| event.kind.as_str() == kind)
+                .count(),
+            "{kind}: exact retry does not duplicate the accepted event"
+        );
+    }
+}
+
 #[test]
 fn session_config_forwards_requested_subprocess_sandbox_to_tool_registry() {
     let temp = tempfile::tempdir().expect("temp dir");
diff --git a/docs/contracts/provenance.md b/docs/contracts/provenance.md
index 88888ad..0b30c3e 100644
--- a/docs/contracts/provenance.md
+++ b/docs/contracts/provenance.md
@@ -79,11 +79,15 @@ identity but cannot prove that an earlier rename survived a failed directory
 sync.
 
 Streamed root-assistant text uses ordinary durable
-`assistant.response.chunk` appends. If a checkpoint append becomes ambiguous,
-its text is not forwarded to the live UI and the writer fences unrelated
-activity; lifecycle reopen is the recovery boundary. A physically complete
-checkpoint is then accepted once and the open call receives an interrupted
-recovery terminal. Chunk content above the blob threshold is content-addressed
+`assistant.response.chunk` appends. Root response ownership lasts until its
+canonical terminal is durably accepted. An unresolved checkpoint, reasoning,
+or terminal append before that boundary fences unrelated live-session activity;
+lifecycle reopen is the recovery boundary. A checkpoint failure stops further
+stream forwarding. Reopen accepts a physically complete checkpoint or terminal
+once, preserves any recorded terminal outcome, and gives a still-open call an
+interrupted recovery terminal. After an accepted canonical terminal, later
+appends retain their ordinary exact-batch reconciliation rules. Chunk content
+above the blob threshold is content-addressed
 and rehydrated for replay. Secret scrub rewrites chunk and terminal
 `retained_content_bytes` together so the scrubbed stream remains
 protocol-valid; immutable `observed_output_bytes` remains the original local

```

/private/tmp/euler-wt/stack-216/crates/euler-core/src/session.rs:640-840
```rust
640:     type Complete = ();
641: 
642:     fn session_id(&self) -> &str {
643:         &self.session.config.session_id
644:     }
645: 
646:     fn target(&self) -> ModelTarget {
647:         self.session.active_target.clone()
648:     }
649: 
650:     fn provider_runtime_observer(&self) -> &ProviderRuntimeObserver {
651:         &self.session.provider_runtime_observer
652:     }
653: 
654:     fn provider_runtime_scope(&self) -> ProviderRuntimeScope {
655:         ProviderRuntimeScope::Root
656:     }
657: 
658:     fn prepare_model_request(
659:         &mut self,
660:         target: &ModelTarget,
661:     ) -> Result<(String, ModelRequest), SessionError> {
662:         let cancellation = self.cancellation.clone();
663:         let prepared = self
664:             .session
665:             .prepare_model_request(target, self.sink, &cancellation)?;
666:         self.response_checkpoint = Some(ResponseCheckpoint::new(prepared.0.clone()));
667:         Ok(prepared)
668:     }
669: 
670:     fn invoke_model(
671:         &mut self,
672:         target: &ModelTarget,
673:         request: ModelRequest,
674:     ) -> Result<ProviderStream, ProviderError> {
675:         let observer = ProviderRuntimeContext::new(
676:             &self.session.config.session_id,
677:             target,
678:             ProviderRuntimeScope::Root,
679:             &self.session.provider_runtime_observer,
680:         )
681:         .attempt_observer();
682:         self.session.providers.invoke_interruptibly(
683:             &target.provider,
684:             request,
685:             provider_cancellation(self.cancellation.clone()),
686:             self.session.config.provider_liveness,
687:             observer,
688:         )
689:     }
690: 
691:     fn emit_provider_error(
692:         &mut self,
693:         error: &ProviderError,
694:         model_call_id: String,
695:         observed_output_bytes: Option<u64>,
696:     ) -> Result<String, SessionError> {
697:         let id = self.session.emit_provider_error_with_response(
698:             error,
699:             model_call_id,
700:             observed_output_bytes,
701:         )?;
702:         self.response_checkpoint = None;
703:         Ok(id)
704:     }
705: 
706:     fn emit_model_call_cancelled(
707:         &mut self,
708:         model_call_id: String,
709:         observed_output_bytes: Option<u64>,
710:     ) -> Result<String, SessionError> {
711:         let mut payload = round_loop::model_call_cancelled_payload();
712:         add_response_terminal_metadata(
713:             &mut payload,
714:             &model_call_id,
715:             observed_output_bytes,
716:             AssistantResponseStatus::Cancelled,
717:         );
718:         let id = self
719:             .session
720:             .emit_with_parent(EventKind::ERROR, payload, Some(model_call_id))?;
721:         self.response_checkpoint = None;
722:         Ok(id)
723:     }
724: 
725:     fn flush_response_checkpoints(
726:         &mut self,
727:         model_call_id: &str,
728:     ) -> Result<Option<u64>, SessionError> {
729:         let Some(checkpoint) = self.response_checkpoint.as_mut() else {
730:             return Ok(None);
731:         };
732:         debug_assert_eq!(checkpoint.response_id(), model_call_id);
733:         let chunks = checkpoint
734:             .flush(Instant::now())
735:             .map_err(std::io::Error::other)?;
736:         self.session.emit_response_chunks(model_call_id, chunks)?;
737:         Ok(checkpoint.has_text().then(|| checkpoint.observed_bytes()))
738:     }
739: 
740:     fn after_stream_event(
741:         &mut self,
742:         event: &ModelStreamEvent,
743:         model_call_id: &str,
744:     ) -> Result<(), SessionError> {
745:         if let ModelStreamEvent::TextDelta(delta) = event {
746:             if let Some(checkpoint) = self.response_checkpoint.as_mut() {
747:                 debug_assert_eq!(checkpoint.response_id(), model_call_id);
748:                 let chunks = checkpoint
749:                     .observe_text(delta, Instant::now())
750:                     .map_err(std::io::Error::other)?;
751:                 // Checkpoints become durable before the runtime delta reaches
752:                 // the sink, so the first visible prefix cannot be ephemeral.
753:                 self.session.emit_response_chunks(model_call_id, chunks)?;
754:             }
755:         }
756:         self.session
757:             .record_stream_event(event, model_call_id, self.sink)
758:     }
759: 
760:     fn flush_events(&mut self) {
761:         self.sink.flush(self.session.bus.events());
762:     }
763: 
764:     fn finish_round(
765:         &mut self,
766:         target: ModelTarget,
767:         model_call_id: String,
768:         data: ModelRoundData,
769:         cancellation: &CancellationToken,
770:         another_round_available: bool,
771:     ) -> Result<RoundOutcome, SessionError> {
772:         let stop_reason = data
773:             .stop_reason
774:             .as_ref()
775:             .expect("validated finished stream");
776:         // Flush every visible text byte before any other fallible
777:         // finalization write. A reasoning/result append failure may fence the
778:         // writer, but lifecycle reopen must still recover the exact response
779:         // prefix the user already saw.
780:         let observed_output_bytes = self.flush_response_checkpoints(&model_call_id)?;
781:         for item in &data.reasoning {
782:             self.session
783:                 .emit_model_reasoning(item, &target, model_call_id.clone())?;
784:             self.sink.flush(self.session.bus.events());
785:         }
786:         let model_result_id = self.session.emit_model_result(
787:             companion::ModelResultRecord {
788:                 content: &data.content,
789:                 tool_calls: &data.tool_calls,
790:                 stop_reason,
791:                 usage: data.usage.as_ref(),
792:                 target: &target,
793:                 parent: model_call_id.clone(),
794:             },
795:             observed_output_bytes,
796:         )?;
797:         self.response_checkpoint = None;
798:         self.sink.flush(self.session.bus.events());
799:         self.session.record_latest_usage(data.usage.as_ref());
800:         self.session.service_compaction_request()?;
801:         self.session.auto_compact_if_triggered()?;
802:         self.session.poll_shadow_compaction()?;
803:         self.sink.flush(self.session.bus.events());
804: 
805:         if self
806:             .session
807:             .finish_context_limit(&data, &model_result_id, self.sink, cancellation)?
808:         {
809:             return Ok(RoundOutcome::Complete(()));
810:         }
811:         if data.tool_calls.is_empty() {
812:             // A truncated/refused round that produced no visible content is
813:             // not a completed turn; ending silently here looked like success
814:             // while the model had only burned reasoning budget.
815:             if data.content.is_empty()
816:                 && matches!(
817:                     stop_reason,
818:                     StopReason::MaxTokens | StopReason::Refusal | StopReason::Error
819:                 )
820:             {
821:                 let error = ProviderError::stream_truncation(format!(
822:                     "model stopped ({}) with no content; raise max_output_tokens if reasoning consumed the budget",
823:                     stop_reason.as_str()
824:                 ));
825:                 self.session.emit_provider_error(&error, model_result_id)?;
826:                 self.sink.flush(self.session.bus.events());
827:                 return Err(error.into());
828:             }
829:             self.session.emit_with_parent(
830:                 EventKind::ASSISTANT_MESSAGE,
831:                 object([("content", data.content.into())]),
832:                 Some(model_result_id),
833:             )?;
834:             self.sink.flush(self.session.bus.events());
835:             return self.resolve_terminal_idle(cancellation, another_round_available);
836:         }
837: 
838:         self.finish_tool_round(&model_result_id, data.tool_calls, cancellation)
839:     }
840: 
```

/private/tmp/euler-wt/stack-216/crates/euler-core/src/session.rs:1640-1765
```rust
1640:     ///
1641:     /// General turn events intentionally retain accepted in-memory evidence
1642:     /// when a later append fails. User messages have a stronger queue
1643:     /// transaction: the exact envelope id, timestamp, and parent are installed
1644:     /// before any accepted backlog is written. A failure while reconciling
1645:     /// that backlog or appending the candidate therefore protects the same
1646:     /// queue row. Repair + retry can reconcile an ambiguous complete append
1647:     /// instead of creating a second event. While it is pending, every
1648:     /// unrelated append is fenced.
1649:     fn admit_user_message(
1650:         &mut self,
1651:         content: &str,
1652:         queue_id: Option<steering::QueueEntryId>,
1653:     ) -> Result<String, SessionError> {
1654:         self.ensure_terminalization_intact()?;
1655:         let payload = object([("content", content.to_owned().into())]);
1656:         if let Some(pending) = &self.pending_admission {
1657:             if pending.queue_id != queue_id
1658:                 || pending.event.kind.as_str() != EventKind::USER_MESSAGE
1659:                 || pending.event.payload != payload
1660:             {
1661:                 return Err(pending_admission_error());
1662:             }
1663:         } else {
1664:             self.pending_admission = Some(PendingAdmission {
1665:                 event: EventEnvelope::new(
1666:                     self.config.session_id.clone(),
1667:                     self.config.agent_id.clone(),
1668:                     self.previous_persisted_event_id(),
1669:                     EventKind::USER_MESSAGE,
1670:                     payload,
1671:                 ),
1672:                 queue_id,
1673:             });
1674:         }
1675:         if let Err(error) = self.persist_pending_admission_backlog() {
1676:             self.protect_pending_queue_row();
1677:             return Err(error);
1678:         }
1679:         let pending = self
1680:             .pending_admission
1681:             .as_ref()
1682:             .expect("admission was matched or created");
1683:         let event = pending.event.clone();
1684:         let id = event.id.clone();
1685:         if let Err(error) = self.append_candidate(&event) {
1686:             self.protect_pending_queue_row();
1687:             return Err(error);
1688:         }
1689:         self.bus.push(event);
1690:         self.pending_admission = None;
1691:         if self.provenance.is_some() {
1692:             self.persisted_events = self.bus.events().len();
1693:         }
1694:         Ok(id)
1695:     }
1696: 
1697:     /// The sole append path allowed after a pending user admission has been
1698:     /// installed. It owns only the older accepted bus suffix; the candidate
1699:     /// itself is appended separately and is not visible in memory yet.
1700:     fn persist_pending_admission_backlog(&mut self) -> Result<(), SessionError> {
1701:         debug_assert!(self.pending_admission.is_some());
1702:         self.ensure_terminalization_intact()?;
1703:         if self.persisted_events < self.bus.events().len() {
1704:             if let Some(writer) = &self.provenance {
1705:                 writer.append(&self.bus.events()[self.persisted_events..])?;
1706:                 self.persisted_events = self.bus.events().len();
1707:             }
1708:         }
1709:         Ok(())
1710:     }
1711: 
1712:     fn protect_pending_queue_row(&self) {
1713:         let queue_id = self
1714:             .pending_admission
1715:             .as_ref()
1716:             .and_then(|pending| pending.queue_id);
1717:         if let (Some(queue), Some(queue_id)) = (&self.steering, queue_id) {
1718:             queue.mark_admission_unresolved(queue_id);
1719:         }
1720:     }
1721: 
1722:     fn ensure_no_pending_admission(&self) -> Result<(), SessionError> {
1723:         self.ensure_terminalization_intact()?;
1724:         if self.pending_admission.is_some() {
1725:             Err(pending_admission_error())
1726:         } else {
1727:             Ok(())
1728:         }
1729:     }
1730: 
1731:     fn ensure_terminalization_intact(&self) -> Result<(), SessionError> {
1732:         if self.terminalization_failed {
1733:             Err(terminalization_failed_error())
1734:         } else {
1735:             Ok(())
1736:         }
1737:     }
1738: 
1739:     fn previous_persisted_event_id(&self) -> Option<String> {
1740:         self.bus
1741:             .events()
1742:             .iter()
1743:             .rev()
1744:             .find(|event| event.kind.as_str() != EventKind::MODEL_DELTA)
1745:             .map(|event| event.id.clone())
1746:     }
1747: 
1748:     fn persist_new_events(&mut self) -> Result<(), SessionError> {
1749:         self.ensure_no_pending_admission()?;
1750:         if let Some(writer) = &self.provenance {
1751:             writer.append(&self.bus.events()[self.persisted_events..])?;
1752:             self.persisted_events = self.bus.events().len();
1753:         }
1754:         Ok(())
1755:     }
1756: 
1757:     pub fn set_permission_mode(&mut self, capability: Capability, mode: ApprovalMode) {
1758:         self.permissions.set_mode(capability, mode);
1759:     }
1760: 
1761:     /// Who resolves uncovered `ask` permission decisions (ADR 0011).
1762:     pub fn permission_reviewer(&self) -> PermissionReviewer {
1763:         self.config.permission_reviewer
1764:     }
1765: 
```

/private/tmp/euler-wt/stack-216/crates/euler-core/src/session.rs:2320-2398
```rust
2320:         let Some(input) = self.queued_dispatch.take() else {
2321:             return;
2322:         };
2323:         if let Some(queue) = &self.steering {
2324:             queue.release_dispatch(&input);
2325:         }
2326:     }
2327: 
2328:     fn close_steering_turn(&self) {
2329:         if let Some(queue) = &self.steering {
2330:             queue.close_turn();
2331:         }
2332:     }
2333: 
2334:     fn run_model_rounds<F>(
2335:         &mut self,
2336:         start: usize,
2337:         cancellation: CancellationToken,
2338:         sink: &mut EventSink<'_, F>,
2339:     ) -> Result<Vec<EventEnvelope>, SessionError>
2340:     where
2341:         F: FnMut(&EventEnvelope),
2342:     {
2343:         let mut turn_state = TurnState::default();
2344:         let mut rounds = 0_u64;
2345:         let max_rounds = self.config.max_tool_rounds;
2346:         let provider_retries = self.config.provider_transport_retries;
2347:         let provider_retry_backoff_ms = self.config.provider_transport_retry_backoff_ms.clone();
2348:         let mut io = SessionRoundIo {
2349:             session: self,
2350:             sink,
2351:             turn_state: &mut turn_state,
2352:             rounds: &mut rounds,
2353:             cancellation: cancellation.clone(),
2354:             response_checkpoint: None,
2355:         };
2356:         let result = RoundLoop::new(
2357:             &mut io,
2358:             RoundLoopConfig {
2359:                 max_rounds,
2360:                 provider_retries,
2361:                 provider_retry_backoff_ms,
2362:             },
2363:         )
2364:         .run(&cancellation);
2365:         // A successful semantic terminal releases the checkpoint. If the
2366:         // round exits earlier, retrying an accepted backlog cannot restore
2367:         // response ownership. Fence checkpoint/reasoning/terminal failures;
2368:         // a terminal in the bus may still be unsynced.
2369:         // A queued user admission retains its own exact retry owner instead.
2370:         if result.is_err()
2371:             && io.response_checkpoint.is_some()
2372:             && io.session.pending_admission.is_none()
2373:             && io.session.provenance.as_ref().is_some_and(|writer| {
2374:                 writer.has_unresolved_append()
2375:                     || io.session.persisted_events < io.session.bus.events().len()
2376:             })
2377:         {
2378:             io.session.terminalization_failed = true;
2379:         }
2380:         if matches!(&result, Err(SessionError::Cancelled)) {
2381:             io.session.interrupt_compaction("turn interrupted")?;
2382:             io.sink.flush(io.session.bus.events());
2383:         }
2384:         drop(io);
2385:         crate::diagnostics::turn_end(&self.config.session_id, rounds);
2386:         result.map(|()| self.bus.events()[start..].to_vec())
2387:     }
2388: 
2389:     fn prepare_model_request<F>(
2390:         &mut self,
2391:         target: &ModelTarget,
2392:         sink: &mut EventSink<'_, F>,
2393:         cancellation: &CancellationToken,
2394:     ) -> Result<(String, ModelRequest), SessionError>
2395:     where
2396:         F: FnMut(&EventEnvelope),
2397:     {
2398:         // One request owns one immutable extension-tool catalog. The same
```

/private/tmp/euler-wt/stack-216/crates/euler-core/src/session.rs:3540-3588
```rust
3540: 
3541:     fn emit(&mut self, kind: &'static str, payload: JsonObject) -> Result<String, SessionError> {
3542:         let parent = self.previous_persisted_event_id();
3543:         self.emit_with_parent(kind, payload, parent)
3544:     }
3545: 
3546:     fn emit_with_parent(
3547:         &mut self,
3548:         kind: &'static str,
3549:         payload: JsonObject,
3550:         parent: Option<String>,
3551:     ) -> Result<String, SessionError> {
3552:         self.ensure_no_pending_admission()?;
3553:         if self.provenance.is_some() && self.persisted_events < self.bus.events().len() {
3554:             self.persist_new_events()?;
3555:         }
3556:         self.bus.push(EventEnvelope::new(
3557:             self.config.session_id.clone(),
3558:             self.config.agent_id.clone(),
3559:             parent,
3560:             kind,
3561:             payload,
3562:         ));
3563:         let id = self
3564:             .bus
3565:             .events()
3566:             .last()
3567:             .expect("event just pushed")
3568:             .id
3569:             .clone();
3570:         self.persist_new_events()?;
3571:         Ok(id)
3572:     }
3573: }
3574: 
3575: fn pending_admission_error() -> SessionError {
3576:     std::io::Error::new(
3577:         std::io::ErrorKind::WouldBlock,
3578:         "a prior authoritative event admission is unresolved; retry the same operation",
3579:     )
3580:     .into()
3581: }
3582: 
3583: fn terminalization_failed_error() -> SessionError {
3584:     std::io::Error::new(
3585:         std::io::ErrorKind::InvalidData,
3586:         "model response persistence is unresolved; reopen the session to recover it",
3587:     )
3588:     .into()
```

/private/tmp/euler-wt/stack-216/crates/euler-core/src/session/round_loop.rs:244-420
```rust
244: 
245:     fn run_round(
246:         &mut self,
247:         cancellation: &CancellationToken,
248:         another_round_available: bool,
249:     ) -> Result<RoundOutcome<Io::Complete>, SessionError> {
250:         let target = self.io.target();
251:         let (model_call_id, request) = self.io.prepare_model_request(&target)?;
252:         let started = Instant::now();
253:         let mut progress = ModelCallProgress::default();
254:         let data = match self.collect_model_round(
255:             &target,
256:             &model_call_id,
257:             request,
258:             cancellation,
259:             &mut progress,
260:         ) {
261:             Ok(data) => data,
262:             Err(error) => {
263:                 crate::diagnostics::model_call_end(crate::diagnostics::ModelCallEnd {
264:                     session_id: self.io.session_id(),
265:                     provider: &target.provider,
266:                     model: &target.model,
267:                     duration_ms: elapsed_ms(started),
268:                     usage: None,
269:                     observed_output_bytes: progress.observed_output_bytes,
270:                     ok: false,
271:                 });
272:                 if matches!(&error, SessionError::Cancelled) {
273:                     let observed = self.io.flush_response_checkpoints(&model_call_id)?;
274:                     self.io.emit_model_call_cancelled(model_call_id, observed)?;
275:                     self.io.flush_events();
276:                 }
277:                 return Err(error);
278:             }
279:         };
280:         crate::diagnostics::model_call_end(crate::diagnostics::ModelCallEnd {
281:             session_id: self.io.session_id(),
282:             provider: &target.provider,
283:             model: &target.model,
284:             duration_ms: elapsed_ms(started),
285:             usage: data.usage.as_ref(),
286:             observed_output_bytes: u64::try_from(data.content.len()).unwrap_or(u64::MAX),
287:             ok: true,
288:         });
289:         self.io.finish_round(
290:             target,
291:             model_call_id,
292:             data,
293:             cancellation,
294:             another_round_available,
295:         )
296:     }
297: 
298:     fn collect_model_round(
299:         &mut self,
300:         target: &ModelTarget,
301:         model_call_id: &str,
302:         request: ModelRequest,
303:         cancellation: &CancellationToken,
304:         progress: &mut ModelCallProgress,
305:     ) -> Result<ModelRoundData, SessionError> {
306:         let mut attempt = 0usize;
307:         loop {
308:             progress.provider_neutral_progress = false;
309:             let error = match self.collect_model_round_attempt(
310:                 target,
311:                 model_call_id,
312:                 request.clone(),
313:                 cancellation,
314:                 progress,
315:             ) {
316:                 Ok(data) => return Ok(data),
317:                 Err(AttemptFailure::Session(error)) => return Err(error),
318:                 Err(AttemptFailure::Provider(error)) => error,
319:             };
320:             let retryable = provider_failure_is_retryable(
321:                 &error,
322:                 progress.provider_neutral_progress,
323:                 attempt,
324:                 self.config.provider_retries,
325:             );
326:             if !retryable {
327:                 let observed = self
328:                     .io
329:                     .flush_response_checkpoints(model_call_id)?
330:                     .or_else(|| {
331:                         (progress.observed_output_bytes > 0)
332:                             .then_some(progress.observed_output_bytes)
333:                     });
334:                 self.io
335:                     .emit_provider_error(&error, model_call_id.to_owned(), observed)?;
336:                 self.io.flush_events();
337:                 return Err(error.into());
338:             }
339:             let backoff_ms = self
340:                 .config
341:                 .provider_retry_backoff_ms
342:                 .get(attempt)
343:                 .or(self.config.provider_retry_backoff_ms.last())
344:                 .copied()
345:                 .unwrap_or(0);
346:             attempt += 1;
347:             ProviderRuntimeContext::new(
348:                 self.io.session_id(),
349:                 target,
350:                 self.io.provider_runtime_scope(),
351:                 self.io.provider_runtime_observer(),
352:             )
353:             .retry_scheduled(
354:                 &error,
355:                 u64::try_from(attempt).unwrap_or(u64::MAX),
356:                 backoff_ms,
357:             );
358:             sleep_with_cancel(backoff_ms, cancellation)?;
359:         }
360:     }
361: 
362:     /// One provider invocation and stream drain. Provider failures are
363:     /// returned WITHOUT emitting an error event so the caller can decide
364:     /// between a silent retry and the terminal emit-then-fail path.
365:     /// `provider_neutral_progress` reports whether visible/readable model
366:     /// output, a tool call, or a finished record was observed. Empty deltas
367:     /// and provider-opaque artifacts do not make automatic replay unsafe.
368:     fn collect_model_round_attempt(
369:         &mut self,
370:         target: &ModelTarget,
371:         model_call_id: &str,
372:         request: ModelRequest,
373:         cancellation: &CancellationToken,
374:         progress: &mut ModelCallProgress,
375:     ) -> Result<ModelRoundData, AttemptFailure> {
376:         let mut stream = match self.io.invoke_model(target, request) {
377:             Ok(stream) => stream,
378:             Err(error) => return Err(AttemptFailure::Provider(error)),
379:         };
380:         let mut data = ModelRoundData::default();
381: 
382:         loop {
383:             if cancellation.is_cancelled() {
384:                 return Err(AttemptFailure::Session(SessionError::Cancelled));
385:             }
386:             let Some(event) = stream.next() else { break };
387:             if cancellation.is_cancelled() {
388:                 return Err(AttemptFailure::Session(SessionError::Cancelled));
389:             }
390:             let event = match event {
391:                 Ok(event) => event,
392:                 Err(error) => return Err(AttemptFailure::Provider(error)),
393:             };
394:             progress.provider_neutral_progress |= event.is_provider_neutral_progress();
395:             if let ModelStreamEvent::TextDelta(delta) = &event {
396:                 progress.observed_output_bytes = progress
397:                     .observed_output_bytes
398:                     .saturating_add(u64::try_from(delta.len()).unwrap_or(u64::MAX));
399:             }
400:             self.io
401:                 .after_stream_event(&event, model_call_id)
402:                 .map_err(AttemptFailure::Session)?;
403:             collect_stream_event(event, &mut data);
404:         }
405: 
406:         if cancellation.is_cancelled() {
407:             return Err(AttemptFailure::Session(SessionError::Cancelled));
408:         }
409:         if data.stop_reason.is_none() {
410:             return Err(AttemptFailure::Provider(ProviderError::stream_truncation(
411:                 "provider stream ended before finished event",
412:             )));
413:         }
414:         Ok(data)
415:     }
416: }
417: 
418: #[derive(Default)]
419: struct ModelCallProgress {
420:     provider_neutral_progress: bool,
```

/private/tmp/euler-wt/pr-213/crates/euler-core/src/session/round_loop.rs:432-457
```rust
432: pub(super) fn provider_failure_is_retryable(
433:     error: &ProviderError,
434:     provider_neutral_progress: bool,
435:     attempt: usize,
436:     provider_retries: usize,
437: ) -> bool {
438:     if error.timeout_stage() == Some(ProviderTimeoutStage::SemanticIdle) {
439:         return false;
440:     }
441:     matches!(
442:         error.category(),
443:         ProviderErrorCategory::Transport | ProviderErrorCategory::RateLimit
444:     ) && !provider_neutral_progress
445:         && attempt < provider_retries
446: }
447: 
448: #[cfg(test)]
449: mod tests {
450:     use super::*;
451:     use euler_provider::{ReasoningEffort, ToolCall};
452:     use euler_sdk::CancellationSource;
453:     use serde_json::json;
454:     use std::time::Duration;
455: 
456:     #[test]
457:     fn semantic_idle_timeout_is_never_retried() {
```

/private/tmp/euler-wt/pr-213/crates/euler-core/src/session/compaction_worker.rs:132-215
```rust
132:         let _ = sender.send(outcome);
133:     });
134:     CompactionWorker {
135:         receiver,
136:         cancellation,
137:         attempt_terminal_observed,
138:         handle: Some(handle),
139:     }
140: }
141: 
142: fn invoke_with_retries(
143:     providers: &ProviderSet,
144:     target: &ModelTarget,
145:     request: &ModelRequest,
146:     config: &ProviderRunConfig,
147:     cancellation: &CancellationToken,
148: ) -> WorkerOutcome {
149:     let mut attempt = 0usize;
150:     loop {
151:         if cancellation.is_cancelled() {
152:             return WorkerOutcome::Cancelled;
153:         }
154:         let mut provider_neutral_progress = false;
155:         match collect_round(
156:             providers,
157:             target,
158:             request.clone(),
159:             &mut provider_neutral_progress,
160:             cancellation,
161:             config,
162:         ) {
163:             Ok(Some(data)) => return WorkerOutcome::Finished(Ok(data)),
164:             Ok(None) => return WorkerOutcome::Cancelled,
165:             Err(error)
166:                 if provider_failure_is_retryable(
167:                     &error,
168:                     provider_neutral_progress,
169:                     attempt,
170:                     config.retries,
171:                 ) =>
172:             {
173:                 let delay = config
174:                     .retry_backoff_ms
175:                     .get(attempt)
176:                     .or_else(|| config.retry_backoff_ms.last())
177:                     .copied()
178:                     .unwrap_or(0);
179:                 ProviderRuntimeContext::new(
180:                     &config.session_id,
181:                     target,
182:                     ProviderRuntimeScope::Compaction,
183:                     &config.runtime_observer,
184:                 )
185:                 .retry_scheduled(
186:                     &error,
187:                     u64::try_from(attempt.saturating_add(1)).unwrap_or(u64::MAX),
188:                     delay,
189:                 );
190:                 if !wait_backoff(Duration::from_millis(delay), cancellation) {
191:                     return WorkerOutcome::Cancelled;
192:                 }
193:                 attempt += 1;
194:             }
195:             Err(error) => return WorkerOutcome::Finished(Err(error)),
196:         }
197:     }
198: }
199: 
200: fn wait_backoff(delay: Duration, cancellation: &CancellationToken) -> bool {
201:     let deadline = Instant::now() + delay;
202:     while Instant::now() < deadline {
203:         if cancellation.is_cancelled() {
204:             return false;
205:         }
206:         std::thread::sleep(
207:             CANCEL_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
208:         );
209:     }
210:     !cancellation.is_cancelled()
211: }
212: 
213: fn collect_round(
214:     providers: &ProviderSet,
215:     target: &ModelTarget,
```
