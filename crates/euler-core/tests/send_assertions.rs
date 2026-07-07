use euler_core::permissions::ScriptedDecider;
use euler_core::Session;
use euler_provider::{
    anthropic::AnthropicProvider, chatgpt::ChatGptProvider, openrouter::OpenRouterProvider,
    EchoProvider, ModelProvider, ProviderSet, ProviderStream, ScriptedProvider,
};

fn assert_send<T: Send>() {}

#[test]
fn session_is_send_for_send_decider() {
    assert_send::<Session<ScriptedDecider>>();
}

#[test]
fn provider_stream_and_provider_set_are_send() {
    assert_send::<Box<dyn ModelProvider>>();
    assert_send::<ProviderStream>();
    assert_send::<ProviderSet>();
}

#[test]
fn built_in_providers_are_send() {
    assert_send::<EchoProvider>();
    assert_send::<ScriptedProvider>();
    assert_send::<ChatGptProvider>();
    assert_send::<AnthropicProvider>();
    assert_send::<OpenRouterProvider>();
}
