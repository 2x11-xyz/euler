use super::{
    assessment_verdict_label, entity_kind_label, entity_lifecycle_label, graph_kind, node_id,
    outcome_label, source_refs, Placement,
};
use crate::research_record::{
    AcceptedRecord, AssessmentVerdict, EntityKind, ResearchAssessment, ResearchEntity,
    ResearchOutcome,
};
use euler_sdk::ExtensionError;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
struct ClaimAssessmentState {
    status: &'static str,
    summary: Option<String>,
    contested: bool,
    scope_limited: bool,
    scope_count: usize,
}

pub(super) fn node_value(
    accepted: &AcceptedRecord,
    entity_id: &str,
    placement: &Placement,
    event_kinds: &BTreeMap<String, String>,
) -> Result<Value, ExtensionError> {
    let entity = accepted.entity(entity_id)?;
    let assessments = if entity.kind == EntityKind::Claim {
        accepted.active_assessments_for(&entity.id)
    } else {
        Vec::new()
    };
    let assessment_state = claim_assessment_state(&assessments);
    let outcome = (entity.kind == EntityKind::Investigation)
        .then(|| accepted.latest_outcome_for(&entity.id))
        .flatten();
    let (status, summary) = node_status_and_summary(entity, outcome, &assessment_state);
    let source_event_ids = node_source_event_ids(entity, outcome, &assessments);
    let source_refs = source_refs(
        &format!("src-node-{}", entity.id),
        &source_event_ids,
        event_kinds,
    );
    let source_ref_ids = source_refs
        .iter()
        .filter_map(|source| source["id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    Ok(json!({
        "id": node_id(&entity.id),
        "root_id": node_id(&placement.root_entity_id),
        "kind": graph_kind(entity.kind),
        "status": status,
        "title": entity.title,
        "summary": summary,
        "source_refs": source_refs,
        "basis": {
            "kind": "direct",
            "summary": "Accepted research-record entity.",
            "source_ref_ids": source_ref_ids
        },
        "metadata": node_metadata(entity, outcome, &assessments, &assessment_state)
    }))
}

fn node_status_and_summary(
    entity: &ResearchEntity,
    outcome: Option<&ResearchOutcome>,
    assessment_state: &ClaimAssessmentState,
) -> (&'static str, String) {
    match entity.kind {
        EntityKind::Question => ("open", entity.summary.clone()),
        EntityKind::Investigation => match outcome {
            Some(outcome) => (
                match outcome.outcome {
                    crate::research_record::InvestigationOutcome::Active => "open",
                    crate::research_record::InvestigationOutcome::Blocked => "blocked",
                    crate::research_record::InvestigationOutcome::DeadEnd => "dead_end",
                    crate::research_record::InvestigationOutcome::Completed => "success",
                    crate::research_record::InvestigationOutcome::Abandoned => "abandoned",
                },
                format!(
                    "Outcome ({}): {}\n{}",
                    outcome_label(outcome.outcome),
                    outcome.summary,
                    entity.summary
                ),
            ),
            None => ("open", entity.summary.clone()),
        },
        EntityKind::Observation | EntityKind::Artifact => ("success", entity.summary.clone()),
        EntityKind::Synthesis => ("verified", entity.summary.clone()),
        EntityKind::Claim => match &assessment_state.summary {
            Some(summary) => (
                assessment_state.status,
                format!("{summary}\n{}", entity.summary),
            ),
            None => ("open", entity.summary.clone()),
        },
    }
}

fn claim_assessment_state(assessments: &[&ResearchAssessment]) -> ClaimAssessmentState {
    if assessments.is_empty() {
        return ClaimAssessmentState {
            status: "open",
            summary: None,
            contested: false,
            scope_limited: false,
            scope_count: 0,
        };
    }
    let mut groups = BTreeMap::<&str, Vec<&ResearchAssessment>>::new();
    for assessment in assessments {
        groups
            .entry(assessment.scope.as_str())
            .or_default()
            .push(*assessment);
    }
    let contested_scopes = groups
        .iter()
        .filter_map(|(scope, group)| group_is_contested(group).then_some(*scope))
        .collect::<Vec<_>>();
    if !contested_scopes.is_empty() {
        return ClaimAssessmentState {
            status: "inconclusive",
            summary: Some(format!(
                "Contested active assessments at scope(s): {}. Inspect the scoped assessment metadata.",
                contested_scopes.join(", ")
            )),
            contested: true,
            scope_limited: groups.len() > 1,
            scope_count: groups.len(),
        };
    }
    if groups.len() > 1 {
        return ClaimAssessmentState {
            status: "inconclusive",
            summary: Some(format!(
                "Active assessments cover {} distinct scopes. Inspect the scoped assessment metadata.",
                groups.len()
            )),
            contested: false,
            scope_limited: true,
            scope_count: groups.len(),
        };
    }
    let assessment = groups
        .values()
        .next()
        .and_then(|group| selected_assessment(group))
        .expect("non-empty assessment groups contain an assessment");
    ClaimAssessmentState {
        status: match assessment.verdict {
            AssessmentVerdict::Supported
            | AssessmentVerdict::Corroborated
            | AssessmentVerdict::Proven => "verified",
            AssessmentVerdict::Refuted => "dead_end",
            AssessmentVerdict::Inconclusive => "inconclusive",
        },
        summary: Some(format!(
            "Assessment ({}, scope: {}): {}",
            assessment_verdict_label(assessment.verdict),
            assessment.scope,
            assessment.summary
        )),
        contested: false,
        scope_limited: false,
        scope_count: 1,
    }
}

fn group_is_contested(group: &[&ResearchAssessment]) -> bool {
    group
        .iter()
        .any(|assessment| is_positive(assessment.verdict))
        && group
            .iter()
            .any(|assessment| assessment.verdict == AssessmentVerdict::Refuted)
}

fn selected_assessment<'a>(group: &'a [&'a ResearchAssessment]) -> Option<&'a ResearchAssessment> {
    group.iter().copied().max_by(|left, right| {
        assessment_priority(left.verdict)
            .cmp(&assessment_priority(right.verdict))
            .then_with(|| right.id.cmp(&left.id))
    })
}

fn assessment_priority(verdict: AssessmentVerdict) -> u8 {
    match verdict {
        AssessmentVerdict::Proven => 5,
        AssessmentVerdict::Corroborated => 4,
        AssessmentVerdict::Supported => 3,
        AssessmentVerdict::Refuted => 2,
        AssessmentVerdict::Inconclusive => 1,
    }
}

fn is_positive(verdict: AssessmentVerdict) -> bool {
    matches!(
        verdict,
        AssessmentVerdict::Supported | AssessmentVerdict::Corroborated | AssessmentVerdict::Proven
    )
}

fn node_metadata(
    entity: &ResearchEntity,
    outcome: Option<&ResearchOutcome>,
    assessments: &[&ResearchAssessment],
    assessment_state: &ClaimAssessmentState,
) -> Value {
    let active_assessments = assessments
        .iter()
        .map(|value| assessment_value(value))
        .collect::<Vec<_>>();
    json!({
        "entity_id": entity.id,
        "entity_kind": entity_kind_label(entity.kind),
        "lifecycle": entity.lifecycle.map(entity_lifecycle_label),
        "investigation_outcome": outcome.map(|value| json!({
            "id": value.id,
            "outcome": outcome_label(value.outcome),
            "summary": value.summary,
            "supersedes_outcome_id": value.supersedes_outcome_id
        })),
        "active_assessments": active_assessments,
        "assessment_presentation": {
            "contested": assessment_state.contested,
            "scope_limited": assessment_state.scope_limited,
            "scope_count": assessment_state.scope_count
        }
    })
}

fn assessment_value(assessment: &ResearchAssessment) -> Value {
    json!({
        "id": assessment.id,
        "verdict": assessment_verdict_label(assessment.verdict),
        "scope": assessment.scope,
        "standard": assessment.standard,
        "summary": assessment.summary,
        "supersedes_assessment_id": assessment.supersedes_assessment_id
    })
}

fn node_source_event_ids(
    entity: &ResearchEntity,
    outcome: Option<&ResearchOutcome>,
    assessments: &[&ResearchAssessment],
) -> Vec<String> {
    let mut ids = entity
        .source_event_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if let Some(outcome) = outcome {
        ids.extend(outcome.source_event_ids.iter().cloned());
    }
    for assessment in assessments {
        ids.extend(assessment.source_event_ids.iter().cloned());
    }
    ids.into_iter().collect()
}
