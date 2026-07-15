use super::{
    valid_id, valid_source_ids, valid_text, AssessmentVerdict, EntityKind, Episode,
    EpisodeCaptureClass, LedgerEntry, ObserverProposalBatch, RelationKind, ResearchAssessment,
    ResearchEntity, ResearchOutcome, ResearchRecord, ResearchRelation, SemanticRecord,
    SourceContext, AUTO_ACCEPT_POLICY, MAX_SCOPE_BYTES, MAX_STANDARD_BYTES, MAX_SUMMARY_BYTES,
    MAX_TITLE_BYTES,
};
use crate::input_error;
use euler_event::{EventEnvelope, EventKind};
use euler_sdk::ExtensionError;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn validate_entity(
    entity: &ResearchEntity,
    sources: &SourceContext,
) -> Result<(), ExtensionError> {
    if !valid_id(&entity.id)
        || !valid_text(&entity.title, MAX_TITLE_BYTES)
        || !valid_text(&entity.summary, MAX_SUMMARY_BYTES)
    {
        return Err(input_error(
            "research observer entity has invalid text or id",
        ));
    }
    sources.validate(&entity.source_event_ids, "entity")
}

pub(super) fn validate_outcome(
    outcome: &ResearchOutcome,
    sources: &SourceContext,
    entities: &BTreeMap<String, ResearchEntity>,
    outcomes: &BTreeMap<String, ResearchOutcome>,
    outcome_order: &[String],
) -> Result<(), ExtensionError> {
    if !valid_id(&outcome.id)
        || !valid_id(&outcome.investigation_id)
        || !valid_text(&outcome.summary, MAX_SUMMARY_BYTES)
    {
        return Err(input_error(
            "research investigation outcome has invalid fields",
        ));
    }
    if entities
        .get(&outcome.investigation_id)
        .map(|entity| entity.kind)
        != Some(EntityKind::Investigation)
    {
        return Err(input_error(
            "research investigation outcome must target an existing investigation",
        ));
    }
    sources.validate(&outcome.source_event_ids, "investigation outcome")?;
    if !outcome
        .source_event_ids
        .first()
        .is_some_and(|anchor| sources.is_new(anchor))
    {
        return Err(input_error(
            "research investigation outcome must use a newly observed event as its first lineage anchor",
        ));
    }
    let current = outcome_order.iter().rev().find(|id| {
        outcomes
            .get(id.as_str())
            .is_some_and(|existing| existing.investigation_id == outcome.investigation_id)
    });
    match (current, outcome.supersedes_outcome_id.as_deref()) {
        (None, None) => Ok(()),
        (Some(current), Some(superseded)) if current == superseded => Ok(()),
        (None, Some(_)) => Err(input_error(
            "first investigation outcome must not supersede another outcome",
        )),
        (Some(_), None) => Err(input_error(
            "later investigation outcome must supersede the current outcome",
        )),
        (Some(_), Some(_)) => Err(input_error(
            "investigation outcome must supersede the current outcome for its target",
        )),
    }
}

pub(super) fn validate_relation_shape(
    relation: &ResearchRelation,
    sources: &SourceContext,
) -> Result<(), ExtensionError> {
    if !valid_id(&relation.id)
        || !valid_id(&relation.from)
        || !valid_id(&relation.to)
        || relation.from == relation.to
        || !valid_text(&relation.summary, MAX_SUMMARY_BYTES)
    {
        return Err(input_error("research observer relation has invalid fields"));
    }
    sources.validate(&relation.source_event_ids, "relation")
}

pub(super) fn validate_assessment(
    assessment: &ResearchAssessment,
    sources: &SourceContext,
    entities: &BTreeMap<String, ResearchEntity>,
) -> Result<(), ExtensionError> {
    validate_assessment_fields(assessment)?;
    if entities.get(&assessment.claim_id).map(|entity| entity.kind) != Some(EntityKind::Claim) {
        return Err(input_error(
            "research assessment must target an existing claim",
        ));
    }
    sources.validate(&assessment.source_event_ids, "assessment")
}

pub(super) fn validate_assessment_supersession(
    assessment: &ResearchAssessment,
    assessments: &BTreeMap<String, ResearchAssessment>,
) -> Result<(), ExtensionError> {
    let Some(superseded_id) = assessment.supersedes_assessment_id.as_deref() else {
        return Ok(());
    };
    let superseded = assessments.get(superseded_id).ok_or_else(|| {
        input_error("research assessment supersession must name an earlier accepted assessment")
    })?;
    if superseded.claim_id != assessment.claim_id || superseded.scope != assessment.scope {
        return Err(input_error(
            "research assessment supersession must preserve claim and exact scope",
        ));
    }
    if assessments
        .values()
        .any(|existing| existing.supersedes_assessment_id.as_deref() == Some(superseded_id))
    {
        return Err(input_error(
            "research assessment supersession must name an active assessment",
        ));
    }
    Ok(())
}

pub(super) fn validate_assessment_fields(
    assessment: &ResearchAssessment,
) -> Result<(), ExtensionError> {
    if !valid_id(&assessment.id)
        || !valid_id(&assessment.claim_id)
        || !valid_text(&assessment.scope, MAX_SCOPE_BYTES)
        || !valid_text(&assessment.standard, MAX_STANDARD_BYTES)
        || !valid_text(&assessment.summary, MAX_SUMMARY_BYTES)
        || assessment
            .supersedes_assessment_id
            .as_deref()
            .is_some_and(|id| !valid_id(id))
        || !valid_source_ids(&assessment.source_event_ids)
    {
        return Err(input_error("research assessment has invalid fields"));
    }
    let standard = assessment.standard.as_str();
    let compatible = match assessment.verdict {
        AssessmentVerdict::Proven => standard == "formal_proof",
        AssessmentVerdict::Refuted => matches!(standard, "counterexample" | "formal_proof"),
        AssessmentVerdict::Corroborated => standard == "replication",
        AssessmentVerdict::Supported | AssessmentVerdict::Inconclusive => matches!(
            standard,
            "formal_proof"
                | "counterexample"
                | "derivation"
                | "experiment"
                | "replication"
                | "measurement"
                | "benchmark"
                | "simulation"
                | "computation"
                | "argument"
                | "review"
        ),
    };
    if compatible {
        Ok(())
    } else {
        Err(input_error(
            "research assessment verdict is incompatible with its evidence standard",
        ))
    }
}

pub(super) fn validate_relation_endpoints(
    relation: &ResearchRelation,
    entities: &BTreeMap<String, ResearchEntity>,
) -> Result<(), ExtensionError> {
    let from = entities.get(&relation.from).ok_or_else(|| {
        input_error(format!(
            "research relation has unknown source `{}`",
            relation.from
        ))
    })?;
    let to = entities.get(&relation.to).ok_or_else(|| {
        input_error(format!(
            "research relation has unknown target `{}`",
            relation.to
        ))
    })?;
    let valid = match relation.kind {
        RelationKind::Investigates => {
            from.kind == EntityKind::Investigation
                && matches!(to.kind, EntityKind::Question | EntityKind::Claim)
        }
        RelationKind::Produces => {
            from.kind == EntityKind::Investigation
                && matches!(
                    to.kind,
                    EntityKind::Observation | EntityKind::Artifact | EntityKind::Claim
                )
        }
        RelationKind::EvidenceFor | RelationKind::EvidenceAgainst => {
            matches!(
                from.kind,
                EntityKind::Observation | EntityKind::Artifact | EntityKind::Investigation
            ) && to.kind == EntityKind::Claim
        }
        RelationKind::Repairs | RelationKind::PivotsFrom | RelationKind::ContinuesFrom => {
            from.kind == EntityKind::Investigation && to.kind == EntityKind::Investigation
        }
        RelationKind::Decomposes => {
            from.kind == EntityKind::Investigation && to.kind == EntityKind::Investigation
        }
        RelationKind::Addresses => {
            from.kind == EntityKind::Synthesis && to.kind == EntityKind::Question
        }
        RelationKind::Integrates => {
            from.kind == EntityKind::Synthesis
                && matches!(
                    to.kind,
                    EntityKind::Investigation | EntityKind::Claim | EntityKind::Observation
                )
        }
    };
    if !valid {
        return Err(input_error(format!(
            "research relation `{}` has incompatible endpoint kinds",
            relation.id
        )));
    }
    Ok(())
}

pub(super) fn append_new_episodes(
    record: &mut ResearchRecord,
    events: &[EventEnvelope],
) -> Result<(), ExtensionError> {
    let mut existing_sources = record
        .episodes
        .iter()
        .flat_map(|episode| episode.source_event_ids.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    for event in events {
        if !existing_sources.insert(event.id.clone()) {
            return Err(input_error(
                "research-record observer attempted to recapture an existing event",
            ));
        }
        record.episodes.push(Episode {
            id: episode_id(&event.id),
            capture_class: episode_capture_class(event),
            event_kind: event.kind.as_str().to_owned(),
            source_event_ids: vec![event.id.clone()],
        });
    }
    Ok(())
}

fn episode_id(event_id: &str) -> String {
    let digest = Sha256::digest(event_id.as_bytes());
    format!("episode-{digest:x}")
}

fn episode_capture_class(event: &EventEnvelope) -> EpisodeCaptureClass {
    match event.kind.as_str() {
        EventKind::USER_MESSAGE | EventKind::ASSISTANT_MESSAGE => EpisodeCaptureClass::Proposition,
        EventKind::TOOL_CALL | EventKind::CHECK_STARTED => EpisodeCaptureClass::Execution,
        EventKind::EXTENSION_ARTIFACT
        | EventKind::PATCH_APPLIED
        | EventKind::FILE_CHANGE
        | EventKind::FILE_DIFF => EpisodeCaptureClass::Artifact,
        EventKind::PLAN_UPDATE | EventKind::PERMISSION_DECISION => EpisodeCaptureClass::Decision,
        _ => EpisodeCaptureClass::Result,
    }
}

pub(super) fn append_batch_ledger(
    record: &mut ResearchRecord,
    batch: ObserverProposalBatch,
) -> Result<(), ExtensionError> {
    let mut proposals = batch
        .entities
        .into_iter()
        .map(SemanticRecord::Entity)
        .chain(
            batch
                .outcomes
                .into_iter()
                .map(SemanticRecord::InvestigationOutcome),
        )
        .chain(batch.relations.into_iter().map(SemanticRecord::Relation))
        .chain(
            batch
                .assessments
                .into_iter()
                .map(SemanticRecord::Assessment),
        )
        .collect::<Vec<_>>();
    proposals.sort_by(|left, right| left.id().cmp(right.id()));
    let mut existing_proposal_ids = record
        .ledger
        .iter()
        .filter_map(|entry| match entry {
            LedgerEntry::Proposal { id, .. } => Some(id.clone()),
            LedgerEntry::Decision { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    for semantic in proposals {
        let proposal_id = format!("proposal-{}", semantic.id());
        let decision_id = format!("decision-{}", semantic.id());
        if !valid_id(&proposal_id)
            || !valid_id(&decision_id)
            || !existing_proposal_ids.insert(proposal_id.clone())
        {
            return Err(input_error("research-record ledger id collision"));
        }
        let source_event_ids = semantic.source_event_ids().to_vec();
        record.ledger.push(LedgerEntry::Proposal {
            id: proposal_id.clone(),
            semantic,
        });
        record.ledger.push(LedgerEntry::Decision {
            id: decision_id,
            proposal_id,
            outcome: super::DecisionOutcome::Accepted,
            policy: AUTO_ACCEPT_POLICY.to_owned(),
            source_event_ids,
        });
    }
    Ok(())
}
