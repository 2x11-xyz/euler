use super::{
    validate_plan_presentation, PlanItemStatus, PlanPresentation, PlanPresentationItem,
    PlanPresentationStatus, MAX_PLAN_PRESENTATION_EXPLANATION_BYTES, MAX_PLAN_PRESENTATION_ITEMS,
    MAX_PLAN_PRESENTATION_REVISION, MAX_PLAN_PRESENTATION_STEP_BYTES,
};

fn presentation() -> PlanPresentation {
    PlanPresentation {
        revision: 1,
        status: PlanPresentationStatus::Active,
        explanation: Some("Initial execution plan".to_owned()),
        items: vec![
            PlanPresentationItem {
                step: "Inspect the boundary".to_owned(),
                status: PlanItemStatus::Completed,
            },
            PlanPresentationItem {
                step: "Implement the substrate".to_owned(),
                status: PlanItemStatus::InProgress,
            },
        ],
    }
}

#[test]
fn plan_presentation_shape_round_trips_with_closed_fields() {
    let presentation = presentation();
    let value = serde_json::to_value(&presentation).expect("serialize");

    assert_eq!(value["revision"], serde_json::json!(1));
    assert_eq!(value["status"], serde_json::json!("active"));
    assert_eq!(
        value["items"][1]["status"],
        serde_json::json!("in_progress")
    );
    assert_eq!(
        serde_json::from_value::<PlanPresentation>(value).expect("deserialize"),
        presentation
    );
    assert!(
        serde_json::from_value::<PlanPresentation>(serde_json::json!({
            "revision": 1,
            "status": "active",
            "explanation": null,
            "items": [{"step": "Do work", "status": "pending"}],
            "unknown": true
        }))
        .is_err()
    );
}

#[test]
fn plan_presentation_status_registries_are_closed() {
    for status in [
        PlanPresentationStatus::Active,
        PlanPresentationStatus::Blocked,
        PlanPresentationStatus::Waiting,
        PlanPresentationStatus::Completed,
    ] {
        assert_eq!(PlanPresentationStatus::parse(status.as_str()), Some(status));
    }
    for status in [
        PlanItemStatus::Pending,
        PlanItemStatus::InProgress,
        PlanItemStatus::Completed,
    ] {
        assert_eq!(PlanItemStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(PlanPresentationStatus::parse("complete"), None);
    assert_eq!(PlanItemStatus::parse("running"), None);
}

#[test]
fn plan_presentation_validation_enforces_only_structural_bounds() {
    validate_plan_presentation(&presentation()).expect("valid presentation");

    let mut zero_revision = presentation();
    zero_revision.revision = 0;
    assert!(validate_plan_presentation(&zero_revision)
        .expect_err("zero revision")
        .to_string()
        .contains("revision"));

    let mut exhausted_revision = presentation();
    exhausted_revision.revision = MAX_PLAN_PRESENTATION_REVISION + 1;
    assert!(validate_plan_presentation(&exhausted_revision).is_err());

    let mut empty = presentation();
    empty.items.clear();
    assert!(validate_plan_presentation(&empty).is_err());

    let mut too_many = presentation();
    too_many.items = (0..=MAX_PLAN_PRESENTATION_ITEMS)
        .map(|index| PlanPresentationItem {
            step: format!("Step {index}"),
            status: PlanItemStatus::Pending,
        })
        .collect();
    assert!(validate_plan_presentation(&too_many).is_err());

    let mut long_step = presentation();
    long_step.items[0].step = "x".repeat(MAX_PLAN_PRESENTATION_STEP_BYTES + 1);
    assert!(validate_plan_presentation(&long_step).is_err());

    let mut long_explanation = presentation();
    long_explanation.explanation = Some("x".repeat(MAX_PLAN_PRESENTATION_EXPLANATION_BYTES + 1));
    assert!(validate_plan_presentation(&long_explanation).is_err());
}

#[test]
fn plan_presentation_validation_rejects_unrenderable_text_not_workflow_states() {
    for invalid in [
        " ",
        "line\nbreak",
        "tab\tbreak",
        "soft\u{00AD}hyphen",
        "arabic\u{0600}sign",
        "arabic\u{061C}mark",
        "ayah\u{06DD}end",
        "pound\u{0890}mark",
        "disputed\u{08E2}end",
        "mongolian\u{180E}separator",
        "zero\u{200B}width",
        "line\u{2028}separator",
        "paragraph\u{2029}separator",
        "bidi\u{202E}spoof",
        "word\u{2060}joiner",
        "annotation\u{FFF9}anchor",
        "kaithi\u{110BD}sign",
        "kaithi\u{110CD}above",
        "hieroglyph\u{13430}joiner",
        "shorthand\u{1BCA0}format",
        "musical\u{1D173}format",
        "language\u{E0001}tag",
        "tag\u{E0020}space",
    ] {
        let mut value = presentation();
        value.items[0].step = invalid.to_owned();
        assert!(
            validate_plan_presentation(&value).is_err(),
            "accepted {invalid:?}"
        );
    }

    let value = PlanPresentation {
        revision: 7,
        status: PlanPresentationStatus::Blocked,
        explanation: None,
        items: vec![
            PlanPresentationItem {
                step: "First".to_owned(),
                status: PlanItemStatus::InProgress,
            },
            PlanPresentationItem {
                step: "Second".to_owned(),
                status: PlanItemStatus::InProgress,
            },
        ],
    };
    validate_plan_presentation(&value).expect("workflow policy stays extension-owned");
}
