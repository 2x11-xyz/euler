use super::{
    input_error, valid_id, valid_source_ids, valid_text, validate_assessment_fields,
    validate_relation_endpoints, AcceptedRecord, EntityKind, InvestigationOutcome, RelationKind,
    ResearchEntity, ResearchRelation, MAX_SUMMARY_BYTES,
};
use euler_sdk::ExtensionError;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn validate_record_semantics(accepted: &AcceptedRecord) -> Result<(), ExtensionError> {
    if !accepted
        .entities
        .values()
        .any(|entity| entity.kind == EntityKind::Question)
    {
        return Err(input_error(
            "research record needs at least one accepted question",
        ));
    }
    validate_outcome_history(accepted)?;
    validate_semantic_fields(accepted)?;
    validate_assessment_history(accepted)?;
    for relation in accepted.relations.values() {
        validate_relation_endpoints(relation, &accepted.entities)?;
        validate_lineage_relation(relation, accepted)?;
    }
    validate_syntheses(accepted)?;
    for investigation in accepted
        .entities
        .values()
        .filter(|entity| entity.kind == EntityKind::Investigation)
    {
        let investigates_question = accepted.relations.values().any(|relation| {
            relation.kind == RelationKind::Investigates
                && relation.from == investigation.id
                && accepted
                    .entities
                    .get(&relation.to)
                    .is_some_and(|target| target.kind == EntityKind::Question)
        });
        if !investigates_question {
            return Err(input_error(format!(
                "investigation `{}` needs an investigates relation to a question",
                investigation.id
            )));
        }
    }
    for assessment in accepted.assessments.values() {
        if accepted
            .entities
            .get(&assessment.claim_id)
            .map(|entity| entity.kind)
            != Some(EntityKind::Claim)
        {
            return Err(input_error(
                "research assessment target is no longer a claim",
            ));
        }
    }
    Ok(())
}

fn validate_syntheses(accepted: &AcceptedRecord) -> Result<(), ExtensionError> {
    for synthesis in accepted
        .entities
        .values()
        .filter(|entity| entity.kind == EntityKind::Synthesis)
    {
        let inputs = accepted
            .relations
            .values()
            .filter(|relation| {
                relation.kind == RelationKind::Integrates && relation.from == synthesis.id
            })
            .map(|relation| relation.to.as_str())
            .collect::<BTreeSet<_>>();
        if inputs.len() < 2 {
            return Err(input_error(
                "research synthesis needs at least two distinct accepted inputs",
            ));
        }
        let addresses_question = accepted.relations.values().any(|relation| {
            relation.kind == RelationKind::Addresses
                && relation.from == synthesis.id
                && accepted
                    .entities
                    .get(&relation.to)
                    .is_some_and(|entity| entity.kind == EntityKind::Question)
        });
        if !addresses_question {
            return Err(input_error(
                "research synthesis needs an addresses relation to a question",
            ));
        }
    }
    Ok(())
}

fn validate_semantic_fields(accepted: &AcceptedRecord) -> Result<(), ExtensionError> {
    for entity in accepted.entities.values() {
        if !valid_id(&entity.id)
            || !valid_text(&entity.title, super::MAX_TITLE_BYTES)
            || !valid_text(&entity.summary, MAX_SUMMARY_BYTES)
            || !valid_source_ids(&entity.source_event_ids)
        {
            return Err(input_error("research-record entity is invalid"));
        }
    }
    for relation in accepted.relations.values() {
        if !valid_id(&relation.id)
            || !valid_id(&relation.from)
            || !valid_id(&relation.to)
            || relation.from == relation.to
            || !valid_text(&relation.summary, MAX_SUMMARY_BYTES)
            || !valid_source_ids(&relation.source_event_ids)
        {
            return Err(input_error("research-record relation is invalid"));
        }
    }
    for assessment in accepted.assessments.values() {
        validate_assessment_fields(assessment)?;
    }
    Ok(())
}

fn validate_outcome_history(accepted: &AcceptedRecord) -> Result<(), ExtensionError> {
    let mut current_by_investigation = BTreeMap::<String, String>::new();
    for outcome_id in &accepted.outcome_order {
        let outcome = accepted.outcomes.get(outcome_id).ok_or_else(|| {
            input_error("research-record outcome order references an unknown outcome")
        })?;
        if !valid_id(&outcome.id)
            || !valid_id(&outcome.investigation_id)
            || !valid_text(&outcome.summary, MAX_SUMMARY_BYTES)
            || !valid_source_ids(&outcome.source_event_ids)
            || outcome
                .supersedes_outcome_id
                .as_deref()
                .is_some_and(|id| !valid_id(id))
            || accepted
                .entities
                .get(&outcome.investigation_id)
                .map(|entity| entity.kind)
                != Some(EntityKind::Investigation)
        {
            return Err(input_error(
                "research-record investigation outcome is invalid",
            ));
        }
        let current = current_by_investigation.get(&outcome.investigation_id);
        if current.map(String::as_str) != outcome.supersedes_outcome_id.as_deref() {
            return Err(input_error(
                "research-record investigation outcome supersession is invalid",
            ));
        }
        current_by_investigation.insert(outcome.investigation_id.clone(), outcome.id.clone());
    }
    Ok(())
}

fn validate_assessment_history(accepted: &AcceptedRecord) -> Result<(), ExtensionError> {
    let mut seen = BTreeMap::<String, &super::ResearchAssessment>::new();
    let mut superseded = BTreeSet::<String>::new();
    for assessment_id in &accepted.assessment_order {
        let assessment = accepted.assessments.get(assessment_id).ok_or_else(|| {
            input_error("research-record assessment order references an unknown assessment")
        })?;
        if let Some(superseded_id) = assessment.supersedes_assessment_id.as_deref() {
            let prior = seen.get(superseded_id).ok_or_else(|| {
                input_error(
                    "research-record assessment supersession must name an earlier assessment",
                )
            })?;
            if prior.claim_id != assessment.claim_id || prior.scope != assessment.scope {
                return Err(input_error(
                    "research-record assessment supersession must preserve claim and exact scope",
                ));
            }
            if !superseded.insert(superseded_id.to_owned()) {
                return Err(input_error(
                    "research-record assessment supersession must name an active assessment",
                ));
            }
        }
        seen.insert(assessment.id.clone(), assessment);
    }
    Ok(())
}

fn validate_lineage_relation(
    relation: &ResearchRelation,
    accepted: &AcceptedRecord,
) -> Result<(), ExtensionError> {
    let requires_predecessor = matches!(
        relation.kind,
        RelationKind::Repairs | RelationKind::PivotsFrom | RelationKind::ContinuesFrom
    );
    if !requires_predecessor {
        return Ok(());
    }
    let successor = accepted.entity(&relation.from)?;
    let predecessor = accepted.entity(&relation.to)?;
    let predecessor_outcome = accepted
        .latest_outcome_for(&predecessor.id)
        .map(|outcome| outcome.outcome)
        .ok_or_else(|| {
            input_error("lineage predecessor needs an accepted investigation outcome")
        })?;
    let terminal = matches!(
        predecessor_outcome,
        InvestigationOutcome::Blocked | InvestigationOutcome::DeadEnd
    );
    match relation.kind {
        RelationKind::Repairs | RelationKind::PivotsFrom if !terminal => {
            return Err(input_error(
                "repair and pivot relations require a blocked or dead-end predecessor",
            ));
        }
        RelationKind::ContinuesFrom
            if matches!(
                predecessor_outcome,
                InvestigationOutcome::Blocked
                    | InvestigationOutcome::DeadEnd
                    | InvestigationOutcome::Abandoned
            ) =>
        {
            return Err(input_error(
                "continuation requires an active or completed productive predecessor",
            ));
        }
        _ => {}
    }
    if relation.kind == RelationKind::ContinuesFrom && !is_productive(predecessor, accepted) {
        return Err(input_error(
            "continuation requires a productive predecessor with an accepted output",
        ));
    }
    require_lineage_source_overlap(relation, predecessor, successor, accepted)
}

fn is_productive(investigation: &ResearchEntity, accepted: &AcceptedRecord) -> bool {
    accepted.relations.values().any(|relation| {
        relation.kind == RelationKind::Produces
            && relation.from == investigation.id
            && accepted.entities.get(&relation.to).is_some_and(|entity| {
                matches!(
                    entity.kind,
                    EntityKind::Observation | EntityKind::Artifact | EntityKind::Claim
                )
            })
    })
}

fn require_lineage_source_overlap(
    relation: &ResearchRelation,
    predecessor: &ResearchEntity,
    successor: &ResearchEntity,
    accepted: &AcceptedRecord,
) -> Result<(), ExtensionError> {
    let relation_sources = relation.source_event_ids.iter().collect::<BTreeSet<_>>();
    let predecessor_seen = investigation_material_sources(predecessor, accepted)
        .iter()
        .any(|source| relation_sources.contains(source));
    let successor_seen = investigation_material_sources(successor, accepted)
        .iter()
        .any(|source| relation_sources.contains(source));
    if predecessor_seen && successor_seen {
        Ok(())
    } else {
        Err(input_error(
            "research lineage relation must cite evidence from both predecessor and successor",
        ))
    }
}

fn investigation_material_sources(
    investigation: &ResearchEntity,
    accepted: &AcceptedRecord,
) -> BTreeSet<String> {
    let mut sources = investigation
        .source_event_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if let Some(outcome) = accepted.latest_outcome_for(&investigation.id) {
        sources.extend(outcome.source_event_ids.iter().cloned());
    }
    for relation in accepted.relations.values().filter(|relation| {
        relation.kind == RelationKind::Produces && relation.from == investigation.id
    }) {
        sources.extend(relation.source_event_ids.iter().cloned());
        if let Some(produced) = accepted.entities.get(&relation.to) {
            sources.extend(produced.source_event_ids.iter().cloned());
        }
    }
    sources
}
