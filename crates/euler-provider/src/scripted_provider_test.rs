use super::*;

#[test]
fn scripted_stream_yields_events_in_order_and_exhausts() {
    let provider = ScriptedProvider::new(vec![FixtureResponse::Stream(vec![
        ScriptedStreamStep::Event(ModelStreamEvent::TextDelta("one".to_owned())),
        ScriptedStreamStep::SleepMs(1),
        ScriptedStreamStep::Event(ModelStreamEvent::TextDelta("two".to_owned())),
        ScriptedStreamStep::Event(ModelStreamEvent::Finished {
            stop_reason: StopReason::Completed,
            usage: None,
        }),
    ])]);
    let events = provider
        .invoke(empty_request())
        .expect("stream")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("events");

    assert_eq!(
        events,
        vec![
            ModelStreamEvent::TextDelta("one".to_owned()),
            ModelStreamEvent::TextDelta("two".to_owned()),
            ModelStreamEvent::Finished {
                stop_reason: StopReason::Completed,
                usage: None,
            },
        ]
    );
    let error = match provider.invoke(empty_request()) {
        Ok(_) => panic!("expected exhausted provider"),
        Err(error) => error,
    };
    assert_eq!(error.category(), ProviderErrorCategory::Transport);
}

fn empty_request() -> ModelRequest {
    ModelRequest {
        model: "echo".to_owned(),
        instructions: String::new(),
        input: Vec::new(),
        tools: Vec::new(),
        reasoning_effort: crate::ReasoningEffort::Medium,
        max_output_tokens: None,
    }
}
