use super::Capability;
use serde_json::json;

#[test]
fn capability_registry_round_trips_every_known_capability() {
    let expected = [
        "fs-read",
        "fs-write",
        "provenance-read",
        "diagnostics-read",
        "artifact-write",
        "agent-record",
        "agent-spawn",
        "shell-exec",
        "network",
        "config-write",
        "secret-resolve",
        "context-slot",
    ];

    assert_eq!(Capability::ALL.len(), expected.len());
    for (&capability, expected_name) in Capability::ALL.iter().zip(expected) {
        assert_eq!(capability.as_str(), expected_name);
        assert_eq!(Capability::parse(expected_name), Some(capability));
    }
    assert_eq!(Capability::parse("context_slot"), None);
}

#[test]
fn capability_serde_spelling_matches_the_manifest_registry() {
    for &capability in Capability::ALL {
        let value = serde_json::to_value(capability).expect("serialize capability");
        assert_eq!(value, json!(capability.as_str()));
        assert_eq!(
            serde_json::from_value::<Capability>(value).expect("deserialize capability"),
            capability
        );
    }
}
