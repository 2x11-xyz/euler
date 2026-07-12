use crate::active_state::ActiveGraphState;
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConstructionOperation {
    Snapshot,
    Incremental,
    Reframe,
    Final,
}

impl ConstructionOperation {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Incremental => "incremental",
            Self::Reframe => "reframe",
            Self::Final => "final",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConstructionPolicy {
    Manual,
    RollingOnly,
    RollingAndFinal,
    FinalOnly,
}

impl ConstructionPolicy {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::RollingOnly => "rolling_only",
            Self::RollingAndFinal => "rolling_and_final",
            Self::FinalOnly => "final_only",
        }
    }

    pub(super) fn from_active(active: Option<&ActiveGraphState>) -> Self {
        match active.map(ActiveGraphState::policy) {
            Some("rolling_only") => Self::RollingOnly,
            Some("final_only") => Self::FinalOnly,
            Some("manual") => Self::Manual,
            _ => Self::RollingAndFinal,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConstructionTrigger {
    Command,
    RoundCadence,
    ExplicitReframe,
    SessionEnd,
}

impl ConstructionTrigger {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::RoundCadence => "round_cadence",
            Self::ExplicitReframe => "explicit_reframe",
            Self::SessionEnd => "session_end",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Construction {
    operation: ConstructionOperation,
    policy: ConstructionPolicy,
    trigger: ConstructionTrigger,
    predecessor_artifact_event_id: Option<String>,
    predecessor_watermark_event_id: Option<String>,
    observer_result_event_id: Option<String>,
}

impl Construction {
    pub(super) fn snapshot() -> Self {
        Self::new(
            ConstructionOperation::Snapshot,
            ConstructionPolicy::Manual,
            ConstructionTrigger::Command,
            None,
            None,
        )
    }

    pub(super) fn rolling(
        active: Option<&ActiveGraphState>,
        observer_result_event_id: Option<String>,
    ) -> Self {
        Self::new(
            if active.is_some() {
                ConstructionOperation::Incremental
            } else {
                ConstructionOperation::Reframe
            },
            ConstructionPolicy::from_active(active),
            ConstructionTrigger::RoundCadence,
            active,
            observer_result_event_id,
        )
    }

    pub(super) fn explicit(
        operation: ConstructionOperation,
        policy: ConstructionPolicy,
        trigger: ConstructionTrigger,
        active: Option<&ActiveGraphState>,
        observer_result_event_id: Option<String>,
    ) -> Self {
        Self::new(operation, policy, trigger, active, observer_result_event_id)
    }

    fn new(
        operation: ConstructionOperation,
        policy: ConstructionPolicy,
        trigger: ConstructionTrigger,
        active: Option<&ActiveGraphState>,
        observer_result_event_id: Option<String>,
    ) -> Self {
        Self {
            operation,
            policy,
            trigger,
            predecessor_artifact_event_id: active
                .map(ActiveGraphState::artifact_event_id)
                .map(str::to_owned),
            predecessor_watermark_event_id: active
                .map(ActiveGraphState::watermark_event_id)
                .map(str::to_owned),
            observer_result_event_id,
        }
    }

    pub(super) fn operation(&self) -> ConstructionOperation {
        self.operation
    }

    pub(super) fn to_value(&self) -> Value {
        json!({
            "operation": self.operation.as_str(),
            "policy": self.policy.as_str(),
            "trigger": self.trigger.as_str(),
            "predecessor_artifact_event_id": self.predecessor_artifact_event_id,
            "predecessor_watermark_event_id": self.predecessor_watermark_event_id,
            "observer_result_event_id": self.observer_result_event_id,
        })
    }
}
