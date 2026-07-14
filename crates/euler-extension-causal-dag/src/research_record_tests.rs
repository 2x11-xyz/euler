use super::*;
use crate::research_projection::ResearchProjection;
use euler_event::{object, EventKind};

fn event(id: &str, kind: &str) -> EventEnvelope {
    EventEnvelope {
        v: 1,
        id: id.to_owned(),
        ts: "2026-07-14T00:00:00Z".to_owned(),
        session: "session-test".to_owned(),
        agent: "agent-test".to_owned(),
        parent: None,
        kind: EventKind::from(kind),
        payload: object([]),
        blobs: BTreeMap::new(),
    }
}

fn batch() -> ObserverProposalBatch {
    ObserverProposalBatch {
        schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
        entities: vec![
            ResearchEntity {
                id: "q-knuth".to_owned(),
                kind: EntityKind::Question,
                title: "Solve the Knuth task".to_owned(),
                summary: "The scoped user request.".to_owned(),
                lifecycle: None,
                source_event_ids: vec!["event-user".to_owned()],
            },
            ResearchEntity {
                id: "i-recurrence".to_owned(),
                kind: EntityKind::Investigation,
                title: "Test a recurrence".to_owned(),
                summary: "Try a closed form recurrence.".to_owned(),
                lifecycle: Some(EntityLifecycle::Active),
                source_event_ids: vec!["event-tool".to_owned()],
            },
        ],
        outcomes: vec![ResearchOutcome {
            id: "outcome-recurrence-dead".to_owned(),
            investigation_id: "i-recurrence".to_owned(),
            outcome: InvestigationOutcome::DeadEnd,
            summary: "The bounded recurrence check contradicted the proposal.".to_owned(),
            supersedes_outcome_id: None,
            source_event_ids: vec!["event-tool".to_owned()],
        }],
        relations: vec![ResearchRelation {
            id: "r-investigates".to_owned(),
            kind: RelationKind::Investigates,
            from: "i-recurrence".to_owned(),
            to: "q-knuth".to_owned(),
            summary: "The recurrence targets the requested problem.".to_owned(),
            source_event_ids: vec!["event-tool".to_owned()],
        }],
        assessments: Vec::new(),
    }
}

fn assessed_batch() -> ObserverProposalBatch {
    let mut value = batch();
    value.entities.push(ResearchEntity {
        id: "c-recurrence".to_owned(),
        kind: EntityKind::Claim,
        title: "The recurrence matches the checked prefix".to_owned(),
        summary: "A scoped claim about the proposed recurrence.".to_owned(),
        lifecycle: Some(EntityLifecycle::Active),
        source_event_ids: vec!["event-user".to_owned()],
    });
    value.relations.push(ResearchRelation {
        id: "r-investigates-claim".to_owned(),
        kind: RelationKind::Investigates,
        from: "i-recurrence".to_owned(),
        to: "c-recurrence".to_owned(),
        summary: "The attempt tests the scoped recurrence claim.".to_owned(),
        source_event_ids: vec!["event-tool".to_owned()],
    });
    value.assessments.push(ResearchAssessment {
        id: "assessment-supported".to_owned(),
        claim_id: "c-recurrence".to_owned(),
        scope: "the checked prefix".to_owned(),
        verdict: AssessmentVerdict::Supported,
        standard: "computation".to_owned(),
        summary: "The initial table agreed at the tested inputs.".to_owned(),
        supersedes_assessment_id: None,
        source_event_ids: vec!["event-tool".to_owned()],
    });
    value
}

#[test]
fn append_creates_a_complete_accepted_ledger() {
    let events = vec![
        event("event-user", EventKind::USER_MESSAGE),
        event("event-tool", EventKind::TOOL_RESULT),
    ];
    let record = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &events,
        batch: batch(),
        watermark_event_id: "event-tool".to_owned(),
        generated_at: events[1].ts.clone(),
        session_id: Some("session-test"),
        observer_result_event_id: None,
    })
    .expect("append record");

    assert_eq!(record.episodes.len(), 2);
    assert_eq!(record.ledger.len(), 8);
    assert_eq!(record.accepted().expect("accepted").entities.len(), 2);
    assert_eq!(record.source_event_ids().len(), 2);
    assert_eq!(record.construction.operation, RecordOperation::Capture);
    assert!(record
        .construction
        .predecessor_record_artifact_event_id
        .is_none());
    ResearchRecord::from_value(&record.value().expect("record value")).expect("round trip");
}

#[test]
fn malformed_ledger_cannot_reuse_a_decision_or_entry_identity() {
    let events = vec![
        event("event-user", EventKind::USER_MESSAGE),
        event("event-tool", EventKind::TOOL_RESULT),
    ];
    let record = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &events,
        batch: batch(),
        watermark_event_id: "event-tool".to_owned(),
        generated_at: events[1].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect("record");

    let mut duplicate_entry_id = record.value().expect("record value");
    duplicate_entry_id["ledger"][1]["id"] = duplicate_entry_id["ledger"][0]["id"].clone();
    let error = ResearchRecord::from_value(&duplicate_entry_id)
        .expect_err("ledger entry ids must remain unique");
    assert!(error.to_string().contains("ledger entry id"));

    let mut second_decision = record.value().expect("record value");
    second_decision["ledger"][3]["proposal_id"] =
        second_decision["ledger"][1]["proposal_id"].clone();
    let error = ResearchRecord::from_value(&second_decision)
        .expect_err("a proposal cannot be decided twice");
    assert!(error.to_string().contains("undecided proposal"));

    let mut missing_decision = record.value().expect("record value");
    missing_decision["ledger"]
        .as_array_mut()
        .expect("ledger")
        .pop();
    let error =
        ResearchRecord::from_value(&missing_decision).expect_err("every proposal needs a decision");
    assert!(error
        .to_string()
        .contains("missing its acceptance decision"));
}

#[test]
fn record_rejects_aggregate_artifact_growth_before_persistence() {
    let events = vec![
        event("event-user", EventKind::USER_MESSAGE),
        event("event-tool", EventKind::TOOL_RESULT),
    ];
    let record = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &events,
        batch: batch(),
        watermark_event_id: "event-tool".to_owned(),
        generated_at: events[1].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect("record");
    let mut oversized = record.value().expect("record value");
    oversized["episodes"][0]["event_kind"] =
        serde_json::Value::String("x".repeat(MAX_RESEARCH_RECORD_ARTIFACT_BYTES));
    let error = ResearchRecord::from_value(&oversized)
        .expect_err("aggregate record size must be bounded before artifact persistence");
    assert!(error
        .to_string()
        .contains("exceeds the artifact size limit"));
}

#[test]
fn reconcile_snapshot_carries_explicit_artifact_lineage() {
    let events = vec![
        event("event-user", EventKind::USER_MESSAGE),
        event("event-tool", EventKind::TOOL_RESULT),
    ];
    let prior = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &events,
        batch: batch(),
        watermark_event_id: "event-tool".to_owned(),
        generated_at: events[1].ts.clone(),
        session_id: None,
        observer_result_event_id: Some("observer-first"),
    })
    .expect("prior record");
    let next_events = vec![event("event-followup", EventKind::TOOL_RESULT)];
    let record = append_observer_batch(AppendInput {
        prior: Some(&prior),
        predecessor_record_artifact_event_id: Some("record-first"),
        events: &next_events,
        batch: ObserverProposalBatch {
            schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
            entities: Vec::new(),
            outcomes: vec![ResearchOutcome {
                id: "outcome-recurrence-abandoned".to_owned(),
                investigation_id: "i-recurrence".to_owned(),
                outcome: InvestigationOutcome::Abandoned,
                summary: "The agent stopped this already blocked line of work.".to_owned(),
                supersedes_outcome_id: Some("outcome-recurrence-dead".to_owned()),
                source_event_ids: vec!["event-followup".to_owned()],
            }],
            relations: Vec::new(),
            assessments: Vec::new(),
        },
        watermark_event_id: "event-followup".to_owned(),
        generated_at: next_events[0].ts.clone(),
        session_id: None,
        observer_result_event_id: Some("observer-second"),
    })
    .expect("reconcile record");

    assert_eq!(record.construction.operation, RecordOperation::Reconcile);
    assert_eq!(
        record
            .construction
            .predecessor_record_artifact_event_id
            .as_deref(),
        Some("record-first")
    );
    assert_eq!(
        record
            .construction
            .predecessor_record_watermark_event_id
            .as_deref(),
        Some("event-tool")
    );
    assert_eq!(
        record.construction.proposal_source_event_ids,
        vec!["event-followup"]
    );
    assert!(record.artifact_source_event_ids().contains("record-first"));
    assert!(record
        .artifact_source_event_ids()
        .contains("observer-second"));
    ResearchRecord::from_value(&record.value().expect("record value")).expect("round trip");
}

#[test]
fn reconciliation_can_advance_without_a_new_semantic_proposal() {
    let initial_events = vec![
        event("event-user", EventKind::USER_MESSAGE),
        event("event-tool", EventKind::TOOL_RESULT),
    ];
    let prior = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &initial_events,
        batch: batch(),
        watermark_event_id: "event-tool".to_owned(),
        generated_at: initial_events[1].ts.clone(),
        session_id: None,
        observer_result_event_id: Some("observer-first"),
    })
    .expect("prior record");
    let recap_events = vec![event("event-recap", EventKind::ASSISTANT_MESSAGE)];
    let record = append_observer_batch(AppendInput {
        prior: Some(&prior),
        predecessor_record_artifact_event_id: Some("record-first"),
        events: &recap_events,
        batch: ObserverProposalBatch {
            schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
            entities: Vec::new(),
            outcomes: Vec::new(),
            relations: Vec::new(),
            assessments: Vec::new(),
        },
        watermark_event_id: "event-recap".to_owned(),
        generated_at: recap_events[0].ts.clone(),
        session_id: None,
        observer_result_event_id: Some("observer-recap"),
    })
    .expect("recap reconciliation");

    assert_eq!(record.ledger, prior.ledger);
    assert_eq!(record.episodes.len(), prior.episodes.len() + 1);
    assert_eq!(record.session.observed_through_event_id, "event-recap");
    assert_eq!(record.construction.operation, RecordOperation::Reconcile);
    assert_eq!(
        record.construction.observer_result_event_id.as_deref(),
        Some("observer-recap")
    );
}

#[test]
fn initial_capture_rejects_an_empty_semantic_proposal() {
    let events = vec![event("event-user", EventKind::USER_MESSAGE)];
    let error = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &events,
        batch: ObserverProposalBatch {
            schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
            entities: Vec::new(),
            outcomes: Vec::new(),
            relations: Vec::new(),
            assessments: Vec::new(),
        },
        watermark_event_id: "event-user".to_owned(),
        generated_at: events[0].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect_err("initial capture needs a semantic root");

    assert!(error
        .to_string()
        .contains("initial research-record capture requires at least one proposal"));
}

#[test]
fn decomposition_is_whole_to_component_and_stops_at_a_dead_end() {
    let initial_events = vec![event("event-structure", EventKind::TOOL_RESULT)];
    let initial = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &initial_events,
        batch: ObserverProposalBatch {
            schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
            entities: vec![
                ResearchEntity {
                    id: "q".to_owned(),
                    kind: EntityKind::Question,
                    title: "Question".to_owned(),
                    summary: "Question".to_owned(),
                    lifecycle: None,
                    source_event_ids: vec!["event-structure".to_owned()],
                },
                ResearchEntity {
                    id: "i-whole".to_owned(),
                    kind: EntityKind::Investigation,
                    title: "Whole investigation".to_owned(),
                    summary: "The broad line of work.".to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-structure".to_owned()],
                },
                ResearchEntity {
                    id: "i-component".to_owned(),
                    kind: EntityKind::Investigation,
                    title: "Component investigation".to_owned(),
                    summary: "A proper component of the broad line.".to_owned(),
                    lifecycle: Some(EntityLifecycle::Active),
                    source_event_ids: vec!["event-structure".to_owned()],
                },
            ],
            outcomes: Vec::new(),
            relations: vec![
                ResearchRelation {
                    id: "r-whole-question".to_owned(),
                    kind: RelationKind::Investigates,
                    from: "i-whole".to_owned(),
                    to: "q".to_owned(),
                    summary: "The broad line addresses the question.".to_owned(),
                    source_event_ids: vec!["event-structure".to_owned()],
                },
                ResearchRelation {
                    id: "r-component-question".to_owned(),
                    kind: RelationKind::Investigates,
                    from: "i-component".to_owned(),
                    to: "q".to_owned(),
                    summary: "The component remains directed at the question.".to_owned(),
                    source_event_ids: vec!["event-structure".to_owned()],
                },
                ResearchRelation {
                    id: "r-whole-component".to_owned(),
                    kind: RelationKind::Decomposes,
                    from: "i-whole".to_owned(),
                    to: "i-component".to_owned(),
                    summary: "The broad investigation decomposes into this component.".to_owned(),
                    source_event_ids: vec!["event-structure".to_owned()],
                },
            ],
            assessments: Vec::new(),
        },
        watermark_event_id: "event-structure".to_owned(),
        generated_at: initial_events[0].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect("initial decomposition record");
    let initial_graph = ResearchProjection::from_record(&initial, "record-initial")
        .expect("initial projection")
        .artifact_value();
    assert!(initial_graph["forest"]["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .any(|edge| {
            edge["from"] == "node-i-whole"
                && edge["to"] == "node-i-component"
                && edge["kind"] == "decomposition"
                && edge["canonical_backbone"] == true
        }));

    let dead_end_events = vec![event("event-dead", EventKind::TOOL_RESULT)];
    let dead_end = append_observer_batch(AppendInput {
        prior: Some(&initial),
        predecessor_record_artifact_event_id: Some("record-initial"),
        events: &dead_end_events,
        batch: ObserverProposalBatch {
            schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
            entities: Vec::new(),
            outcomes: vec![ResearchOutcome {
                id: "outcome-whole-dead".to_owned(),
                investigation_id: "i-whole".to_owned(),
                outcome: InvestigationOutcome::DeadEnd,
                summary: "The broad line reached a documented dead end.".to_owned(),
                supersedes_outcome_id: None,
                source_event_ids: vec!["event-dead".to_owned()],
            }],
            relations: Vec::new(),
            assessments: Vec::new(),
        },
        watermark_event_id: "event-dead".to_owned(),
        generated_at: dead_end_events[0].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect("dead-end record");
    let dead_end_graph = ResearchProjection::from_record(&dead_end, "record-dead-end")
        .expect("dead-end projection")
        .artifact_value();
    let edges = dead_end_graph["forest"]["edges"].as_array().expect("edges");
    assert!(edges.iter().any(|edge| {
        edge["from"] == "node-q"
            && edge["to"] == "node-i-component"
            && edge["kind"] == "fork"
            && edge["canonical_backbone"] == true
    }));
    assert!(!edges.iter().any(|edge| {
        edge["id"] == "edge-r-whole-component" && edge["canonical_backbone"] == true
    }));
}

#[test]
fn assessments_remain_scoped_and_conflicts_do_not_silently_overwrite() {
    let events = vec![
        event("event-user", EventKind::USER_MESSAGE),
        event("event-tool", EventKind::TOOL_RESULT),
    ];
    let prior = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &events,
        batch: assessed_batch(),
        watermark_event_id: "event-tool".to_owned(),
        generated_at: events[1].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect("initial assessed record");
    let conflict_events = vec![event("event-counterexample", EventKind::TOOL_RESULT)];
    let record = append_observer_batch(AppendInput {
        prior: Some(&prior),
        predecessor_record_artifact_event_id: Some("record-first"),
        events: &conflict_events,
        batch: ObserverProposalBatch {
            schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
            entities: Vec::new(),
            outcomes: Vec::new(),
            relations: Vec::new(),
            assessments: vec![ResearchAssessment {
                id: "assessment-refuted".to_owned(),
                claim_id: "c-recurrence".to_owned(),
                scope: "the checked prefix".to_owned(),
                verdict: AssessmentVerdict::Refuted,
                standard: "counterexample".to_owned(),
                summary: "A newly generated row contradicts the recurrence.".to_owned(),
                supersedes_assessment_id: None,
                source_event_ids: vec!["event-counterexample".to_owned()],
            }],
        },
        watermark_event_id: "event-counterexample".to_owned(),
        generated_at: conflict_events[0].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect("conflicting assessment is retained");

    let accepted = record.accepted().expect("accepted record");
    assert_eq!(accepted.active_assessments_for("c-recurrence").len(), 2);
    let graph = ResearchProjection::from_record(&record, "record-second")
        .expect("projection")
        .artifact_value();
    let claim = graph["forest"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["id"] == "node-c-recurrence")
        .expect("claim node");
    assert_eq!(claim["status"], "inconclusive");
    assert_eq!(
        claim["metadata"]["assessment_presentation"]["contested"],
        true
    );
    assert_eq!(
        claim["metadata"]["active_assessments"]
            .as_array()
            .expect("assessments")
            .len(),
        2
    );

    let invalid_events = vec![event("event-rescope", EventKind::TOOL_RESULT)];
    let error = append_observer_batch(AppendInput {
        prior: Some(&record),
        predecessor_record_artifact_event_id: Some("record-second"),
        events: &invalid_events,
        batch: ObserverProposalBatch {
            schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
            entities: Vec::new(),
            outcomes: Vec::new(),
            relations: Vec::new(),
            assessments: vec![ResearchAssessment {
                id: "assessment-invalid-revision".to_owned(),
                claim_id: "c-recurrence".to_owned(),
                scope: "a broader untested domain".to_owned(),
                verdict: AssessmentVerdict::Inconclusive,
                standard: "review".to_owned(),
                summary: "This cannot revise an assessment of another scope.".to_owned(),
                supersedes_assessment_id: Some("assessment-supported".to_owned()),
                source_event_ids: vec!["event-rescope".to_owned()],
            }],
        },
        watermark_event_id: "event-rescope".to_owned(),
        generated_at: invalid_events[0].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect_err("assessment revisions must preserve scope");
    assert!(error
        .to_string()
        .contains("supersession must preserve claim and exact scope"));
}

#[test]
fn repair_requires_shared_predecessor_and_successor_evidence() {
    let events = vec![
        event("event-user", EventKind::USER_MESSAGE),
        event("event-tool", EventKind::TOOL_RESULT),
    ];
    let prior = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &events,
        batch: batch(),
        watermark_event_id: "event-tool".to_owned(),
        generated_at: events[1].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect("prior record");
    let next_events = vec![event("event-repair", EventKind::TOOL_RESULT)];
    let invalid = ObserverProposalBatch {
        schema: RESEARCH_PROPOSALS_SCHEMA.to_owned(),
        entities: vec![ResearchEntity {
            id: "i-repair".to_owned(),
            kind: EntityKind::Investigation,
            title: "Repair recurrence".to_owned(),
            summary: "Reuse the contradiction.".to_owned(),
            lifecycle: Some(EntityLifecycle::Active),
            source_event_ids: vec!["event-repair".to_owned()],
        }],
        outcomes: Vec::new(),
        relations: vec![
            ResearchRelation {
                id: "r-repair-investigates".to_owned(),
                kind: RelationKind::Investigates,
                from: "i-repair".to_owned(),
                to: "q-knuth".to_owned(),
                summary: "The repair stays on the question.".to_owned(),
                source_event_ids: vec!["event-repair".to_owned()],
            },
            ResearchRelation {
                id: "r-repairs".to_owned(),
                kind: RelationKind::Repairs,
                from: "i-repair".to_owned(),
                to: "i-recurrence".to_owned(),
                summary: "Claims to reuse the failure.".to_owned(),
                source_event_ids: vec!["event-repair".to_owned()],
            },
        ],
        assessments: Vec::new(),
    };
    let error = append_observer_batch(AppendInput {
        prior: Some(&prior),
        predecessor_record_artifact_event_id: Some("record-prior"),
        events: &next_events,
        batch: invalid,
        watermark_event_id: "event-repair".to_owned(),
        generated_at: next_events[0].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect_err("repair without predecessor evidence must fail");
    assert!(error
        .to_string()
        .contains("lineage relation must cite evidence from both"));
}

#[test]
fn structural_parent_relations_cannot_cycle() {
    let events = vec![
        event("event-user", EventKind::USER_MESSAGE),
        event("event-tool", EventKind::TOOL_RESULT),
    ];
    let mut cyclic = batch();
    cyclic.entities.push(ResearchEntity {
        id: "i-repair".to_owned(),
        kind: EntityKind::Investigation,
        title: "Repair the recurrence".to_owned(),
        summary: "A successor line that reuses the counterexample.".to_owned(),
        lifecycle: Some(EntityLifecycle::Active),
        source_event_ids: vec!["event-tool".to_owned()],
    });
    cyclic.outcomes.push(ResearchOutcome {
        id: "outcome-repair-dead".to_owned(),
        investigation_id: "i-repair".to_owned(),
        outcome: InvestigationOutcome::DeadEnd,
        summary: "The repair also reached a documented dead end.".to_owned(),
        supersedes_outcome_id: None,
        source_event_ids: vec!["event-tool".to_owned()],
    });
    cyclic.relations.extend([
        ResearchRelation {
            id: "r-repair-question".to_owned(),
            kind: RelationKind::Investigates,
            from: "i-repair".to_owned(),
            to: "q-knuth".to_owned(),
            summary: "The repair still addresses the question.".to_owned(),
            source_event_ids: vec!["event-tool".to_owned()],
        },
        ResearchRelation {
            id: "r-repair-to-recurrence".to_owned(),
            kind: RelationKind::Repairs,
            from: "i-repair".to_owned(),
            to: "i-recurrence".to_owned(),
            summary: "The repair cites the failed recurrence.".to_owned(),
            source_event_ids: vec!["event-tool".to_owned()],
        },
        ResearchRelation {
            id: "r-recurrence-to-repair".to_owned(),
            kind: RelationKind::Repairs,
            from: "i-recurrence".to_owned(),
            to: "i-repair".to_owned(),
            summary: "The original line cannot repair its successor.".to_owned(),
            source_event_ids: vec!["event-tool".to_owned()],
        },
    ]);
    let error = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &events,
        batch: cyclic,
        watermark_event_id: "event-tool".to_owned(),
        generated_at: events[1].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect_err("a durable causal backbone must be acyclic");
    assert!(error
        .to_string()
        .contains("structural backbone contains a cycle"));
}

#[test]
fn synthesis_requires_two_inputs_and_an_addressed_question() {
    let events = vec![
        event("event-user", EventKind::USER_MESSAGE),
        event("event-tool", EventKind::TOOL_RESULT),
    ];
    let mut one_input = batch();
    one_input.entities.extend([
        ResearchEntity {
            id: "o-table".to_owned(),
            kind: EntityKind::Observation,
            title: "Counterexample table".to_owned(),
            summary: "A table records the failed recurrence.".to_owned(),
            lifecycle: Some(EntityLifecycle::Active),
            source_event_ids: vec!["event-tool".to_owned()],
        },
        ResearchEntity {
            id: "s-summary".to_owned(),
            kind: EntityKind::Synthesis,
            title: "Integrated conclusion".to_owned(),
            summary: "The table and failed attempt jointly determine the next step.".to_owned(),
            lifecycle: Some(EntityLifecycle::Active),
            source_event_ids: vec!["event-tool".to_owned()],
        },
    ]);
    one_input.relations.extend([
        ResearchRelation {
            id: "r-produces-table".to_owned(),
            kind: RelationKind::Produces,
            from: "i-recurrence".to_owned(),
            to: "o-table".to_owned(),
            summary: "The recurrence check produced the table.".to_owned(),
            source_event_ids: vec!["event-tool".to_owned()],
        },
        ResearchRelation {
            id: "r-summary-addresses".to_owned(),
            kind: RelationKind::Addresses,
            from: "s-summary".to_owned(),
            to: "q-knuth".to_owned(),
            summary: "The conclusion addresses the requested task.".to_owned(),
            source_event_ids: vec!["event-tool".to_owned()],
        },
        ResearchRelation {
            id: "r-summary-integrates-attempt".to_owned(),
            kind: RelationKind::Integrates,
            from: "s-summary".to_owned(),
            to: "i-recurrence".to_owned(),
            summary: "The conclusion names the failed attempt.".to_owned(),
            source_event_ids: vec!["event-tool".to_owned()],
        },
    ]);
    let error = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &events,
        batch: one_input,
        watermark_event_id: "event-tool".to_owned(),
        generated_at: events[1].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect_err("one-input synthesis must be rejected");
    assert!(error.to_string().contains("at least two distinct"));

    let mut no_question = batch();
    no_question.entities.extend([
        ResearchEntity {
            id: "o-table".to_owned(),
            kind: EntityKind::Observation,
            title: "Counterexample table".to_owned(),
            summary: "A table records the failed recurrence.".to_owned(),
            lifecycle: Some(EntityLifecycle::Active),
            source_event_ids: vec!["event-tool".to_owned()],
        },
        ResearchEntity {
            id: "s-summary".to_owned(),
            kind: EntityKind::Synthesis,
            title: "Integrated conclusion".to_owned(),
            summary: "The table and failed attempt jointly determine the next step.".to_owned(),
            lifecycle: Some(EntityLifecycle::Active),
            source_event_ids: vec!["event-tool".to_owned()],
        },
    ]);
    no_question.relations.extend([
        ResearchRelation {
            id: "r-produces-table".to_owned(),
            kind: RelationKind::Produces,
            from: "i-recurrence".to_owned(),
            to: "o-table".to_owned(),
            summary: "The recurrence check produced the table.".to_owned(),
            source_event_ids: vec!["event-tool".to_owned()],
        },
        ResearchRelation {
            id: "r-summary-integrates-attempt".to_owned(),
            kind: RelationKind::Integrates,
            from: "s-summary".to_owned(),
            to: "i-recurrence".to_owned(),
            summary: "The conclusion names the failed attempt.".to_owned(),
            source_event_ids: vec!["event-tool".to_owned()],
        },
        ResearchRelation {
            id: "r-summary-integrates-table".to_owned(),
            kind: RelationKind::Integrates,
            from: "s-summary".to_owned(),
            to: "o-table".to_owned(),
            summary: "The conclusion names the counterexample table.".to_owned(),
            source_event_ids: vec!["event-tool".to_owned()],
        },
    ]);
    let error = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &events,
        batch: no_question,
        watermark_event_id: "event-tool".to_owned(),
        generated_at: events[1].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect_err("synthesis without a question must be rejected");
    assert!(error.to_string().contains("addresses relation"));
}

#[test]
fn uppercase_ulid_events_receive_valid_stable_episode_ids() {
    let event_id = "01KXGD29NMR76HREFS7BM9KVY9";
    let events = vec![event(event_id, EventKind::TOOL_RESULT)];
    let mut proposals = batch();
    for entity in &mut proposals.entities {
        entity.source_event_ids = vec![event_id.to_owned()];
    }
    for outcome in &mut proposals.outcomes {
        outcome.source_event_ids = vec![event_id.to_owned()];
    }
    for relation in &mut proposals.relations {
        relation.source_event_ids = vec![event_id.to_owned()];
    }
    let record = append_observer_batch(AppendInput {
        prior: None,
        predecessor_record_artifact_event_id: None,
        events: &events,
        batch: proposals,
        watermark_event_id: event_id.to_owned(),
        generated_at: events[0].ts.clone(),
        session_id: None,
        observer_result_event_id: None,
    })
    .expect("uppercase event id must be accepted");
    let episode = record.episodes.first().expect("episode");
    assert!(episode.id.starts_with("episode-"));
    assert!(episode.id.len() <= 96);
    assert!(episode
        .id
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'));
    ResearchRecord::from_value(&record.value().expect("record value")).expect("round trip");
}
