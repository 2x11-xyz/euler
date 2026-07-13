//! Semantic graph revision commit path shared by observer entry points.

use super::{
    input_error, split_update_events, write_projection_artifact, ObserveInput, SCHEMA_NAME,
};
use crate::active_state::ActiveGraphState;
use crate::construction::{
    Construction, ConstructionOperation, ConstructionPolicy, ConstructionTrigger,
};
use crate::observer_brief::listed_events;
use crate::projection::Projection;
use crate::slot_summary::{publish_graph_slot, with_slot_publication};
use euler_event::EventEnvelope;
use euler_sdk::{ExtensionError, HostApi, ProvenancePage};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// Shared hints-folding path for `observe` (operator-provided hints file),
/// `observer-apply` (round observer), and explicit `refresh` revisions.
pub(crate) fn execute_observe_projection(
    host: &dyn HostApi,
    input: &ObserveInput,
    command: &'static str,
    commit: ObservationCommit,
) -> Result<Value, ExtensionError> {
    let mut page = host.query_provenance(input.query())?;
    if let Some(watermark) = &input.watermark_event_id {
        cut_page_at_watermark(&mut page, watermark, input.after_event_id.as_deref())?;
    } else if page.truncated {
        return Err(input_error(format!(
            "causal-dag {command} requires a complete bounded event page"
        )));
    }
    let revision_events = revision_events(&page, &commit)?;
    let source_events = &revision_events.source_events;
    let active = ActiveGraphState::load(host)?;
    if source_events.is_empty() && active.is_none() {
        return Err(input_error(format!(
            "causal-dag {command} requires a non-empty bounded event page"
        )));
    }
    commit.validate_predecessor(active.as_ref())?;
    commit.validate_cursor(active.as_ref(), input.after_event_id.as_deref())?;
    let construction = commit.construction(active.as_ref());
    let projection = Projection::from_observer_revision(
        source_events,
        &input.hints,
        input.session_id.as_deref(),
        active.as_ref(),
        construction,
    )?;
    let source_event_ids = artifact_source_event_ids(&projection, source_events, &commit);
    let cited_source_event_count = source_event_ids.len();
    let source_event_count = source_events.len();
    let record = write_projection_artifact(host, &projection, &page, source_event_ids)?;
    let active_cursor_event_id = page
        .watermark_event_id
        .as_deref()
        .or(input.after_event_id.as_deref());
    let active = ActiveGraphState::commit(
        host,
        &record,
        projection.artifact_value(),
        active_cursor_event_id,
    )?;
    let slot_publication = publish_graph_slot(host, &projection);

    Ok(with_slot_publication(
        json!({
            "schema": SCHEMA_NAME,
            "command": command,
            "persisted_event_id": record.persisted_event_id,
            "relative_path": record.relative_path,
            "sha256": record.sha256,
            "byte_len": record.byte_len,
            "source_event_count": source_event_count,
            "cited_source_event_count": cited_source_event_count,
            "ignored_event_count": revision_events.ignored_event_count,
            "node_count": projection.node_count(),
            "edge_count": projection.edge_count(),
            "degraded": projection.degraded(),
            "truncated": page.truncated,
            "applied_limit": page.applied_limit,
            "applied_scan_limit": page.applied_scan_limit,
            "scanned_events": page.scanned_events,
            "next_after_event_id": page.next_after_event_id,
            "watermark_event_id": projection.watermark_event_id(),
            "query_watermark_event_id": page.watermark_event_id,
            "checkpoint_after_event_id": Value::Null,
            "construction": projection.construction_value(),
            "active_artifact_event_id": active.artifact_event_id(),
            "active_artifact_sha256": active.artifact_sha256(),
            "active_cursor_event_id": active.cursor_event_id(),
        }),
        slot_publication,
    ))
}

struct RevisionEvents {
    source_events: Vec<EventEnvelope>,
    ignored_event_count: usize,
}

fn revision_events(
    page: &ProvenancePage,
    commit: &ObservationCommit,
) -> Result<RevisionEvents, ExtensionError> {
    if commit.uses_observer_visibility() {
        let source_events = listed_events(&page.events)?;
        return Ok(RevisionEvents {
            ignored_event_count: page.events.len() - source_events.len(),
            source_events,
        });
    }
    let split = split_update_events(&page.events)?;
    Ok(RevisionEvents {
        ignored_event_count: split.ignored_self_events(),
        source_events: split.source_events,
    })
}

fn artifact_source_event_ids(
    projection: &Projection,
    source_events: &[EventEnvelope],
    commit: &ObservationCommit,
) -> Vec<String> {
    let mut source_event_ids = cited_source_event_ids(projection, source_events);
    if let Some(observer_result_event_id) = commit.observer_result_event_id() {
        if !source_event_ids
            .iter()
            .any(|event_id| event_id == observer_result_event_id)
        {
            source_event_ids.push(observer_result_event_id.to_owned());
        }
    }
    source_event_ids
}

#[derive(Clone, Debug)]
pub(crate) enum ObservationCommit {
    ManualReframe,
    Rolling {
        expected_predecessor_artifact_event_id: Option<String>,
        observer_result_event_id: String,
    },
    Explicit {
        operation: ConstructionOperation,
        policy: ConstructionPolicy,
        trigger: ConstructionTrigger,
        expected_predecessor_artifact_event_id: Option<String>,
        observer_result_event_id: String,
    },
}

impl ObservationCommit {
    fn uses_observer_visibility(&self) -> bool {
        !matches!(self, Self::ManualReframe)
    }

    fn construction(&self, active: Option<&ActiveGraphState>) -> Construction {
        match self {
            Self::ManualReframe => Construction::explicit(
                ConstructionOperation::Reframe,
                ConstructionPolicy::Manual,
                ConstructionTrigger::ExplicitReframe,
                active,
                None,
            ),
            Self::Rolling {
                observer_result_event_id,
                ..
            } => Construction::rolling(active, Some(observer_result_event_id.clone())),
            Self::Explicit {
                operation,
                policy,
                trigger,
                observer_result_event_id,
                ..
            } => Construction::explicit(
                *operation,
                *policy,
                *trigger,
                active,
                Some(observer_result_event_id.clone()),
            ),
        }
    }

    fn observer_result_event_id(&self) -> Option<&str> {
        match self {
            Self::ManualReframe => None,
            Self::Rolling {
                observer_result_event_id,
                ..
            }
            | Self::Explicit {
                observer_result_event_id,
                ..
            } => Some(observer_result_event_id),
        }
    }

    fn validate_predecessor(
        &self,
        active: Option<&ActiveGraphState>,
    ) -> Result<(), ExtensionError> {
        let expected_predecessor_artifact_event_id = match self {
            Self::Rolling {
                expected_predecessor_artifact_event_id,
                ..
            }
            | Self::Explicit {
                expected_predecessor_artifact_event_id,
                ..
            } => expected_predecessor_artifact_event_id,
            Self::ManualReframe => return Ok(()),
        };
        let actual = active.map(ActiveGraphState::artifact_event_id);
        if expected_predecessor_artifact_event_id.as_deref() != actual {
            return Err(input_error(
                "causal-dag active graph changed between observer brief and apply",
            ));
        }
        Ok(())
    }

    fn validate_cursor(
        &self,
        active: Option<&ActiveGraphState>,
        after_event_id: Option<&str>,
    ) -> Result<(), ExtensionError> {
        if matches!(self, Self::ManualReframe) {
            return Ok(());
        }
        let expected = active.map(ActiveGraphState::cursor_event_id);
        if after_event_id != expected {
            return Err(input_error(
                "causal-dag revision cursor does not match the active graph watermark",
            ));
        }
        Ok(())
    }
}

/// Rewrite the command-local page to the exact window observed by the model.
fn cut_page_at_watermark(
    page: &mut ProvenancePage,
    watermark: &str,
    after_event_id: Option<&str>,
) -> Result<(), ExtensionError> {
    if after_event_id == Some(watermark) {
        page.events.clear();
        page.scanned_events = 0;
        page.watermark_event_id = Some(watermark.to_owned());
        page.next_after_event_id = None;
        page.truncated = false;
        return Ok(());
    }
    if page.events.is_empty() && page.watermark_event_id.as_deref() == Some(watermark) {
        page.next_after_event_id = None;
        page.truncated = false;
        return Ok(());
    }
    let Some(index) = page.events.iter().position(|event| event.id == watermark) else {
        return Err(input_error(format!(
            "causal-dag observe watermark_event_id `{watermark}` was not reached in the bounded provenance page"
        )));
    };
    page.events.truncate(index + 1);
    page.watermark_event_id = Some(watermark.to_owned());
    page.next_after_event_id = None;
    page.truncated = false;
    Ok(())
}

fn cited_source_event_ids(
    projection: &Projection,
    current_events: &[EventEnvelope],
) -> Vec<String> {
    let cited = projection.cited_event_ids();
    let current_ids = current_events
        .iter()
        .map(|event| event.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut ordered = cited
        .iter()
        .filter(|event_id| !current_ids.contains(event_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    ordered.extend(
        current_events
            .iter()
            .filter(|event| cited.contains(&event.id))
            .map(|event| event.id.clone()),
    );
    ordered
}
