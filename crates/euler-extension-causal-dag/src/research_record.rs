//! Durable, append-only research-record semantics for the Causal-DAG pilot.
//!
//! This module deliberately separates what happened (episodes), what an
//! observer proposed, what a named policy accepted, and the later graph
//! projection. It owns no provider calls and does not inspect opaque model
//! reasoning.

use crate::input_error;
use euler_event::EventEnvelope;
use euler_sdk::ExtensionError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

mod semantics;
use semantics::validate_record_semantics;
mod validation;
use validation::{
    append_batch_ledger, append_new_episodes, validate_assessment, validate_assessment_fields,
    validate_assessment_supersession, validate_entity, validate_outcome,
    validate_relation_endpoints, validate_relation_shape,
};

pub(crate) const RESEARCH_RECORD_SCHEMA: &str = "euler.research_record.v1";
pub(crate) const RESEARCH_RECORD_MEDIA_TYPE: &str = "application/vnd.euler.research-record.v1+json";
pub(crate) const RESEARCH_PROPOSALS_SCHEMA: &str = "euler.research_record.proposals.v1";
pub(crate) const RESEARCH_DAG_SCHEMA: &str = "euler.causal_dag.v4";
pub(crate) const RESEARCH_DAG_MEDIA_TYPE: &str = "application/vnd.euler.causal-dag.v4+json";
pub(crate) const AUTO_ACCEPT_POLICY: &str = "source-grounded-auto-accept-v1";
pub(crate) const MAX_RESEARCH_RECORD_ARTIFACT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_RESEARCH_DAG_ARTIFACT_BYTES: usize = 2 * 1024 * 1024;

const MAX_ID_BYTES: usize = 96;
const MAX_TITLE_BYTES: usize = 280;
const MAX_SUMMARY_BYTES: usize = 1_200;
const MAX_SCOPE_BYTES: usize = 600;
const MAX_STANDARD_BYTES: usize = 120;
const MAX_SOURCE_REFS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResearchRecord {
    pub(crate) schema: String,
    pub(crate) media_type: String,
    pub(crate) generated_at: String,
    pub(crate) session: RecordSession,
    pub(crate) construction: RecordConstruction,
    pub(crate) episodes: Vec<Episode>,
    pub(crate) ledger: Vec<LedgerEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordSession {
    pub(crate) id: String,
    pub(crate) provenance_watermark_event_id: String,
    pub(crate) observed_through_event_id: String,
}

/// Snapshot-level provenance. The complete ledger is immutable; this header
/// says how this particular snapshot was constructed from its predecessor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordConstruction {
    pub(crate) operation: RecordOperation,
    pub(crate) predecessor_record_artifact_event_id: Option<String>,
    pub(crate) predecessor_record_watermark_event_id: Option<String>,
    pub(crate) proposal_source_event_ids: Vec<String>,
    pub(crate) observer_result_event_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordOperation {
    Capture,
    Reconcile,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Episode {
    pub(crate) id: String,
    pub(crate) capture_class: EpisodeCaptureClass,
    pub(crate) event_kind: String,
    pub(crate) source_event_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EpisodeCaptureClass {
    Proposition,
    Execution,
    Result,
    Artifact,
    Decision,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entry_kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum LedgerEntry {
    Proposal {
        id: String,
        semantic: SemanticRecord,
    },
    Decision {
        id: String,
        proposal_id: String,
        outcome: DecisionOutcome,
        policy: String,
        source_event_ids: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DecisionOutcome {
    Accepted,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum SemanticRecord {
    Entity(ResearchEntity),
    InvestigationOutcome(ResearchOutcome),
    Relation(ResearchRelation),
    Assessment(ResearchAssessment),
}

impl SemanticRecord {
    fn id(&self) -> &str {
        match self {
            Self::Entity(value) => &value.id,
            Self::InvestigationOutcome(value) => &value.id,
            Self::Relation(value) => &value.id,
            Self::Assessment(value) => &value.id,
        }
    }

    fn source_event_ids(&self) -> &[String] {
        match self {
            Self::Entity(value) => &value.source_event_ids,
            Self::InvestigationOutcome(value) => &value.source_event_ids,
            Self::Relation(value) => &value.source_event_ids,
            Self::Assessment(value) => &value.source_event_ids,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResearchEntity {
    pub(crate) id: String,
    pub(crate) kind: EntityKind,
    pub(crate) title: String,
    pub(crate) summary: String,
    pub(crate) lifecycle: Option<EntityLifecycle>,
    pub(crate) source_event_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntityKind {
    Question,
    Claim,
    Investigation,
    Observation,
    Artifact,
    Synthesis,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntityLifecycle {
    Draft,
    Active,
    Withdrawn,
    Archived,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InvestigationOutcome {
    Active,
    Blocked,
    DeadEnd,
    Completed,
    Abandoned,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResearchOutcome {
    pub(crate) id: String,
    pub(crate) investigation_id: String,
    pub(crate) outcome: InvestigationOutcome,
    pub(crate) summary: String,
    pub(crate) supersedes_outcome_id: Option<String>,
    pub(crate) source_event_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResearchRelation {
    pub(crate) id: String,
    pub(crate) kind: RelationKind,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) summary: String,
    pub(crate) source_event_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RelationKind {
    Investigates,
    Produces,
    EvidenceFor,
    EvidenceAgainst,
    Repairs,
    ContinuesFrom,
    PivotsFrom,
    Decomposes,
    Addresses,
    Integrates,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResearchAssessment {
    pub(crate) id: String,
    pub(crate) claim_id: String,
    pub(crate) scope: String,
    pub(crate) verdict: AssessmentVerdict,
    pub(crate) standard: String,
    pub(crate) summary: String,
    pub(crate) supersedes_assessment_id: Option<String>,
    pub(crate) source_event_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssessmentVerdict {
    Supported,
    Corroborated,
    Proven,
    Refuted,
    Inconclusive,
}

/// The observer's only semantic output format. It is intentionally proposal
/// shaped: applying it emits separate proposal and acceptance ledger entries.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObserverProposalBatch {
    pub(crate) schema: String,
    pub(crate) entities: Vec<ResearchEntity>,
    pub(crate) outcomes: Vec<ResearchOutcome>,
    pub(crate) relations: Vec<ResearchRelation>,
    pub(crate) assessments: Vec<ResearchAssessment>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcceptedRecord {
    pub(crate) entities: BTreeMap<String, ResearchEntity>,
    pub(crate) outcomes: BTreeMap<String, ResearchOutcome>,
    pub(crate) relations: BTreeMap<String, ResearchRelation>,
    pub(crate) assessments: BTreeMap<String, ResearchAssessment>,
    outcome_order: Vec<String>,
    assessment_order: Vec<String>,
}

impl ResearchRecord {
    pub(crate) fn new(
        session_id: String,
        watermark_event_id: String,
        generated_at: String,
    ) -> Self {
        Self {
            schema: RESEARCH_RECORD_SCHEMA.to_owned(),
            media_type: RESEARCH_RECORD_MEDIA_TYPE.to_owned(),
            generated_at,
            session: RecordSession {
                id: session_id,
                provenance_watermark_event_id: watermark_event_id.clone(),
                observed_through_event_id: watermark_event_id,
            },
            construction: RecordConstruction {
                operation: RecordOperation::Capture,
                predecessor_record_artifact_event_id: None,
                predecessor_record_watermark_event_id: None,
                proposal_source_event_ids: Vec::new(),
                observer_result_event_id: None,
            },
            episodes: Vec::new(),
            ledger: Vec::new(),
        }
    }

    pub(crate) fn from_value(value: &Value) -> Result<Self, ExtensionError> {
        let record = serde_json::from_value::<Self>(value.clone())
            .map_err(|error| input_error(format!("invalid research record: {error}")))?;
        record.validate_shape()?;
        let accepted = record.accepted()?;
        validate_record_semantics(&accepted)?;
        Ok(record)
    }

    pub(crate) fn value(&self) -> Result<Value, ExtensionError> {
        serde_json::to_value(self)
            .map_err(|error| input_error(format!("research record encode failed: {error}")))
    }

    pub(crate) fn accepted(&self) -> Result<AcceptedRecord, ExtensionError> {
        let (proposals, accepted_order) = accepted_proposals(&self.ledger)?;

        let mut entities = BTreeMap::new();
        let mut outcomes = BTreeMap::new();
        let mut relations = BTreeMap::new();
        let mut assessments = BTreeMap::new();
        let mut outcome_order = Vec::new();
        let mut assessment_order = Vec::new();
        let mut semantic_ids = BTreeSet::new();
        for proposal_id in accepted_order {
            let semantic = proposals
                .get(&proposal_id)
                .expect("accepted proposal was checked above");
            if !semantic_ids.insert(semantic.id().to_owned()) {
                return Err(input_error("research-record semantic id is duplicated"));
            }
            match semantic {
                SemanticRecord::Entity(value) => {
                    entities.insert(value.id.clone(), value.clone());
                }
                SemanticRecord::InvestigationOutcome(value) => {
                    outcomes.insert(value.id.clone(), value.clone());
                    outcome_order.push(value.id.clone());
                }
                SemanticRecord::Relation(value) => {
                    relations.insert(value.id.clone(), value.clone());
                }
                SemanticRecord::Assessment(value) => {
                    assessments.insert(value.id.clone(), value.clone());
                    assessment_order.push(value.id.clone());
                }
            }
        }
        Ok(AcceptedRecord {
            entities,
            outcomes,
            relations,
            assessments,
            outcome_order,
            assessment_order,
        })
    }

    pub(crate) fn source_event_ids(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        for episode in &self.episodes {
            ids.extend(episode.source_event_ids.iter().cloned());
        }
        for entry in &self.ledger {
            match entry {
                LedgerEntry::Proposal { semantic, .. } => {
                    ids.extend(semantic.source_event_ids().iter().cloned());
                }
                LedgerEntry::Decision {
                    source_event_ids, ..
                } => ids.extend(source_event_ids.iter().cloned()),
            }
        }
        ids
    }

    pub(crate) fn artifact_source_event_ids(&self) -> BTreeSet<String> {
        let mut ids = self.source_event_ids();
        ids.extend(self.construction.proposal_source_event_ids.iter().cloned());
        if let Some(event_id) = &self.construction.predecessor_record_artifact_event_id {
            ids.insert(event_id.clone());
        }
        if let Some(event_id) = &self.construction.observer_result_event_id {
            ids.insert(event_id.clone());
        }
        ids
    }

    fn validate_shape(&self) -> Result<(), ExtensionError> {
        if self.schema != RESEARCH_RECORD_SCHEMA || self.media_type != RESEARCH_RECORD_MEDIA_TYPE {
            return Err(input_error(
                "research-record schema or media type is unsupported",
            ));
        }
        if !valid_token(&self.session.id)
            || !valid_event_id(&self.session.provenance_watermark_event_id)
            || !valid_event_id(&self.session.observed_through_event_id)
            || self.generated_at.is_empty()
        {
            return Err(input_error("research-record session header is invalid"));
        }
        validate_construction(&self.construction)?;
        let mut episode_ids = BTreeSet::new();
        let mut episode_sources = BTreeSet::new();
        for episode in &self.episodes {
            if !valid_id(&episode.id)
                || episode.event_kind.is_empty()
                || !valid_source_ids(&episode.source_event_ids)
                || !episode_ids.insert(episode.id.clone())
            {
                return Err(input_error("research-record episode is invalid"));
            }
            for source in &episode.source_event_ids {
                if !episode_sources.insert(source.clone()) {
                    return Err(input_error(
                        "research-record event belongs to more than one factual episode",
                    ));
                }
            }
        }
        if canonical_artifact_bytes(&self.value()?, "research record")?.len()
            > MAX_RESEARCH_RECORD_ARTIFACT_BYTES
        {
            return Err(input_error(
                "research-record exceeds the artifact size limit",
            ));
        }
        Ok(())
    }
}

pub(crate) fn canonical_artifact_bytes(
    value: &Value,
    label: &str,
) -> Result<Vec<u8>, ExtensionError> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| input_error(format!("{label} encode failed: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn accepted_proposals(
    ledger: &[LedgerEntry],
) -> Result<(BTreeMap<String, SemanticRecord>, Vec<String>), ExtensionError> {
    let mut proposals = BTreeMap::new();
    let mut ledger_ids = BTreeSet::new();
    let mut decided_proposal_ids = BTreeSet::new();
    let mut accepted_order = Vec::new();
    for entry in ledger {
        match entry {
            LedgerEntry::Proposal { id, semantic } => {
                if !valid_id(id) || !ledger_ids.insert(id.clone()) {
                    return Err(input_error("research-record ledger entry id is invalid"));
                }
                if proposals.insert(id.clone(), semantic.clone()).is_some() {
                    return Err(input_error("research-record proposal id is duplicated"));
                }
            }
            LedgerEntry::Decision {
                id,
                proposal_id,
                outcome,
                policy,
                source_event_ids,
            } => {
                if !valid_id(id) || !ledger_ids.insert(id.clone()) {
                    return Err(input_error("research-record ledger entry id is invalid"));
                }
                if !valid_id(proposal_id)
                    || policy != AUTO_ACCEPT_POLICY
                    || !valid_source_ids(source_event_ids)
                {
                    return Err(input_error("research-record decision is invalid"));
                }
                if !proposals.contains_key(proposal_id)
                    || !decided_proposal_ids.insert(proposal_id.clone())
                {
                    return Err(input_error(
                        "research-record decision does not name one undecided proposal",
                    ));
                }
                if *outcome == DecisionOutcome::Accepted {
                    accepted_order.push(proposal_id.clone());
                }
            }
        }
    }
    if proposals.len() != decided_proposal_ids.len() {
        return Err(input_error(
            "research-record proposal is missing its acceptance decision",
        ));
    }
    Ok((proposals, accepted_order))
}

impl AcceptedRecord {
    pub(crate) fn entity(&self, id: &str) -> Result<&ResearchEntity, ExtensionError> {
        self.entities
            .get(id)
            .ok_or_else(|| input_error(format!("research-record references unknown entity `{id}`")))
    }

    pub(crate) fn source_event_ids(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        for entity in self.entities.values() {
            ids.extend(entity.source_event_ids.iter().cloned());
        }
        for outcome in self.outcomes.values() {
            ids.extend(outcome.source_event_ids.iter().cloned());
        }
        for relation in self.relations.values() {
            ids.extend(relation.source_event_ids.iter().cloned());
        }
        for assessment in self.assessments.values() {
            ids.extend(assessment.source_event_ids.iter().cloned());
        }
        ids
    }

    pub(crate) fn active_assessments_for(&self, claim_id: &str) -> Vec<&ResearchAssessment> {
        let superseded = self
            .assessments
            .values()
            .filter(|assessment| assessment.claim_id == claim_id)
            .filter_map(|assessment| assessment.supersedes_assessment_id.as_deref())
            .collect::<BTreeSet<_>>();
        self.assessment_order
            .iter()
            .filter(|id| !superseded.contains(id.as_str()))
            .filter_map(|id| self.assessments.get(id))
            .filter(|assessment| assessment.claim_id == claim_id)
            .collect()
    }

    pub(crate) fn latest_outcome_for(&self, investigation_id: &str) -> Option<&ResearchOutcome> {
        self.outcome_order.iter().rev().find_map(|id| {
            self.outcomes
                .get(id)
                .filter(|outcome| outcome.investigation_id == investigation_id)
        })
    }

    pub(crate) fn viable_decomposition_parent(&self, entity_id: &str) -> bool {
        !matches!(
            self.latest_outcome_for(entity_id)
                .map(|outcome| outcome.outcome),
            Some(
                InvestigationOutcome::Blocked
                    | InvestigationOutcome::DeadEnd
                    | InvestigationOutcome::Abandoned
            )
        )
    }
}

pub(crate) struct AppendInput<'a> {
    pub(crate) prior: Option<&'a ResearchRecord>,
    pub(crate) predecessor_record_artifact_event_id: Option<&'a str>,
    pub(crate) events: &'a [EventEnvelope],
    pub(crate) batch: ObserverProposalBatch,
    pub(crate) watermark_event_id: String,
    pub(crate) generated_at: String,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) observer_result_event_id: Option<&'a str>,
}

pub(crate) fn append_observer_batch(
    input: AppendInput<'_>,
) -> Result<ResearchRecord, ExtensionError> {
    validate_batch_header(&input.batch)?;
    let mut record = record_for_append(&input)?;
    let accepted = record.accepted()?;
    let source_context = SourceContext::new(&record, &accepted, input.events);
    validate_batch(&input.batch, &accepted, &source_context)?;

    append_new_episodes(&mut record, input.events)?;
    append_batch_ledger(&mut record, input.batch)?;
    record.generated_at = input.generated_at;
    record.session.provenance_watermark_event_id = input.watermark_event_id.clone();
    record.session.observed_through_event_id = input.watermark_event_id;
    record.construction = RecordConstruction {
        operation: if input.prior.is_some() {
            RecordOperation::Reconcile
        } else {
            RecordOperation::Capture
        },
        predecessor_record_artifact_event_id: input
            .predecessor_record_artifact_event_id
            .map(str::to_owned),
        predecessor_record_watermark_event_id: input
            .prior
            .map(|prior| prior.session.provenance_watermark_event_id.clone()),
        proposal_source_event_ids: input
            .events
            .iter()
            .map(|event| event.id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        observer_result_event_id: input.observer_result_event_id.map(str::to_owned),
    };
    record.validate_shape()?;
    let accepted = record.accepted()?;
    validate_record_semantics(&accepted)?;
    Ok(record)
}

fn record_for_append(input: &AppendInput<'_>) -> Result<ResearchRecord, ExtensionError> {
    let first = input
        .events
        .first()
        .ok_or_else(|| input_error("research-record apply requires source events"))?;
    if input
        .events
        .iter()
        .any(|event| event.session != first.session)
    {
        return Err(input_error("research-record apply requires one session"));
    }
    if input
        .session_id
        .is_some_and(|session_id| session_id != first.session)
    {
        return Err(input_error(
            "session_id does not match the research-record event window",
        ));
    }
    if input.watermark_event_id != input.events.last().expect("non-empty").id {
        return Err(input_error(
            "research-record apply watermark does not end the source window",
        ));
    }
    match input.prior {
        Some(prior) => reconcile_record(input, prior, &first.session),
        None => capture_record(input, &first.session),
    }
}

fn reconcile_record(
    input: &AppendInput<'_>,
    prior: &ResearchRecord,
    session_id: &str,
) -> Result<ResearchRecord, ExtensionError> {
    if prior.session.id != session_id {
        return Err(input_error(
            "research-record state belongs to another session",
        ));
    }
    if !input
        .predecessor_record_artifact_event_id
        .is_some_and(valid_event_id)
    {
        return Err(input_error(
            "research-record reconcile requires its predecessor artifact event id",
        ));
    }
    Ok(prior.clone())
}

fn capture_record(
    input: &AppendInput<'_>,
    session_id: &str,
) -> Result<ResearchRecord, ExtensionError> {
    if input.predecessor_record_artifact_event_id.is_some() {
        return Err(input_error(
            "initial research-record capture must not name a predecessor artifact",
        ));
    }
    Ok(ResearchRecord::new(
        session_id.to_owned(),
        input.watermark_event_id.clone(),
        input.generated_at.clone(),
    ))
}

fn validate_batch_header(batch: &ObserverProposalBatch) -> Result<(), ExtensionError> {
    if batch.schema != RESEARCH_PROPOSALS_SCHEMA {
        return Err(input_error(format!(
            "research observer output must use `{RESEARCH_PROPOSALS_SCHEMA}`"
        )));
    }
    if batch.entities.is_empty()
        && batch.outcomes.is_empty()
        && batch.relations.is_empty()
        && batch.assessments.is_empty()
    {
        return Err(input_error(
            "research observer output contains no proposals",
        ));
    }
    Ok(())
}

struct SourceContext {
    new_ids: BTreeSet<String>,
    known_ids: BTreeSet<String>,
}

impl SourceContext {
    fn new(record: &ResearchRecord, accepted: &AcceptedRecord, events: &[EventEnvelope]) -> Self {
        let new_ids = events
            .iter()
            .map(|event| event.id.clone())
            .collect::<BTreeSet<_>>();
        let mut known_ids = record.source_event_ids();
        known_ids.extend(accepted.source_event_ids());
        known_ids.extend(new_ids.iter().cloned());
        Self { new_ids, known_ids }
    }

    fn validate(&self, source_event_ids: &[String], owner: &str) -> Result<(), ExtensionError> {
        if !valid_source_ids(source_event_ids)
            || source_event_ids
                .iter()
                .any(|source| !self.known_ids.contains(source))
            || !source_event_ids
                .iter()
                .any(|source| self.new_ids.contains(source))
        {
            return Err(input_error(format!(
                "research observer {owner} must cite known evidence and at least one newly observed event"
            )));
        }
        Ok(())
    }
}

fn validate_batch(
    batch: &ObserverProposalBatch,
    accepted: &AcceptedRecord,
    sources: &SourceContext,
) -> Result<(), ExtensionError> {
    let mut entities = accepted.entities.clone();
    let mut outcomes = accepted.outcomes.clone();
    let mut outcome_order = accepted.outcome_order.clone();
    let mut all_semantic_ids = semantic_ids(accepted);
    for entity in &batch.entities {
        validate_entity(entity, sources)?;
        if !all_semantic_ids.insert(entity.id.clone()) {
            return Err(input_error(format!(
                "research observer reuses semantic id `{}`",
                entity.id
            )));
        }
        entities.insert(entity.id.clone(), entity.clone());
    }
    let mut batch_outcome_targets = BTreeSet::new();
    for outcome in &batch.outcomes {
        if !all_semantic_ids.insert(outcome.id.clone()) {
            return Err(input_error(format!(
                "research observer reuses semantic id `{}`",
                outcome.id
            )));
        }
        if !batch_outcome_targets.insert(outcome.investigation_id.clone()) {
            return Err(input_error(
                "research observer may assert only one outcome per investigation in one batch",
            ));
        }
        validate_outcome(outcome, sources, &entities, &outcomes, &outcome_order)?;
        outcome_order.push(outcome.id.clone());
        outcomes.insert(outcome.id.clone(), outcome.clone());
    }
    for relation in &batch.relations {
        validate_relation_shape(relation, sources)?;
        if !all_semantic_ids.insert(relation.id.clone()) {
            return Err(input_error(format!(
                "research observer reuses semantic id `{}`",
                relation.id
            )));
        }
        validate_relation_endpoints(relation, &entities)?;
    }
    let mut assessments = accepted.assessments.clone();
    let mut assessment_order = accepted.assessment_order.clone();
    let mut proposed_assessments = batch.assessments.iter().collect::<Vec<_>>();
    proposed_assessments.sort_by(|left, right| left.id.cmp(&right.id));
    for assessment in proposed_assessments {
        if !all_semantic_ids.insert(assessment.id.clone()) {
            return Err(input_error(format!(
                "research observer reuses semantic id `{}`",
                assessment.id
            )));
        }
        validate_assessment(assessment, sources, &entities)?;
        validate_assessment_supersession(assessment, &assessments)?;
        assessments.insert(assessment.id.clone(), assessment.clone());
        assessment_order.push(assessment.id.clone());
    }

    let candidate = AcceptedRecord {
        entities,
        outcomes,
        relations: accepted
            .relations
            .iter()
            .map(|(id, relation)| (id.clone(), relation.clone()))
            .chain(
                batch
                    .relations
                    .iter()
                    .map(|relation| (relation.id.clone(), relation.clone())),
            )
            .collect(),
        assessments,
        outcome_order,
        assessment_order,
    };
    validate_record_semantics(&candidate)
}

fn semantic_ids(accepted: &AcceptedRecord) -> BTreeSet<String> {
    accepted
        .entities
        .keys()
        .chain(accepted.outcomes.keys())
        .chain(accepted.relations.keys())
        .chain(accepted.assessments.keys())
        .cloned()
        .collect()
}

fn validate_construction(construction: &RecordConstruction) -> Result<(), ExtensionError> {
    let predecessor_artifact = construction.predecessor_record_artifact_event_id.as_deref();
    let predecessor_watermark = construction
        .predecessor_record_watermark_event_id
        .as_deref();
    if !valid_source_ids(&construction.proposal_source_event_ids)
        || construction
            .observer_result_event_id
            .as_deref()
            .is_some_and(|id| !valid_event_id(id))
    {
        return Err(input_error("research-record construction is invalid"));
    }
    match construction.operation {
        RecordOperation::Capture
            if predecessor_artifact.is_none() && predecessor_watermark.is_none() =>
        {
            Ok(())
        }
        RecordOperation::Reconcile
            if predecessor_artifact.is_some_and(valid_event_id)
                && predecessor_watermark.is_some_and(valid_event_id) =>
        {
            Ok(())
        }
        _ => Err(input_error(
            "research-record construction lineage is invalid",
        )),
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_event_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.contains('\0')
}

fn valid_source_ids(values: &[String]) -> bool {
    !values.is_empty()
        && values.len() <= MAX_SOURCE_REFS
        && values.iter().all(|value| valid_event_id(value))
        && values.iter().collect::<BTreeSet<_>>().len() == values.len()
}

#[cfg(test)]
#[path = "research_record_tests.rs"]
mod tests;
