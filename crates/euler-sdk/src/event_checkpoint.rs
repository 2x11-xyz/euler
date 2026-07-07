use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const EVENT_FEED_CHECKPOINT_SCHEMA_VERSION: u16 = 1;
pub const MAX_EVENT_FEED_CHECKPOINT_BYTES: usize = 4096;
pub const MAX_EVENT_FEED_CHECKPOINT_CURSOR_BYTES: usize = 128;
pub const MAX_EVENT_FEED_CHECKPOINT_NAME_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventFeedCheckpoint {
    pub schema_version: u16,
    pub after_event_id: String,
}

impl EventFeedCheckpoint {
    pub fn new(after_event_id: impl Into<String>) -> Result<Self, EventFeedCheckpointError> {
        let checkpoint = Self {
            schema_version: EVENT_FEED_CHECKPOINT_SCHEMA_VERSION,
            after_event_id: after_event_id.into(),
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    pub fn validate(&self) -> Result<(), EventFeedCheckpointError> {
        if self.schema_version != EVENT_FEED_CHECKPOINT_SCHEMA_VERSION {
            return Err(EventFeedCheckpointError::UnsupportedSchemaVersion);
        }
        if valid_event_feed_cursor(&self.after_event_id) {
            Ok(())
        } else {
            Err(EventFeedCheckpointError::InvalidCursor)
        }
    }

    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, EventFeedCheckpointError> {
        if bytes.is_empty() {
            return Err(EventFeedCheckpointError::CorruptJson);
        }
        if bytes.len() > MAX_EVENT_FEED_CHECKPOINT_BYTES {
            return Err(EventFeedCheckpointError::TooLarge);
        }
        let checkpoint = serde_json::from_slice::<Self>(bytes)
            .map_err(|_| EventFeedCheckpointError::CorruptJson)?;
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    pub fn to_json_bytes(&self) -> Result<Vec<u8>, EventFeedCheckpointError> {
        self.validate()?;
        let mut bytes =
            serde_json::to_vec(self).map_err(|_| EventFeedCheckpointError::CorruptJson)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum EventFeedCheckpointError {
    #[error("corrupt checkpoint json")]
    CorruptJson,
    #[error("invalid checkpoint cursor")]
    InvalidCursor,
    #[error("unsupported checkpoint schema version")]
    UnsupportedSchemaVersion,
    #[error("checkpoint file is too large")]
    TooLarge,
}

pub fn valid_checkpoint_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    let edge = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    (1..=MAX_EVENT_FEED_CHECKPOINT_NAME_BYTES).contains(&bytes.len())
        && bytes.first().is_some_and(edge)
        && bytes.last().is_some_and(edge)
        && bytes.iter().all(|byte| edge(byte) || *byte == b'-')
}

pub fn valid_event_feed_cursor(value: &str) -> bool {
    (1..=MAX_EVENT_FEED_CHECKPOINT_CURSOR_BYTES).contains(&value.len())
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_feed_checkpoint_round_trips_v1_json() {
        let checkpoint = EventFeedCheckpoint::new("01HXEXAMPLECURSOR").expect("checkpoint");
        let bytes = checkpoint.to_json_bytes().expect("serialize");

        assert_eq!(
            EventFeedCheckpoint::from_json_bytes(&bytes).expect("deserialize"),
            checkpoint
        );
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["schema_version"], json!(1));
        assert_eq!(json["after_event_id"], json!("01HXEXAMPLECURSOR"));
    }

    #[test]
    fn event_feed_checkpoint_rejects_invalid_cursor_shape() {
        for cursor in ["", "has space", "line\nbreak", "snowman-☃", "\u{7f}"] {
            assert!(EventFeedCheckpoint::new(cursor).is_err());
        }
        assert!(
            EventFeedCheckpoint::new("x".repeat(MAX_EVENT_FEED_CHECKPOINT_CURSOR_BYTES + 1))
                .is_err()
        );
    }

    #[test]
    fn event_feed_checkpoint_accepts_cursor_boundaries() {
        assert!(EventFeedCheckpoint::new("x").is_ok());
        assert!(
            EventFeedCheckpoint::new("x".repeat(MAX_EVENT_FEED_CHECKPOINT_CURSOR_BYTES)).is_ok()
        );
    }

    #[test]
    fn event_feed_checkpoint_json_decode_is_strict() {
        assert_eq!(
            EventFeedCheckpoint::from_json_bytes(b""),
            Err(EventFeedCheckpointError::CorruptJson)
        );
        assert_eq!(
            EventFeedCheckpoint::from_json_bytes(
                br#"{"schema_version":2,"after_event_id":"cursor"}"#
            ),
            Err(EventFeedCheckpointError::UnsupportedSchemaVersion)
        );
        assert_eq!(
            EventFeedCheckpoint::from_json_bytes(
                br#"{"schema_version":1,"after_event_id":"cursor","extra":true}"#
            ),
            Err(EventFeedCheckpointError::CorruptJson)
        );
        assert_eq!(
            EventFeedCheckpoint::from_json_bytes(&vec![b' '; MAX_EVENT_FEED_CHECKPOINT_BYTES + 1]),
            Err(EventFeedCheckpointError::TooLarge)
        );
    }

    #[test]
    fn checkpoint_names_use_frozen_portable_grammar() {
        assert!(valid_checkpoint_name("a"));
        assert!(valid_checkpoint_name(&format!(
            "a{}0",
            "b".repeat(MAX_EVENT_FEED_CHECKPOINT_NAME_BYTES - 2)
        )));
        for name in [
            "",
            "-bad",
            "bad-",
            "Bad",
            "bad_name",
            "bad.name",
            "bad/name",
            "bad name",
            "snowman-☃",
        ] {
            assert!(!valid_checkpoint_name(name), "{name}");
        }
    }
}
