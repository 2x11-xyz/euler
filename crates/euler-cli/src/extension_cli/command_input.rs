//! Host-side input for `user` extension commands. Every route (`euler
//! extension run`, the headless `extension_run` control line, and the TUI)
//! resolves its input here against the live command descriptor, so declared
//! arguments are validated once, by the host, before the extension runs.

use anyhow::{anyhow, Result};
use euler_sdk::{
    parse_model_targets_list, validate_model_targets, ArgValueKind, CommandDescriptor,
};

/// Input as the user supplied it. JSON and declared `--flag` arguments are
/// mutually exclusive forms.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UserCommandInput {
    Json(serde_json::Value),
    Flags(Vec<String>),
}

impl UserCommandInput {
    /// TUI slash input: `raw_args` text, when present, is whitespace-tokenized
    /// into flags (values with spaces need the JSON form); otherwise `input`
    /// is the JSON form.
    pub(crate) fn from_slash_args(input: serde_json::Value, raw_args: Option<String>) -> Self {
        match raw_args {
            Some(raw) => Self::Flags(raw.split_whitespace().map(str::to_owned).collect()),
            None => Self::Json(input),
        }
    }
}

/// Parse `Flags` against the declared args, then check the resulting (or
/// supplied JSON) object against the same args. A model-targets argument
/// reaches the extension as a JSON array of `provider::model` strings.
pub(crate) fn resolve_user_command_input(
    reference: &str,
    descriptor: &CommandDescriptor,
    input: UserCommandInput,
) -> Result<serde_json::Value> {
    let input = match input {
        UserCommandInput::Json(input) => input,
        UserCommandInput::Flags(flags) => input_from_flags(reference, descriptor, flags)?,
    };
    validate_declared_args(reference, descriptor, &input)?;
    Ok(input)
}

fn input_from_flags(
    reference: &str,
    descriptor: &CommandDescriptor,
    flags: Vec<String>,
) -> Result<serde_json::Value> {
    let mut input = serde_json::Map::new();
    let mut tokens = flags.into_iter();
    while let Some(token) = tokens.next() {
        let Some(flag) = token.strip_prefix("--") else {
            return Err(anyhow!("unexpected argument for {reference}: {token}"));
        };
        let Some(spec) = descriptor.args.iter().find(|spec| spec.flag == flag) else {
            return Err(anyhow!("--{flag} is not supported by {reference}"));
        };
        if input.contains_key(&spec.input_key) {
            return Err(anyhow!("--{flag} was provided more than once"));
        }
        let value = tokens
            .next()
            .ok_or_else(|| anyhow!("--{flag} requires a value"))?;
        let ArgValueKind::ModelTargets { max } = spec.value_kind else {
            return Err(anyhow!("--{flag} is not supported by {reference}"));
        };
        let targets =
            parse_model_targets_list(&value, max).map_err(|error| anyhow!("--{flag}: {error}"))?;
        input.insert(spec.input_key.clone(), serde_json::json!(targets));
    }
    Ok(serde_json::Value::Object(input))
}

fn validate_declared_args(
    reference: &str,
    descriptor: &CommandDescriptor,
    input: &serde_json::Value,
) -> Result<()> {
    for spec in &descriptor.args {
        let ArgValueKind::ModelTargets { max } = spec.value_kind else {
            continue;
        };
        let Some(value) = input.get(&spec.input_key) else {
            if spec.required {
                return Err(anyhow!(
                    "{reference} requires --{} <provider::model,...>",
                    spec.flag
                ));
            }
            continue;
        };
        let targets = value
            .as_array()
            .and_then(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| {
                anyhow!(
                    "{reference} input `{}` must be an array of provider::model strings",
                    spec.input_key
                )
            })?;
        validate_model_targets(&targets, max)
            .map_err(|error| anyhow!("{reference} input `{}`: {error}", spec.input_key))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_sdk::{ArgSpec, Invocation};
    use serde_json::json;

    fn descriptor(required: bool) -> CommandDescriptor {
        CommandDescriptor {
            name: "configure".to_owned(),
            display_name: "Configure".to_owned(),
            summary: "Pick target models.".to_owned(),
            required_capabilities: Vec::new(),
            args: vec![ArgSpec {
                flag: "models".to_owned(),
                input_key: "models".to_owned(),
                value_kind: ArgValueKind::ModelTargets { max: 2 },
                required,
                repeatable: false,
            }],
            accepts_session_id: false,
            invocation: Invocation::User,
            model_tool: None,
        }
    }

    fn flags(tokens: &[&str]) -> UserCommandInput {
        UserCommandInput::Flags(tokens.iter().map(|token| (*token).to_owned()).collect())
    }

    #[test]
    fn resolve_user_command_input_delivers_only_target_strings() {
        assert_eq!(
            resolve_user_command_input(
                "ext.configure",
                &descriptor(true),
                flags(&["--models", "openai::gpt-5.5,anthropic::claude-opus-4-1"]),
            )
            .expect("flags"),
            json!({"models": ["openai::gpt-5.5", "anthropic::claude-opus-4-1"]})
        );
        assert_eq!(
            resolve_user_command_input(
                "ext.configure",
                &descriptor(true),
                UserCommandInput::Json(json!({"models": ["openai::gpt-5.5"], "note": "kept"})),
            )
            .expect("json"),
            json!({"models": ["openai::gpt-5.5"], "note": "kept"})
        );
        // An optional argument may be omitted on either route.
        assert_eq!(
            resolve_user_command_input("ext.configure", &descriptor(false), flags(&[]))
                .expect("optional"),
            json!({})
        );
    }

    #[test]
    fn from_slash_args_routes_tui_text_through_the_resolver() {
        let resolve = |raw: Option<&str>| {
            resolve_user_command_input(
                "ext.configure",
                &descriptor(true),
                UserCommandInput::from_slash_args(
                    json!({"models": ["openai::gpt-5.5"]}),
                    raw.map(str::to_owned),
                ),
            )
        };
        assert_eq!(
            resolve(Some("  --models   a::b,c::d ")).expect("flags"),
            json!({"models": ["a::b", "c::d"]})
        );
        assert_eq!(
            resolve(Some("--models a::b,a::b"))
                .expect_err("duplicate")
                .to_string(),
            "--models: duplicate model target: a::b"
        );
        assert_eq!(
            resolve(None).expect("json"),
            json!({"models": ["openai::gpt-5.5"]})
        );
    }

    #[test]
    fn resolve_user_command_input_rejects_bad_flags_and_json() {
        let cases = [
            (
                flags(&[]),
                "ext.configure requires --models <provider::model,...>",
            ),
            (
                flags(&["--models", "a::b,c::d,e::f"]),
                "--models: expected 1 to 2 provider::model targets, got 3",
            ),
            (
                flags(&["--models", "gpt-5.5"]),
                "--models: model target must use provider::model form: gpt-5.5",
            ),
            (
                flags(&["--models", "a::b,a::b"]),
                "--models: duplicate model target: a::b",
            ),
            (flags(&["--models"]), "--models requires a value"),
            (
                flags(&["--models", "a::b", "--models", "c::d"]),
                "--models was provided more than once",
            ),
            (
                flags(&["--limit", "2"]),
                "--limit is not supported by ext.configure",
            ),
            (
                flags(&["models"]),
                "unexpected argument for ext.configure: models",
            ),
            (
                UserCommandInput::Json(json!({})),
                "ext.configure requires --models <provider::model,...>",
            ),
            (
                UserCommandInput::Json(json!({"models": "a::b"})),
                "ext.configure input `models` must be an array of provider::model strings",
            ),
            (
                UserCommandInput::Json(json!({"models": ["a::b", 1]})),
                "ext.configure input `models` must be an array of provider::model strings",
            ),
            (
                UserCommandInput::Json(json!({"models": ["a::b", "c::d", "e::f"]})),
                "ext.configure input `models`: expected 1 to 2 provider::model targets, got 3",
            ),
            (
                UserCommandInput::Json(json!({"models": ["openai"]})),
                "ext.configure input `models`: model target must use provider::model form: openai",
            ),
        ];
        for (input, expected) in cases {
            let error = resolve_user_command_input("ext.configure", &descriptor(true), input)
                .expect_err(expected);
            assert_eq!(error.to_string(), expected);
        }
    }
}
