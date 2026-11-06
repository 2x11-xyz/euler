use euler_core::permissions::{DeciderVerdict, PermissionRequest, PermissionRequestBatch};
use euler_core::{GrantScope, PermissionDecider, PermissionDecisionOutcome, ScopePattern};
use euler_sdk::CancellationToken;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

#[derive(Debug)]
pub struct TuiDecider {
    request_tx: Sender<PermissionPromptEnvelope>,
}

/// A single capability request or an operation-level request batch waiting for
/// an answer from the terminal UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionPrompt {
    Request(Box<PermissionRequest>),
    Batch(PermissionRequestBatch),
}

impl From<PermissionRequest> for PermissionPrompt {
    fn from(request: PermissionRequest) -> Self {
        Self::Request(Box::new(request))
    }
}

/// UI → decider reply for an approval panel decision.
///
/// Scope strings are opaque patterns already derived by the UI. Empty means
/// unscoped (whole capability) and is only honest when the panel labeled
/// whole-capability. Invalid (control/oversize) patterns must **not** broaden
/// to unscoped — they fall back to allow-once at the decider boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionReply {
    AllowOnce,
    AllowSessionScope(String),
    AllowProjectScope(String),
    /// Durable user rule ("always"). Unlike session/project scopes, empty is
    /// never honest here — the panel only offers `u` with a derived prefix —
    /// so an empty pattern falls back to allow-once, not unscoped.
    AllowUserScope(String),
    Deny,
    DenyWithInstruction(String),
}

/// One prompt plus its unique reply route and cancellation identity.
///
/// A fresh one-shot channel per prompt prevents a late reply from an old
/// modal from satisfying a later ask. The UI observes the same token as the
/// waiting decider and drops a prompt that is already stale.
#[derive(Clone, Debug)]
pub struct PermissionPromptEnvelope {
    pub prompt: PermissionPrompt,
    pub cancellation: CancellationToken,
    pub reply_tx: Sender<PermissionReply>,
}

#[derive(Debug)]
pub struct PermissionChannels {
    pub request_rx: Receiver<PermissionPromptEnvelope>,
}

impl TuiDecider {
    pub fn new() -> (Self, PermissionChannels) {
        let (request_tx, request_rx) = mpsc::channel();
        (Self { request_tx }, PermissionChannels { request_rx })
    }
}

impl PermissionDecider for TuiDecider {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        self.decide_prompt(PermissionPrompt::from(request.clone()))
    }

    fn decide_batch(&mut self, batch: &PermissionRequestBatch) -> DeciderVerdict {
        self.decide_prompt(PermissionPrompt::Batch(batch.clone()))
    }

    fn decide_cancellable(
        &mut self,
        request: &PermissionRequest,
        cancellation: &CancellationToken,
    ) -> PermissionDecisionOutcome<DeciderVerdict> {
        self.decide_prompt_cancellable(
            PermissionPrompt::from(request.clone()),
            cancellation.clone(),
        )
    }

    fn decide_batch_cancellable(
        &mut self,
        batch: &PermissionRequestBatch,
        cancellation: &CancellationToken,
    ) -> PermissionDecisionOutcome<DeciderVerdict> {
        self.decide_prompt_cancellable(PermissionPrompt::Batch(batch.clone()), cancellation.clone())
    }
}

impl TuiDecider {
    fn decide_prompt(&mut self, prompt: PermissionPrompt) -> DeciderVerdict {
        match self.decide_prompt_cancellable(prompt, CancellationToken::new()) {
            PermissionDecisionOutcome::Decided(verdict) => verdict,
            PermissionDecisionOutcome::Cancelled | PermissionDecisionOutcome::TimedOut => {
                unreachable!("a private never-cancelled TUI prompt cannot cancel or time out")
            }
        }
    }

    fn decide_prompt_cancellable(
        &mut self,
        prompt: PermissionPrompt,
        cancellation: CancellationToken,
    ) -> PermissionDecisionOutcome<DeciderVerdict> {
        if cancellation.is_cancelled() {
            return PermissionDecisionOutcome::Cancelled;
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .request_tx
            .send(PermissionPromptEnvelope {
                prompt,
                cancellation: cancellation.clone(),
                reply_tx,
            })
            .is_err()
        {
            return PermissionDecisionOutcome::Decided(DeciderVerdict::Deny);
        }
        loop {
            if cancellation.is_cancelled() {
                return PermissionDecisionOutcome::Cancelled;
            }
            match reply_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(_) if cancellation.is_cancelled() => {
                    return PermissionDecisionOutcome::Cancelled;
                }
                Ok(reply) => {
                    return PermissionDecisionOutcome::Decided(reply_to_verdict(reply));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return PermissionDecisionOutcome::Decided(DeciderVerdict::Deny);
                }
            }
        }
    }
}

fn reply_to_verdict(reply: PermissionReply) -> DeciderVerdict {
    match reply {
        PermissionReply::AllowOnce => DeciderVerdict::Allow,
        PermissionReply::AllowSessionScope(pattern) => match ScopePattern::new(pattern) {
            Ok(pattern) => DeciderVerdict::AllowScoped(GrantScope::Session(pattern)),
            // Invalid pattern never broadens to whole-capability; allow once.
            Err(_) => DeciderVerdict::Allow,
        },
        PermissionReply::AllowProjectScope(pattern) => match ScopePattern::new(pattern) {
            Ok(pattern) => DeciderVerdict::AllowScoped(GrantScope::Project(pattern)),
            Err(_) => DeciderVerdict::Allow,
        },
        PermissionReply::AllowUserScope(pattern) => {
            if pattern.is_empty() {
                // A user rule is never unscoped: empty would broaden a prefix
                // rule to the whole capability forever.
                return DeciderVerdict::Allow;
            }
            match ScopePattern::new(pattern) {
                Ok(pattern) => DeciderVerdict::AllowScoped(GrantScope::User(pattern)),
                Err(_) => DeciderVerdict::Allow,
            }
        }
        PermissionReply::Deny => DeciderVerdict::Deny,
        PermissionReply::DenyWithInstruction(text) => DeciderVerdict::DenyWithInstruction(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_core::{Session, SessionConfig};
    use euler_event::EventKind;
    use euler_provider::ScriptedProvider;
    use euler_sdk::Capability;
    use std::thread;
    use std::time::Duration;

    fn request() -> PermissionRequest {
        PermissionRequest::new(Capability::FsWrite, "edit file".to_owned())
    }

    fn next_prompt(channels: &PermissionChannels) -> PermissionPromptEnvelope {
        channels.request_rx.recv().expect("request")
    }

    #[test]
    fn decide_sends_request_and_returns_reply() {
        let (mut decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || decider.decide(&request()));

        let sent = next_prompt(&channels);
        assert_eq!(sent.prompt, PermissionPrompt::from(request()));
        sent.reply_tx
            .send(PermissionReply::AllowOnce)
            .expect("reply");

        assert_eq!(handle.join().expect("join"), DeciderVerdict::Allow);
    }

    #[test]
    fn extension_operation_arrives_as_one_tui_batch_and_records_each_capability() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().to_owned();
        let (decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || {
            let mut session = Session::new(
                SessionConfig::new(root),
                ScriptedProvider::new(Vec::new()),
                decider,
            );
            let result = session.approve_extension_capabilities(
                "example",
                "run",
                &[Capability::FsWrite, Capability::Network],
            );
            (result, session.events().to_vec())
        });

        let prompt = channels
            .request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("one operation prompt");
        let PermissionPrompt::Batch(batch) = &prompt.prompt else {
            panic!("extension capabilities must use one batch prompt");
        };
        assert_eq!(batch.operation(), "extension example.run");
        assert_eq!(
            batch.capabilities().collect::<Vec<_>>(),
            vec![Capability::FsWrite, Capability::Network]
        );
        prompt
            .reply_tx
            .send(PermissionReply::AllowSessionScope(String::new()))
            .expect("reply");

        let (result, events) = handle.join().expect("join");
        result.expect("allow operation");
        let decisions = events
            .iter()
            .filter(|event| event.kind.as_str() == EventKind::PERMISSION_DECISION)
            .collect::<Vec<_>>();
        assert_eq!(decisions.len(), 2);
        assert!(decisions
            .iter()
            .all(|event| event.payload["allowed"] == true));
        assert!(decisions
            .iter()
            .all(|event| event.payload["scope"] == "session"));
    }

    #[test]
    fn decide_maps_scoped_session_and_project() {
        let (mut decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || {
            let session = decider.decide(&request());
            let project = decider.decide(&request());
            (session, project)
        });

        next_prompt(&channels)
            .reply_tx
            .send(PermissionReply::AllowSessionScope("cargo".into()))
            .expect("reply");
        next_prompt(&channels)
            .reply_tx
            .send(PermissionReply::AllowProjectScope("src".into()))
            .expect("reply");

        let (session, project) = handle.join().expect("join");
        assert_eq!(
            session,
            DeciderVerdict::AllowScoped(GrantScope::Session(
                ScopePattern::new("cargo").expect("pattern")
            ))
        );
        assert_eq!(
            project,
            DeciderVerdict::AllowScoped(GrantScope::Project(
                ScopePattern::new("src").expect("pattern")
            ))
        );
    }

    #[test]
    fn decide_maps_user_scope_and_never_broadens_it() {
        let (mut decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || {
            let user = decider.decide(&request());
            let empty = decider.decide(&request());
            let control = decider.decide(&request());
            (user, empty, control)
        });

        next_prompt(&channels)
            .reply_tx
            .send(PermissionReply::AllowUserScope("cargo".into()))
            .expect("reply");
        next_prompt(&channels)
            .reply_tx
            .send(PermissionReply::AllowUserScope(String::new()))
            .expect("reply");
        next_prompt(&channels)
            .reply_tx
            .send(PermissionReply::AllowUserScope("cargo\u{0001}".into()))
            .expect("reply");

        let (user, empty, control) = handle.join().expect("join");
        assert_eq!(
            user,
            DeciderVerdict::AllowScoped(GrantScope::User(
                ScopePattern::new("cargo").expect("pattern")
            ))
        );
        // Empty would be an unscoped ("whole capability forever") rule and
        // invalid patterns must not broaden either: both degrade to once.
        assert_eq!(empty, DeciderVerdict::Allow);
        assert_eq!(control, DeciderVerdict::Allow);
    }

    #[test]
    fn decide_maps_deny_with_instruction() {
        let (mut decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || decider.decide(&request()));

        next_prompt(&channels)
            .reply_tx
            .send(PermissionReply::DenyWithInstruction(
                "use apply_patch".into(),
            ))
            .expect("reply");

        assert_eq!(
            handle.join().expect("join"),
            DeciderVerdict::DenyWithInstruction("use apply_patch".into())
        );
    }

    #[test]
    fn decide_denies_when_reply_channel_closes() {
        let (mut decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || decider.decide(&request()));

        let prompt = next_prompt(&channels);
        drop(prompt.reply_tx);

        assert_eq!(handle.join().expect("join"), DeciderVerdict::Deny);
    }

    #[test]
    fn cancellation_releases_prompt_and_late_reply_cannot_poison_next_ask() {
        let (mut decider, channels) = TuiDecider::new();
        let cancellation = euler_sdk::CancellationSource::new();
        let token = cancellation.token();
        let handle = thread::spawn(move || {
            let cancelled = decider.decide_cancellable(&request(), &token);
            let next = decider.decide(&request());
            (cancelled, next)
        });

        let stale = channels
            .request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first prompt");
        cancellation.cancel();
        let next = channels
            .request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("next prompt after cancellation");
        assert!(
            stale.reply_tx.send(PermissionReply::AllowOnce).is_err(),
            "the cancelled prompt's one-shot receiver must be gone"
        );
        next.reply_tx
            .send(PermissionReply::Deny)
            .expect("reply to current prompt");

        let (cancelled, next) = handle.join().expect("join");
        assert_eq!(cancelled, PermissionDecisionOutcome::Cancelled);
        assert_eq!(next, DeciderVerdict::Deny);
    }

    /// A gate whose execution profile sets `ask_timeout` (ADR 0027 section
    /// 4), asking through `decider`.
    fn timed_gate<D: PermissionDecider>(
        decider: D,
        ask_timeout: Option<u32>,
    ) -> euler_core::permissions::PermissionGate<D> {
        let mut fields = euler_core::ExecutionProfile::default().fields;
        fields.ask_timeout = ask_timeout;
        let mut gate = euler_core::permissions::PermissionGate::new(decider);
        gate.set_execution_profile(fields, false);
        gate
    }

    fn ask(
        gate: &mut euler_core::permissions::PermissionGate<impl PermissionDecider>,
        cancellation: &CancellationToken,
    ) -> PermissionDecisionOutcome<bool> {
        match gate.decide_detailed_cancellable(
            &request(),
            euler_core::ApprovalMode::Ask,
            cancellation,
        ) {
            PermissionDecisionOutcome::Decided(decision) => {
                PermissionDecisionOutcome::Decided(decision.allowed())
            }
            PermissionDecisionOutcome::Cancelled => PermissionDecisionOutcome::Cancelled,
            PermissionDecisionOutcome::TimedOut => PermissionDecisionOutcome::TimedOut,
        }
    }

    fn wait_until_closed(prompt: &PermissionPromptEnvelope) {
        let start = std::time::Instant::now();
        while !prompt.cancellation.is_cancelled() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "prompt never closed"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// ADR 0027 section 4: when `ask_timeout` passes, the prompt closes and
    /// the ask times out. An answer after that is discarded, installs no
    /// grant, and cannot answer the next prompt.
    #[test]
    fn a_timeout_closes_the_prompt_and_a_late_answer_is_discarded() {
        let (decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || {
            let mut gate = timed_gate(decider, Some(1));
            let turn = CancellationToken::new();
            let timed_out = ask(&mut gate, &turn);
            let next = ask(&mut gate, &turn);
            (timed_out, next, gate.is_granted(&request()))
        });

        let stale = next_prompt(&channels);
        wait_until_closed(&stale);
        let next = channels
            .request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("next prompt after the timeout");
        assert!(
            stale
                .reply_tx
                .send(PermissionReply::AllowSessionScope(String::new()))
                .is_err(),
            "the timed-out prompt's one-shot receiver must be gone"
        );
        assert!(!next.cancellation.is_cancelled());
        next.reply_tx
            .send(PermissionReply::AllowOnce)
            .expect("reply to the current prompt");

        let (timed_out, next, granted) = handle.join().expect("join");
        assert_eq!(timed_out, PermissionDecisionOutcome::TimedOut);
        assert_eq!(next, PermissionDecisionOutcome::Decided(true));
        assert!(!granted, "the late session answer installed no grant");
    }

    /// A cancellation before the timeout stays a cancellation, and closes
    /// the prompt.
    #[test]
    fn a_cancellation_before_the_timeout_stays_a_cancellation() {
        let (decider, channels) = TuiDecider::new();
        let cancellation = euler_sdk::CancellationSource::new();
        let turn = cancellation.token();
        let handle = thread::spawn(move || ask(&mut timed_gate(decider, Some(60)), &turn));

        let prompt = next_prompt(&channels);
        cancellation.cancel();
        wait_until_closed(&prompt);
        assert_eq!(
            handle.join().expect("join"),
            PermissionDecisionOutcome::Cancelled
        );
    }

    /// Section 4: each pending ask has its own timer at its own effective
    /// timeout. One child's ask times out at its 1 second; the next child's
    /// ask, with no timeout, stays open past that and takes its own answer,
    /// not the late answer to the first.
    #[test]
    fn another_childs_ask_keeps_its_own_timer() {
        let (mut decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || {
            let turn = CancellationToken::new();
            let first = ask(&mut timed_gate(&mut decider, Some(1)), &turn);
            let second = ask(&mut timed_gate(&mut decider, None), &turn);
            (first, second)
        });

        let first = next_prompt(&channels);
        wait_until_closed(&first);
        let second = channels
            .request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("second child's prompt");
        thread::sleep(Duration::from_millis(1500));
        assert!(
            !second.cancellation.is_cancelled(),
            "the first ask's timer does not close the second prompt"
        );
        assert!(first.reply_tx.send(PermissionReply::AllowOnce).is_err());
        second
            .reply_tx
            .send(PermissionReply::Deny)
            .expect("reply to the second prompt");

        let (first, second) = handle.join().expect("join");
        assert_eq!(first, PermissionDecisionOutcome::TimedOut);
        assert_eq!(second, PermissionDecisionOutcome::Decided(false));
    }

    #[test]
    fn control_bearing_or_oversize_scope_does_not_become_session_wide_grant() {
        let (mut decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || {
            let control = decider.decide(&request());
            let oversize = decider.decide(&request());
            (control, oversize)
        });

        next_prompt(&channels)
            .reply_tx
            .send(PermissionReply::AllowSessionScope("cargo\u{0001}".into()))
            .expect("reply");
        next_prompt(&channels)
            .reply_tx
            .send(PermissionReply::AllowProjectScope(
                "x".repeat(euler_core::MAX_SCOPE_PATTERN_BYTES + 1),
            ))
            .expect("reply");

        let (control, oversize) = handle.join().expect("join");
        // Allow once — never AllowScoped(Session/Project unscoped).
        assert_eq!(control, DeciderVerdict::Allow);
        assert_eq!(oversize, DeciderVerdict::Allow);
    }
}
