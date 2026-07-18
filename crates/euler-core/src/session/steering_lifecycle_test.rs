//! Lifecycle regressions for mid-turn steering (issue #146): persistence
//! failures must never discard queued input, and cancellation must win over
//! absorption.

use super::SteeringQueue;
use crate::permissions::ScriptedDecider;
use crate::provenance::ProvenanceWriter;
use crate::session::{RoundObserverConfig, Session};
use crate::SessionConfig;
use euler_event::EventKind;
use euler_provider::{FixtureResponse, ScriptedProvider, ToolCall};
use euler_sdk::{
    CommandContext, CommandRegistrar, ExtensionCommand, ExtensionError, ExtensionManifest, HostApi,
};
use serde_json::{json, Value};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;

/// Round observer whose brief command breaks the provenance log and then
/// queues steering. It runs in exactly the window between a round's last
/// persisted event and the next round's steering absorption, so the FIRST
/// emit to hit the broken log is the steering user.message — the failure
/// mode under test. Observer-chain emission failures themselves degrade
/// silently by design, which is what lets the turn reach absorption.
struct SabotageObserver {
    log_path: PathBuf,
    queue: Arc<SteeringQueue>,
}

struct SabotageBrief {
    log_path: PathBuf,
    queue: Arc<SteeringQueue>,
}

impl ExtensionCommand for SabotageBrief {
    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        self.queue.push_back("steer one".to_owned());
        self.queue.push_back("steer two".to_owned());
        std::fs::remove_file(&self.log_path).expect("remove log");
        std::fs::create_dir(&self.log_path).expect("block log path");
        Ok(json!({}))
    }
}

struct NoopApply;

impl ExtensionCommand for NoopApply {
    fn execute(
        &self,
        _context: CommandContext,
        _host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        Ok(json!({"applied": true}))
    }
}

impl euler_sdk::Extension for SabotageObserver {
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            id: "sabotage-observer".to_owned(),
            version: "0.1.0".to_owned(),
            display_name: "sabotage-observer".to_owned(),
            capabilities: Vec::new(),
        }
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        registrar.register_command(
            "brief",
            Box::new(SabotageBrief {
                log_path: self.log_path.clone(),
                queue: Arc::clone(&self.queue),
            }),
        );
        registrar.register_command("apply", Box::new(NoopApply));
        Ok(())
    }
}

fn tool_round() -> FixtureResponse {
    FixtureResponse::ToolCalls(vec![ToolCall {
        id: "call-read".to_owned(),
        name: "read_file".to_owned(),
        input: json!({"path": "note.txt"}),
    }])
}

#[test]
fn steering_emission_failure_keeps_every_unabsorbed_entry_queued() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log_path = temp.path().join("events.jsonl");
    let writer = ProvenanceWriter::new(log_path.clone()).expect("writer");
    std::fs::write(temp.path().join("note.txt"), "hello").expect("write note");
    let mut config = SessionConfig::new(temp.path());
    config.session_id = "session-steering-sabotage".to_owned();
    config
        .extensions_enabled
        .insert("sabotage-observer".to_owned());
    config.round_observer = Some(RoundObserverConfig {
        cadence_rounds: NonZeroU64::new(1).expect("nonzero cadence"),
        brief_command: "brief".to_owned(),
        apply_command: "apply".to_owned(),
    });
    let mut session = Session::new(
        config,
        ScriptedProvider::new(vec![
            tool_round(),
            FixtureResponse::Assistant("done".to_owned()),
        ]),
        ScriptedDecider::new(Vec::new()),
    )
    .with_provenance(writer);
    let queue = Arc::new(SteeringQueue::default());
    session.set_steering_queue(Arc::clone(&queue));
    session.set_observer_extension(Arc::new(SabotageObserver {
        log_path,
        queue: Arc::clone(&queue),
    }));

    let result = session.run_turn("start");

    // The steering emit hit the broken log and failed the turn — but no
    // queued input was lost: the failed entry and the one behind it are
    // both still queued (peek → emit → ack never acked), and neither made
    // it onto the bus as an absorbed user.message.
    assert!(result.is_err(), "turn must surface the persistence failure");
    assert_eq!(queue.snapshot(), vec!["steer one", "steer two"]);
    let steering_messages = session
        .events()
        .iter()
        .filter(|event| {
            event.kind.as_str() == EventKind::USER_MESSAGE
                && event
                    .payload
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.starts_with("steer"))
        })
        .count();
    assert!(
        steering_messages <= 1,
        "at most the failed entry may sit unpersisted on the bus"
    );
}
