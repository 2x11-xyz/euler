use super::{compact, input_error};
use crate::research_record::{
    AcceptedRecord, DecisionOutcome, EntityKind, LedgerEntry, ResearchRecord, SemanticRecord,
    RESEARCH_PROPOSALS_SCHEMA,
};
use euler_agents::MAX_TASK_BYTES;
use euler_event::EventEnvelope;
use euler_sdk::ExtensionError;
use std::collections::{BTreeMap, BTreeSet};

const EVENT_EXTRACT_CHARS: usize = 240;
const MAX_KNOWN_ENTITIES: usize = 96;
const MAX_KNOWN_RELATIONS: usize = 128;
const MAX_KNOWN_ASSESSMENTS: usize = 64;
const MAX_CURRENT_INVESTIGATIONS: usize = 32;
const MAX_RECENT_SEMANTIC_IDS: usize = 24;
const MAX_RECENT_SEMANTIC_ID_BYTES: usize = 1536;
// A fresh observer must always receive enough room to ground at least one new
// event. The accepted record is context, not a replacement for the evidence
// window it is meant to reconcile.
const MIN_EVENT_CONTEXT_BYTES: usize = 3 * 1024;

pub(super) fn fit_task(
    record: Option<&ResearchRecord>,
    events: &[EventEnvelope],
) -> Result<(String, usize), ExtensionError> {
    let prefix = task_prefix(record);
    let event_lines = events.iter().map(render_event_line).collect::<Vec<_>>();
    if render_task(&prefix, &event_lines, 1, 0).len() > MAX_TASK_BYTES {
        return Err(input_error(
            "research-record observer context cannot fit one source event; use a smaller record or add compaction",
        ));
    }
    let mut count = event_lines.len();
    while count > 0
        && render_task(&prefix, &event_lines, count, EVENT_EXTRACT_CHARS).len() > MAX_TASK_BYTES
    {
        count -= 1;
    }
    if count == 0 {
        return Err(input_error(
            "research-record observer task cannot fit one source event",
        ));
    }
    let mut extract = EVENT_EXTRACT_CHARS;
    while render_task(&prefix, &event_lines, count, extract).len() > MAX_TASK_BYTES && extract > 0 {
        extract /= 2;
    }
    Ok((render_task(&prefix, &event_lines, count, extract), count))
}

pub(super) fn task_prefix(record: Option<&ResearchRecord>) -> Vec<String> {
    let mut lines = vec![
        "Observe only this pilot evidence. Do not solve, call tools, infer hidden reasoning, or add prose. Return exactly one proposal JSON object; every array is required. When an accepted record exists, a window with no new durable semantic change may return all arrays empty instead of duplicating recap material.".to_owned(),
        "Use only NEW EVENT ids or ids in ACCEPTED RECORD. Every new semantic record cites at least one NEW EVENT.".to_owned(),
        "RECENT ACCEPTED SEMANTIC IDS is a collision fence: omit any listed id instead of re-emitting it from overlapping or recap evidence.".to_owned(),
        "Each investigation has investigates(investigation,question); an attempt claim also has investigates(investigation,claim) or produces(investigation,claim). repairs/continues_from/pivots_from are successor→predecessor; decomposes is whole→component; produces is investigation→output.".to_owned(),
        "repairs/pivots_from require a predecessor whose latest accepted outcome is blocked or dead_end. continues_from requires an active/completed productive predecessor. For any lineage relation, cite its predecessor's listed lineage_anchor and a successor source; never change an outcome to force lineage—continue or omit it.".to_owned(),
        "Outcomes append, never edit: first supersedes_outcome_id=null; a revision names the exact current_outcome_id from the CURRENT INVESTIGATION LEDGER. Claims use scoped assessments: proven only formal_proof; refuted only counterexample or formal_proof; a revision preserves exact claim and scope.".to_owned(),
        "Synthesis needs addresses(synthesis,question) plus two distinct integrates(synthesis,input); integrates never chooses its backbone parent.".to_owned(),
        "FIELDS: entities {id,kind,title,summary,lifecycle,source_event_ids}; outcomes {id,investigation_id,outcome,summary,supersedes_outcome_id,source_event_ids}; relations {id,kind,from,to,summary,source_event_ids}; assessments {id,claim_id,scope,verdict,standard,summary,supersedes_assessment_id,source_event_ids}. No alias fields.".to_owned(),
        "ENUMS: kind=question|claim|investigation|observation|artifact|synthesis; lifecycle=draft|active|withdrawn|archived|null; outcome=active|blocked|dead_end|completed|abandoned; relation=investigates|produces|evidence_for|evidence_against|repairs|continues_from|pivots_from|decomposes|addresses|integrates; verdict=supported|corroborated|proven|refuted|inconclusive; standard=formal_proof|counterexample|derivation|experiment|replication|measurement|benchmark|simulation|computation|argument|review.".to_owned(),
        "New ids use lowercase letters, digits, hyphens, or underscores.".to_owned(),
        format!("OUTPUT SCHEMA: {{\"schema\":\"{RESEARCH_PROPOSALS_SCHEMA}\",\"entities\":[],\"outcomes\":[],\"relations\":[],\"assessments\":[]}}"),
    ];
    if let Some(record) = record {
        lines.push("ACCEPTED RECORD (semantic context, not a prior graph):".to_owned());
        lines.extend(render_record_summary(record, record_summary_budget(&lines)));
    } else {
        lines.push("No accepted record exists yet. Establish a source-backed question before adding investigations.".to_owned());
    }
    lines.push("NEW EVENTS:".to_owned());
    lines
}

fn record_summary_budget(header: &[String]) -> usize {
    let header_bytes = header.iter().map(|line| line.len() + 1).sum::<usize>();
    let fixed_bytes = header_bytes
        .saturating_add("NEW EVENTS:".len() + 1)
        .saturating_add(MIN_EVENT_CONTEXT_BYTES);
    MAX_TASK_BYTES.saturating_sub(fixed_bytes)
}

fn render_record_summary(record: &ResearchRecord, budget: usize) -> Vec<String> {
    let Ok(accepted) = record.accepted() else {
        return vec!["accepted record could not be read".to_owned()];
    };
    let mut candidates = recent_semantic_id_lines(record, &accepted);
    candidates.extend(summary_candidates(record, &accepted));
    trim_to_budget(candidates, budget)
}

fn recent_semantic_id_lines(record: &ResearchRecord, accepted: &AcceptedRecord) -> Vec<String> {
    let accepted_proposals = accepted_proposal_ids(record);
    let accepted_ids = accepted
        .entities
        .keys()
        .chain(accepted.outcomes.keys())
        .chain(accepted.relations.keys())
        .chain(accepted.assessments.keys())
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut ids = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut bytes = 0usize;
    for entry in record.ledger.iter().rev() {
        let LedgerEntry::Proposal {
            id: proposal_id,
            semantic,
        } = entry
        else {
            continue;
        };
        let id = semantic_id(semantic);
        if !accepted_proposals.contains(proposal_id.as_str())
            || !accepted_ids.contains(id)
            || !seen_ids.insert(id)
        {
            continue;
        }
        let separator = if ids.is_empty() { 0 } else { 1 };
        let next = bytes.saturating_add(separator).saturating_add(id.len());
        if ids.len() == MAX_RECENT_SEMANTIC_IDS || next > MAX_RECENT_SEMANTIC_ID_BYTES {
            break;
        }
        bytes = next;
        ids.push(id);
    }
    if ids.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "RECENT ACCEPTED SEMANTIC IDS (do not re-emit): {}",
            ids.join(",")
        )]
    }
}

fn semantic_id(semantic: &SemanticRecord) -> &str {
    match semantic {
        SemanticRecord::Entity(value) => &value.id,
        SemanticRecord::InvestigationOutcome(value) => &value.id,
        SemanticRecord::Relation(value) => &value.id,
        SemanticRecord::Assessment(value) => &value.id,
    }
}

fn accepted_proposal_ids(record: &ResearchRecord) -> BTreeSet<&str> {
    record
        .ledger
        .iter()
        .filter_map(|entry| match entry {
            LedgerEntry::Decision {
                proposal_id,
                outcome,
                ..
            } if *outcome == DecisionOutcome::Accepted => Some(proposal_id.as_str()),
            _ => None,
        })
        .collect()
}

fn summary_candidates(record: &ResearchRecord, accepted: &AcceptedRecord) -> Vec<String> {
    let investigations = current_investigation_lines(record, accepted);
    let mut lines = Vec::new();
    if !investigations.is_empty() {
        lines.push("CURRENT INVESTIGATION LEDGER:".to_owned());
        lines.extend(investigations);
    }
    lines.extend(entity_summary_lines(accepted, true));
    lines.extend(outcome_summary_lines(accepted));
    lines.extend(relation_summary_lines(accepted));
    lines.extend(entity_summary_lines(accepted, false));
    lines.extend(assessment_summary_lines(accepted));
    lines
}

fn current_investigation_lines(record: &ResearchRecord, accepted: &AcceptedRecord) -> Vec<String> {
    let positions = latest_investigation_positions(record, accepted);
    let mut investigations = accepted
        .entities
        .values()
        .filter(|entity| entity.kind == EntityKind::Investigation)
        .collect::<Vec<_>>();
    investigations.sort_by(|left, right| {
        positions
            .get(&right.id)
            .cmp(&positions.get(&left.id))
            .then_with(|| left.id.cmp(&right.id))
    });
    investigations
        .into_iter()
        .take(MAX_CURRENT_INVESTIGATIONS)
        .map(|investigation| {
            let (current_outcome_id, current_outcome) = accepted
                .latest_outcome_for(&investigation.id)
                .map(|outcome| (outcome.id.as_str(), outcome.outcome.as_str()))
                .unwrap_or(("-", "none"));
            format!(
                "CURRENT investigation={} current_outcome_id={} current_outcome={} productive={} lineage_anchor={}",
                investigation.id,
                current_outcome_id,
                current_outcome,
                accepted.is_productive_investigation(&investigation.id),
                accepted
                    .lineage_anchor_for(&investigation.id)
                    .unwrap_or("-"),
            )
        })
        .collect()
}

fn latest_investigation_positions(
    record: &ResearchRecord,
    accepted: &AcceptedRecord,
) -> BTreeMap<String, usize> {
    let accepted_proposals = accepted_proposal_ids(record);
    let investigation_ids = accepted
        .entities
        .values()
        .filter(|entity| entity.kind == EntityKind::Investigation)
        .map(|entity| entity.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut positions = BTreeMap::new();
    for (position, entry) in record.ledger.iter().enumerate() {
        let LedgerEntry::Proposal {
            id: proposal_id,
            semantic,
        } = entry
        else {
            continue;
        };
        if !accepted_proposals.contains(proposal_id.as_str()) {
            continue;
        }
        for investigation_id in semantic_investigation_ids(semantic) {
            if investigation_ids.contains(investigation_id) {
                positions.insert(investigation_id.to_owned(), position);
            }
        }
    }
    positions
}

fn semantic_investigation_ids(semantic: &SemanticRecord) -> Vec<&str> {
    match semantic {
        SemanticRecord::Entity(entity) if entity.kind == EntityKind::Investigation => {
            vec![entity.id.as_str()]
        }
        SemanticRecord::InvestigationOutcome(outcome) => vec![outcome.investigation_id.as_str()],
        SemanticRecord::Relation(relation) => vec![relation.from.as_str(), relation.to.as_str()],
        SemanticRecord::Entity(_) | SemanticRecord::Assessment(_) => Vec::new(),
    }
}

fn entity_summary_lines(accepted: &AcceptedRecord, core: bool) -> Vec<String> {
    accepted
        .entities
        .values()
        .filter(|entity| is_core_entity(entity.kind) == core)
        .take(MAX_KNOWN_ENTITIES)
        .map(|entity| {
            format!(
                "ENTITY id={} kind={:?} lifecycle={:?} sources={} title={}",
                entity.id,
                entity.kind,
                entity.lifecycle,
                source_excerpt(&entity.source_event_ids),
                compact(&entity.title, 96)
            )
        })
        .collect()
}

fn is_core_entity(kind: EntityKind) -> bool {
    matches!(kind, EntityKind::Question | EntityKind::Investigation)
}

fn outcome_summary_lines(accepted: &AcceptedRecord) -> Vec<String> {
    accepted
        .outcomes
        .values()
        .take(MAX_KNOWN_ENTITIES)
        .map(|outcome| {
            format!(
                "OUTCOME id={} investigation={} outcome={:?} supersedes={:?} sources={} summary={}",
                outcome.id,
                outcome.investigation_id,
                outcome.outcome,
                outcome.supersedes_outcome_id,
                source_excerpt(&outcome.source_event_ids),
                compact(&outcome.summary, 80),
            )
        })
        .collect()
}

fn relation_summary_lines(accepted: &AcceptedRecord) -> Vec<String> {
    accepted
        .relations
        .values()
        .take(MAX_KNOWN_RELATIONS)
        .map(|relation| {
            format!(
                "RELATION id={} kind={:?} from={} to={} sources={}",
                relation.id,
                relation.kind,
                relation.from,
                relation.to,
                source_excerpt(&relation.source_event_ids),
            )
        })
        .collect()
}

fn assessment_summary_lines(accepted: &AcceptedRecord) -> Vec<String> {
    accepted
        .assessments
        .values()
        .take(MAX_KNOWN_ASSESSMENTS)
        .map(|assessment| {
            format!(
                "ASSESSMENT id={} claim={} verdict={:?} scope={} supersedes={:?} sources={}",
                assessment.id,
                assessment.claim_id,
                assessment.verdict,
                compact(&assessment.scope, 100),
                assessment.supersedes_assessment_id,
                source_excerpt(&assessment.source_event_ids),
            )
        })
        .collect()
}

fn trim_to_budget(candidates: Vec<String>, budget: usize) -> Vec<String> {
    let mut used = 0usize;
    candidates
        .into_iter()
        .take_while(|line| {
            let next = used.saturating_add(line.len()).saturating_add(1);
            if next > budget {
                false
            } else {
                used = next;
                true
            }
        })
        .collect()
}

fn source_excerpt(source_event_ids: &[String]) -> String {
    match source_event_ids {
        [] => "-".to_owned(),
        [only] => only.clone(),
        [first, last] => format!("{first},{last}"),
        [first, .., last] => format!("{first},{last} (+{} more)", source_event_ids.len() - 2),
    }
}

fn render_event_line(event: &EventEnvelope) -> String {
    format!(
        "EVENT id={} kind={} data={}",
        event.id,
        event.kind.as_str(),
        compact(&event_extract(event), EVENT_EXTRACT_CHARS),
    )
}

fn render_task(prefix: &[String], events: &[String], count: usize, extract: usize) -> String {
    let mut lines = prefix.to_vec();
    lines.extend(
        events
            .iter()
            .take(count)
            .map(|line| compact(line, line.len().min(128 + extract))),
    );
    lines.join("\n")
}

fn event_extract(event: &EventEnvelope) -> String {
    for key in ["content", "output", "summary", "message", "command", "path"] {
        if let Some(value) = event.payload.get(key) {
            if let Some(value) = value.as_str() {
                return value.to_owned();
            }
            if !value.is_null() {
                return value.to_string();
            }
        }
    }
    event
        .payload
        .iter()
        .next()
        .map(|(key, value)| format!("{key}={value}"))
        .unwrap_or_default()
}
