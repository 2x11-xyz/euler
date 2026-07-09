use euler_core::permissions::{DeciderVerdict, PermissionRequest};
use euler_core::PermissionDecider;
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Debug)]
pub struct TuiDecider {
    request_tx: Sender<PermissionRequest>,
    reply_rx: Receiver<PermissionReply>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionReply {
    Allow,
    Deny,
    AllowAll,
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
            PermissionReply::Allow => DeciderVerdict::Allow,
            PermissionReply::Deny => DeciderVerdict::Deny,
            PermissionReply::AllowAll => DeciderVerdict::AllowSession,
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
            .send(PermissionReply::Allow)
            .expect("reply");

        assert_eq!(handle.join().expect("join"), DeciderVerdict::Allow);
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
