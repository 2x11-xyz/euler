# Independent Claude Code review

Supplied-source review of #213 `ff70bb6` and #216 `67abc78`. No tools or mutations were permitted. Source-only reviewer concerns below require adjudication; this is the raw opinion, not the final finding list.

Read the [adjudication](claude-adjudication.md) for reachability checks, rejected scenarios, and the adopted simplification.

## #213 — share retry policy with shadow compaction

No correctness defects found. The substitution is behavior-preserving for the non-timeout cases (`Transport|RateLimit && !neutral_progress && attempt < retries`) and only adds the SemanticIdle veto, matching the round-loop. `provider_neutral_progress` is reset per attempt in `invoke_with_retries` (line 154), so the shared predicate sees the same input shape as the root loop. The stage table test exercises both retryable stages with a real second dispatch, which is the right shape. Doc scope update is consistent.

## #216 — require reopen after unresolved response persistence

### 1. Fence is skipped when a user admission is pending, and never restored — MEDIUM/HIGH
**File/fn:** `session.rs::run_model_rounds` (conjunct `io.session.pending_admission.is_none()`, ~2372).
**Trigger:** the round exits with an error while (a) the root response checkpoint is still open (no durable terminal for the `model.call`) and (b) a user admission is pending — e.g. a mid-turn steering admission whose candidate append became ambiguous while the response was still owned. The fence is not set; the caller then performs the sanctioned *exact retry* of the admission, which succeeds and clears `pending_admission`. Now neither fence is active, `can_accept_turn()` is true, and the next turn dispatches a new provider call while the first `model.call` still has no terminal — precisely the bug the patch targets.
**Why the exclusion is unsound:** the two fences are not alternatives. `pending_admission` protects one queue row; it says nothing about response ownership, and it is cleared by a successful retry. Reopen is required in this state regardless.
**Minimal fix:** drop the `pending_admission.is_none()` conjunct. Retrying the admission then fails with the reopen error, which is the correct outcome (the session must be reopened anyway). If the WouldBlock/InvalidData ordering matters to callers, keep `pending_admission_error()` precedence in `ensure_no_pending_admission` and set `terminalization_failed` unconditionally.
*(Source-only caveat: whether `admit_user_message` can be reached while `response_checkpoint.is_some()` is not visible here. If it provably cannot, the conjunct is dead code and should be deleted for the same reason.)*

### 2. Provenance-unresolved conjunct can under-fence a checkpoint-store failure — MEDIUM (source-only)
**File/fn:** `run_model_rounds`, conjunct `writer.has_unresolved_append() || persisted_events < bus.len()`.
**Trigger:** the failure originates in `ResponseCheckpoint::flush`/`observe_text` (mapped to `io::Error` at `flush_response_checkpoints:735` / `after_stream_event:750`) rather than in the provenance append. In that path no chunk events are pushed and the provenance writer is untouched, so the third conjunct is only true incidentally — via unpersisted `MODEL_DELTA` entries still sitting in the bus. If deltas are not bus-resident, or were flushed to the writer by the immediately preceding `emit_response_chunks`, the fence does not fire even though the call has no terminal and the visible prefix is unresolved; a later turn dispatches again.
**Minimal fix:** make the fence depend on response ownership, not on writer state: `if result.is_err() && io.response_checkpoint.is_some() { terminalization_failed = true }`. All error paths that produce a durable terminal (`emit_provider_error`, `emit_model_call_cancelled`, `finish_round:797`) already clear the checkpoint, so this does not over-fence provider/cancel failures — it only removes a bookkeeping-dependent hole. If you keep the conjunct, add the checkpoint store's own unresolved-write state to it.

### 3. Fence is set before the cancellation branch, converting `Cancelled` into `InvalidData` — LOW/MEDIUM
**File/fn:** `run_model_rounds`, ordering of the new block (2370-2379) versus `if matches!(&result, Err(SessionError::Cancelled)) { io.session.interrupt_compaction(...)?; io.sink.flush(...) }` (2380-2383).
**Trigger:** any `Err(Cancelled)` that reaches here with the checkpoint still open. `interrupt_compaction` appends, so it now hits `ensure_terminalization_intact` and fails; `?` replaces the user-visible `SessionError::Cancelled` with the reopen `InvalidData` error and skips the final `sink.flush`, so the UI loses the interrupt signal and the last accepted events.
**Minimal fix:** move the fence assignment below the cancellation block, or compute the boolean first and assign after `interrupt_compaction`/`flush`. (Reachability is source-only: `run_round:272-276` clears the checkpoint on a successful cancelled terminal, so this may currently be latent — but the ordering is fragile and free to correct.)

### Not repeated
The pre-`prepare_model_request` sync-failure gap (bus call with no dispatch and no terminal) is the known adjacent issue and is out of scope here.
