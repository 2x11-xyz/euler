use super::*;
use serde_json::json;

#[test]
fn event_script_rejects_invalid_shapes() {
    let cases = [
        (
            json!({"version": 2, "responses": [{"events": [{"finished": {"stop_reason": "completed"}}]}]}),
            "unsupported fixture event script version 2",
        ),
        (
            json!({"version": 1, "responses": []}),
            "fixture event script must contain at least one response",
        ),
        (
            json!({"version": 1, "responses": [{"events": [{"text_delta": "partial"}]}]}),
            "fixture response 0 must end with a finished event",
        ),
        (
            json!({"version": 1, "responses": [{"events": [{"text_delta": "a", "sleep_ms": 1}, {"finished": {"stop_reason": "completed"}}]}]}),
            "fixture response 0 event 0 must contain exactly one event kind",
        ),
        (
            json!({"version": 1, "responses": [{"events": [{"sleep_ms": 5001}, {"finished": {"stop_reason": "completed"}}]}]}),
            "fixture response 0 event 0 sleep_ms 5001 exceeds 5000",
        ),
        (
            json!({"version": 1, "responses": [{"events": [{"finished": {"stop_reason": "tool_use"}}]}]}),
            "fixture response 0 without tool calls cannot finish with tool_use",
        ),
        (
            json!({"version": 1, "responses": [{"events": [{"tool_call": {"id": "call-1", "name": "read_file", "input": {}}}, {"finished": {"stop_reason": "completed"}}]}]}),
            "fixture response 0 with tool calls must finish with tool_use",
        ),
        (
            json!({"version": 1, "extra": true, "responses": [{"events": [{"finished": {"stop_reason": "completed"}}]}]}),
            "unknown field `extra`",
        ),
        (
            json!({"version": 1, "responses": [{"events": [{"text_delta": "a", "unknown": true}, {"finished": {"stop_reason": "completed"}}]}]}),
            "unknown field `unknown`",
        ),
        (
            json!({"version": 1, "responses": [{"events": [{"finished": {"stop_reason": "completed"}}, {"finished": {"stop_reason": "completed"}}]}]}),
            "fixture response 0 must contain exactly one finished event",
        ),
        (
            json!({"version": 1, "responses": [{"events": [{"finished": {"stop_reason": "unknown"}}]}]}),
            "unknown fixture stop reason `unknown`",
        ),
    ];

    for (script, expected) in cases {
        let error =
            EventScript::from_slice(script.to_string().as_bytes()).expect_err("script should fail");

        assert!(
            error.to_string().contains(expected),
            "expected {expected:?} in {error}"
        );
    }
}

#[test]
fn event_script_path_reads_json_data_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let script = temp.path().join("script.json");
    std::fs::write(
        &script,
        r#"{"version":1,"responses":[{"events":[{"text_delta":"from file"},{"finished":{"stop_reason":"completed"}}]}]}"#,
    )
    .expect("write script");

    provider_from_event_script_path(&script).expect("provider");
}

#[test]
fn event_script_path_rejects_non_file_and_oversized_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let directory_error = provider_from_event_script_path(temp.path()).expect_err("directory");
    assert!(directory_error
        .to_string()
        .contains("fixture event script path is not a file"));

    let script = temp.path().join("too-large.json");
    std::fs::write(&script, vec![b' '; MAX_BYTES as usize + 1]).expect("write script");
    let size_error = provider_from_event_script_path(&script).expect_err("too large");
    assert!(size_error.to_string().contains("fixture event script"));
    assert!(size_error.to_string().contains("is too large"));
}
