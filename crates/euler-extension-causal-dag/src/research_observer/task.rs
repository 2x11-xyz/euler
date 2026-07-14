use super::{compact, input_error};
use crate::research_record::{
    AcceptedRecord, EntityKind, ResearchRecord, RESEARCH_PROPOSALS_SCHEMA,
};
use euler_agents::MAX_TASK_BYTES;
use euler_event::EventEnvelope;
use euler_sdk::ExtensionError;

const EVENT_EXTRACT_CHARS: usize = 240;
const MAX_KNOWN_ENTITIES: usize = 96;
const MAX_KNOWN_RELATIONS: usize = 128;
const MAX_KNOWN_ASSESSMENTS: usize = 64;
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
        "Observe only this pilot evidence. Do not solve, call tools, infer hidden reasoning, or add prose. Return exactly one proposal JSON object; every array is required.".to_owned(),
        "Use only NEW EVENT ids or ids in ACCEPTED RECORD. Every new semantic record cites at least one NEW EVENT.".to_owned(),
        "Each investigation has investigates(investigation,question); an attempt claim also has investigates(investigation,claim) or produces(investigation,claim). repairs/continues_from/pivots_from are successor→predecessor; decomposes is whole→component; produces is investigation→output.".to_owned(),
        "repairs/pivots_from require a predecessor whose latest accepted outcome is blocked or dead_end. continues_from requires an active/completed productive predecessor. Cite both lines; never change an outcome to force lineage—continue or omit it.".to_owned(),
        "Outcomes append, never edit: first supersedes_outcome_id=null; a revision names the current outcome. Claims use scoped assessments: proven only formal_proof; refuted only counterexample or formal_proof; a revision preserves exact claim and scope.".to_owned(),
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
    trim_to_budget(summary_candidates(&accepted), budget)
}

fn summary_candidates(accepted: &AcceptedRecord) -> Vec<String> {
    let mut lines = entity_summary_lines(accepted, true);
    lines.extend(outcome_summary_lines(accepted));
    lines.extend(relation_summary_lines(accepted));
    lines.extend(entity_summary_lines(accepted, false));
    lines.extend(assessment_summary_lines(accepted));
    lines
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
