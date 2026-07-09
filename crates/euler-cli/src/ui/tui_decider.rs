use euler_core::permissions::{DeciderVerdict, PermissionRequest};
use euler_core::{GrantScope, PermissionDecider, ScopePattern};
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Debug)]
pub struct TuiDecider {
    request_tx: Sender<PermissionRequest>,
    reply_rx: Receiver<PermissionReply>,
}

/// UI → decider reply for an approval panel decision.
///
/// Scope strings are opaque patterns already derived by the UI. Empty means
/// unscoped (whole capability). Invalid patterns fall back to unscoped at the
/// boundary so the gate never receives a rejected `ScopePattern`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionReply {
    AllowOnce,
    AllowSessionScope(String),
    AllowProjectScope(String),
    Deny,
    DenyWithInstruction(String),
}

#[derive(Debug)]
pub struct PermissionChannels {
    pub request_rx: Receiver<PermissionRequest>,
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
        if self.request_tx.send(request.clone()).is_err() {
            return DeciderVerdict::Deny;
        }
        match self.reply_rx.recv().unwrap_or(PermissionReply::Deny) {
            PermissionReply::AllowOnce => DeciderVerdict::Allow,
            PermissionReply::AllowSessionScope(pattern) => {
                DeciderVerdict::AllowScoped(GrantScope::Session(scope_pattern(pattern)))
            }
            PermissionReply::AllowProjectScope(pattern) => {
                DeciderVerdict::AllowScoped(GrantScope::Project(scope_pattern(pattern)))
            }
            PermissionReply::Deny => DeciderVerdict::Deny,
            PermissionReply::DenyWithInstruction(text) => DeciderVerdict::DenyWithInstruction(text),
        }
    }
}

fn scope_pattern(raw: String) -> ScopePattern {
    ScopePattern::new(raw).unwrap_or_else(|_| ScopePattern::unscoped())
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_sdk::Capability;
    use std::thread;

    fn request() -> PermissionRequest {
        PermissionRequest::new(Capability::FsWrite, "edit file".to_owned())
    }

    #[test]
    fn decide_sends_request_and_returns_reply() {
        let (mut decider, channels) = TuiDecider::new();
        let handle = thread::spawn(move || decider.decide(&request()));

        let sent = channels.request_rx.recv().expect("request");
        assert_eq!(sent, request());
        channels
            .reply_tx
            .send(PermissionReply::AllowOnce)
            .expect("reply");

        assert_eq!(handle.join().expect("join"), DeciderVerdict::Allow);
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
}
