//! OS notifications for Warm Ledger §5.7.
//!
//! Privacy: bodies are fixed generic strings — never paths, secrets, or
//! user/model content. Emit only when the terminal is unfocused and only for
//! the four allowed events.

/// Exactly four notification events. Nothing else may call into this module
/// for user-visible notify.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotifyEvent {
    TurnDone,
    ApprovalNeeded,
    Failure,
    Stall,
}

impl NotifyEvent {
    /// Privacy-limited body. Keep short and generic.
    pub fn body(self) -> &'static str {
        match self {
            Self::TurnDone => "euler — turn done",
            Self::ApprovalNeeded => "euler — approval needed",
            Self::Failure => "euler — failure",
            Self::Stall => "euler — stall",
        }
    }
}

/// OSC 9 desktop notification (`ESC ] 9 ; <body> ESC \`) with BEL fallback.
pub fn notification_sequence(event: NotifyEvent) -> String {
    let body = event.body();
    debug_assert!(!body.as_bytes().contains(&0x1b));
    debug_assert!(!body.as_bytes().contains(&0x07));
    format!("\x1b]9;{body}\x1b\\\x07")
}

/// Stall threshold: no turn output for this long → one stall notify.
pub const STALL_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_are_generic_and_bounded() {
        for event in [
            NotifyEvent::TurnDone,
            NotifyEvent::ApprovalNeeded,
            NotifyEvent::Failure,
            NotifyEvent::Stall,
        ] {
            let body = event.body();
            assert!(body.starts_with("euler — "));
            assert!(body.len() < 40);
            assert!(!body.contains('/'));
            assert!(!body.contains('\\'));
        }
    }

    #[test]
    fn osc9_sequence_shape() {
        let seq = notification_sequence(NotifyEvent::TurnDone);
        assert!(seq.starts_with("\x1b]9;euler — turn done\x1b\\"));
        assert!(seq.ends_with('\x07'));
    }
}
