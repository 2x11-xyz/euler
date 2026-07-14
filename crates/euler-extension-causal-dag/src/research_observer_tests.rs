use super::*;
use crate::research_record::{
    EntityKind, EntityLifecycle, InvestigationOutcome, RelationKind, ResearchEntity,
    ResearchOutcome, ResearchRelation,
};
use euler_event::{object, EventKind};
use std::collections::BTreeMap;

fn event(id: &str) -> EventEnvelope {
    EventEnvelope {
        v: 1,
        id: id.to_owned(),
        ts: "2026-07-14T00:00:00Z".to_owned(),
        session: "session".to_owned(),
        agent: "agent".to_owned(),
        parent: None,
        kind: EventKind::TOOL_RESULT.into(),
        payload: object([("output", "bounded evidence".into())]),
        blobs: BTreeMap::new(),
    }
}

#[test]
fn task_states_the_observer_authority_boundary() {
    let (task, count) = fit_task(None, &[event("event-1")]).expect("task");
    assert_eq!(count, 1);
    assert!(task.contains("Do not solve, call tools, infer hidden reasoning"));
    assert!(task.contains(RESEARCH_PROPOSALS_SCHEMA));
    assert!(task.contains("No alias fields"));
    assert!(task.contains("{id,kind,title,summary,lifecycle,source_event_ids}"));
    assert!(task.contains("standard=formal_proof|counterexample|derivation"));
}

#[test]
fn proposal_parser_accepts_plain_markdown_fences() {
    let output = format!(
        "```\n{}\n```",
        serde_json::json!({
            "schema": RESEARCH_PROPOSALS_SCHEMA,
            "entities": [],
            "outcomes": [],
            "relations": [],
            "assessments": []
        })
    );
    let parsed = parse_proposals(&output).expect("plain fence should be stripped");
    assert_eq!(parsed.schema, RESEARCH_PROPOSALS_SCHEMA);
}

#[test]
fn failed_companion_is_reported_before_missing_output() {
    let error = parse_apply_envelope(&serde_json::json!({
        "apply": {"mode": RESEARCH_MODE},
        "companion": {"ok": false, "error": "observer quota exhausted"}
    }))
    .expect_err("failed companion must not require an output payload");
    assert!(error
        .to_string()
        .contains("companion failed: observer quota exhausted"));
}

#[test]
fn task_preserves_repair_and_pivot_direction_rules() {
    let record = ResearchRecord {
        schema: crate::research_record::RESEARCH_RECORD_SCHEMA.to_owned(),
        media_type: crate::research_record::RESEARCH_RECORD_MEDIA_TYPE.to_owned(),
        generated_at: "2026-07-14T00:00:00Z".to_owned(),
        session: crate::research_record::RecordSession {
            id: "session".to_owned(),
            provenance_watermark_event_id: "event-0".to_owned(),
            observed_through_event_id: "event-0".to_owned(),
        },
        construction: crate::research_record::RecordConstruction {
            operation: crate::research_record::RecordOperation::Capture,
            predecessor_record_artifact_event_id: None,
            predecessor_record_watermark_event_id: None,
            proposal_source_event_ids: vec!["event-0".to_owned()],
            observer_result_event_id: None,
        },
        episodes: Vec::new(),
        ledger: vec![
            crate::research_record::LedgerEntry::Proposal {
                id: "proposal-q".to_owned(),
                semantic: crate::research_record::SemanticRecord::Entity(ResearchEntity {
                    id: "q".to_owned(),
                    kind: EntityKind::Question,
                    title: "Q".to_owned(),
                    summary: "Q".to_owned(),
                    lifecycle: None,
                    source_event_ids: vec!["event-0".to_owned()],
                }),
            },
            crate::research_record::LedgerEntry::Decision {
                id: "decision-q".to_owned(),
                proposal_id: "proposal-q".to_owned(),
                outcome: crate::research_record::DecisionOutcome::Accepted,
                policy: crate::research_record::AUTO_ACCEPT_POLICY.to_owned(),
                source_event_ids: vec!["event-0".to_owned()],
            },
            crate::research_record::LedgerEntry::Proposal {
                id: "proposal-a".to_owned(),
                semantic: crate::research_record::SemanticRecord::Entity(ResearchEntity {
                    id: "a".to_owned(),
                    kind: EntityKind::Investigation,
                    title: "A".to_owned(),
                    summary: "A".to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-0".to_owned()],
                }),
            },
            crate::research_record::LedgerEntry::Decision {
                id: "decision-a".to_owned(),
                proposal_id: "proposal-a".to_owned(),
                outcome: crate::research_record::DecisionOutcome::Accepted,
                policy: crate::research_record::AUTO_ACCEPT_POLICY.to_owned(),
                source_event_ids: vec!["event-0".to_owned()],
            },
            crate::research_record::LedgerEntry::Proposal {
                id: "proposal-r".to_owned(),
                semantic: crate::research_record::SemanticRecord::Relation(ResearchRelation {
                    id: "r".to_owned(),
                    kind: RelationKind::Investigates,
                    from: "a".to_owned(),
                    to: "q".to_owned(),
                    summary: "A investigates Q".to_owned(),
                    source_event_ids: vec!["event-0".to_owned()],
                }),
            },
            crate::research_record::LedgerEntry::Decision {
                id: "decision-r".to_owned(),
                proposal_id: "proposal-r".to_owned(),
                outcome: crate::research_record::DecisionOutcome::Accepted,
                policy: crate::research_record::AUTO_ACCEPT_POLICY.to_owned(),
                source_event_ids: vec!["event-0".to_owned()],
            },
            crate::research_record::LedgerEntry::Proposal {
                id: "proposal-outcome-old".to_owned(),
                semantic: crate::research_record::SemanticRecord::InvestigationOutcome(
                    ResearchOutcome {
                        id: "outcome-old".to_owned(),
                        investigation_id: "a".to_owned(),
                        outcome: InvestigationOutcome::Active,
                        summary: "The first active state.".to_owned(),
                        supersedes_outcome_id: None,
                        source_event_ids: vec!["event-0".to_owned()],
                    },
                ),
            },
            crate::research_record::LedgerEntry::Decision {
                id: "decision-outcome-old".to_owned(),
                proposal_id: "proposal-outcome-old".to_owned(),
                outcome: crate::research_record::DecisionOutcome::Accepted,
                policy: crate::research_record::AUTO_ACCEPT_POLICY.to_owned(),
                source_event_ids: vec!["event-0".to_owned()],
            },
            crate::research_record::LedgerEntry::Proposal {
                id: "proposal-outcome-current".to_owned(),
                semantic: crate::research_record::SemanticRecord::InvestigationOutcome(
                    ResearchOutcome {
                        id: "outcome-current".to_owned(),
                        investigation_id: "a".to_owned(),
                        outcome: InvestigationOutcome::Completed,
                        summary: "The current completed state.".to_owned(),
                        supersedes_outcome_id: Some("outcome-old".to_owned()),
                        source_event_ids: vec!["event-0".to_owned()],
                    },
                ),
            },
            crate::research_record::LedgerEntry::Decision {
                id: "decision-outcome-current".to_owned(),
                proposal_id: "proposal-outcome-current".to_owned(),
                outcome: crate::research_record::DecisionOutcome::Accepted,
                policy: crate::research_record::AUTO_ACCEPT_POLICY.to_owned(),
                source_event_ids: vec!["event-0".to_owned()],
            },
        ],
    };
    let lines = task_prefix(Some(&record));
    let task = lines.join("\n");
    assert!(task.contains("repairs/continues_from/pivots_from are successor→predecessor"));
    assert!(task.contains("decomposes is whole→component"));
    assert!(task.contains("repairs/pivots_from require a predecessor whose latest accepted outcome is blocked or dead_end"));
    assert!(task.contains("continues_from requires an active/completed productive predecessor"));
    assert!(task.contains("never change an outcome to force lineage"));
    assert!(task.contains("all arrays empty instead of duplicating recap material"));
    assert!(task.contains("RECENT ACCEPTED SEMANTIC IDS is a collision fence"));
    assert!(task.contains("RECENT ACCEPTED SEMANTIC IDS (do not re-emit):"));
    assert!(task.contains("CURRENT INVESTIGATION LEDGER:"));
    assert!(task.contains("current_outcome_id=outcome-current"));
    assert!(task.contains("lineage_anchor=event-0"));
}

#[test]
fn dense_accepted_record_keeps_room_for_new_evidence() {
    let mut record = ResearchRecord {
        schema: crate::research_record::RESEARCH_RECORD_SCHEMA.to_owned(),
        media_type: crate::research_record::RESEARCH_RECORD_MEDIA_TYPE.to_owned(),
        generated_at: "2026-07-14T00:00:00Z".to_owned(),
        session: crate::research_record::RecordSession {
            id: "session".to_owned(),
            provenance_watermark_event_id: "event-0".to_owned(),
            observed_through_event_id: "event-0".to_owned(),
        },
        construction: crate::research_record::RecordConstruction {
            operation: crate::research_record::RecordOperation::Capture,
            predecessor_record_artifact_event_id: None,
            predecessor_record_watermark_event_id: None,
            proposal_source_event_ids: vec!["event-0".to_owned()],
            observer_result_event_id: None,
        },
        episodes: Vec::new(),
        ledger: Vec::new(),
    };
    for index in 0..128 {
        let id = format!("artifact-{index:03}");
        record.ledger.extend([
            crate::research_record::LedgerEntry::Proposal {
                id: format!("proposal-{id}"),
                semantic: crate::research_record::SemanticRecord::Entity(ResearchEntity {
                    id: id.clone(),
                    kind: EntityKind::Artifact,
                    title: format!("Artifact {index}"),
                    summary: "A bounded durable-record fixture.".to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-0".to_owned()],
                }),
            },
            crate::research_record::LedgerEntry::Decision {
                id: format!("decision-{id}"),
                proposal_id: format!("proposal-{id}"),
                outcome: crate::research_record::DecisionOutcome::Accepted,
                policy: crate::research_record::AUTO_ACCEPT_POLICY.to_owned(),
                source_event_ids: vec!["event-0".to_owned()],
            },
        ]);
    }

    let (task, count) = fit_task(Some(&record), &[event("event-new")]).expect("task fits");
    assert_eq!(count, 1);
    assert!(task.len() <= euler_agents::MAX_TASK_BYTES);
    assert!(task.contains("EVENT id=event-new"));
    assert!(task.contains("artifact-127"));
}

#[test]
fn large_event_page_is_trimmed_to_a_fitting_prefix() {
    let events = (0..128)
        .map(|index| event(&format!("event-{index:03}")))
        .collect::<Vec<_>>();
    let (task, count) = fit_task(None, &events).expect("fitting prefix");
    assert!(count > 0);
    assert!(count < events.len());
    assert!(task.len() <= euler_agents::MAX_TASK_BYTES);
}
