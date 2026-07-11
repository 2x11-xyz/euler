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
