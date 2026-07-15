use euler_core::permissions::{DeciderVerdict, PermissionRequest, PermissionRequestBatch};
use euler_core::{GrantScope, PermissionDecider, ScopePattern};
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Debug)]
pub struct TuiDecider {
    request_tx: Sender<PermissionPrompt>,
    reply_rx: Receiver<PermissionReply>,
}

/// A single capability request or an operation-level request batch waiting for
/// an answer from the terminal UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionPrompt {
    Request(PermissionRequest),
    Batch(PermissionRequestBatch),
}

impl From<PermissionRequest> for PermissionPrompt {
    fn from(request: PermissionRequest) -> Self {
        Self::Request(request)
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

#[derive(Debug)]
pub struct PermissionChannels {
    pub request_rx: Receiver<PermissionPrompt>,
    pub reply_tx: Sender<PermissionReply>,
}

impl TuiDecider {
    pub fn new() -> (Self, PermissionChannels) {
        let (request_tx, request_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        (
            Self {
                request_tx,
                reply_rx,
            },
            PermissionChannels {
                request_rx,
                reply_tx,
            },
        )
    }
}

impl PermissionDecider for TuiDecider {
    fn decide(&mut self, request: &PermissionRequest) -> DeciderVerdict {
        self.decide_prompt(PermissionPrompt::Request(request.clone()))
    }

    fn decide_batch(&mut self, batch: &PermissionRequestBatch) -> DeciderVerdict {
        self.decide_prompt(PermissionPrompt::Batch(batch.clone()))
    }
}

impl TuiDecider {
    fn decide_prompt(&mut self, prompt: PermissionPrompt) -> DeciderVerdict {
        if self.request_tx.send(prompt).is_err() {
            return DeciderVerdict::Deny;
        }
        match self.reply_rx.recv().unwrap_or(PermissionReply::Deny) {
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
                    // A user rule is never unscoped: empty would broaden a
                    // prefix rule to the whole capability forever.
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

    #[test]
    fn decide_sends_request_and_returns_reply() {
        let (mut decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || decider.decide(&request()));

        let sent = channels.request_rx.recv().expect("request");
        assert_eq!(sent, PermissionPrompt::Request(request()));
        channels
            .reply_tx
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
        let PermissionPrompt::Batch(batch) = prompt else {
            panic!("extension capabilities must use one batch prompt");
        };
        assert_eq!(batch.operation(), "extension example.run");
        assert_eq!(
            batch.capabilities().collect::<Vec<_>>(),
            vec![Capability::FsWrite, Capability::Network]
        );
        channels
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

        let _ = channels.request_rx.recv().expect("request");
        channels
            .reply_tx
            .send(PermissionReply::AllowSessionScope("cargo".into()))
            .expect("reply");
        let _ = channels.request_rx.recv().expect("request");
        channels
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

        let _ = channels.request_rx.recv().expect("request");
        channels
            .reply_tx
            .send(PermissionReply::AllowUserScope("cargo".into()))
            .expect("reply");
        let _ = channels.request_rx.recv().expect("request");
        channels
            .reply_tx
            .send(PermissionReply::AllowUserScope(String::new()))
            .expect("reply");
        let _ = channels.request_rx.recv().expect("request");
        channels
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

        let _ = channels.request_rx.recv().expect("request");
        channels
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

        let _ = channels.request_rx.recv().expect("request");
        drop(channels.reply_tx);

        assert_eq!(handle.join().expect("join"), DeciderVerdict::Deny);
    }

    #[test]
    fn control_bearing_or_oversize_scope_does_not_become_session_wide_grant() {
        let (mut decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || {
            let control = decider.decide(&request());
            let oversize = decider.decide(&request());
            (control, oversize)
        });

        let _ = channels.request_rx.recv().expect("request");
        channels
            .reply_tx
            .send(PermissionReply::AllowSessionScope("cargo\u{0001}".into()))
            .expect("reply");
        let _ = channels.request_rx.recv().expect("request");
        channels
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
