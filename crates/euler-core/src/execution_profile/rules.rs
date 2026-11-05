//! Permission rules, `ask_timeout`, and `extension_reviewers` (ADR 0027):
//! their parsing, validation, and the layers a profile records. A layer is
//! never concatenated into another, so most restrictive across layers stays
//! distinct from last match within one layer.

use crate::tools::CORE_TOOL_NAMES;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path};

/// At most this many rules per surface per layer (resolved question 11).
const MAX_RULES_PER_SURFACE: usize = 256;
/// At most this many bytes per pattern (resolved question 11) and per reason.
const MAX_PATTERN_BYTES: usize = 512;
const MAX_REASON_BYTES: usize = 512;
/// One day, the largest `ask_timeout` in seconds.
const MAX_ASK_TIMEOUT_SECONDS: u64 = 86_400;

const SURFACES: &str = "shell, path, outside_workspace, tools";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleState {
    Allow,
    Ask,
    Deny,
}

/// The direction a `path` or `outside_workspace` rule covers; absent means
/// `any`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAccess {
    Read,
    Write,
    Any,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRule {
    pub pattern: String,
    pub state: RuleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<RuleAccess>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One `rules` object: each surface is an ordered list in which the last
/// matching rule wins.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRules {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shell: Vec<PermissionRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<PermissionRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outside_workspace: Vec<PermissionRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<PermissionRule>,
}

/// Where a layer of rules came from: the user profile, the project overlay,
/// or a named child profile's own rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleLayerLabel {
    Profile,
    Overlay,
    Subagent,
}

/// One layer of the recorded `rules` list.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleLayer {
    pub layer: RuleLayerLabel,
    pub rules: ProfileRules,
}

impl ProfileRules {
    /// Parse and validate a `rules` value. An empty object or one whose
    /// surfaces are all empty is unset (`None`), so the digest and the
    /// summaries see one value for "no rules".
    pub(super) fn parse(value: &Value) -> Result<Option<Self>, String> {
        let Value::Object(surfaces) = value else {
            return Err(format!("rules: expected an object with {SURFACES}"));
        };
        let mut rules = Self::default();
        for (surface, list) in surfaces {
            let slot = match surface.as_str() {
                "shell" => &mut rules.shell,
                "path" => &mut rules.path,
                "outside_workspace" => &mut rules.outside_workspace,
                "tools" => &mut rules.tools,
                _ => {
                    return Err(format!(
                        "rules: unknown surface `{surface}`; expected one of {SURFACES}"
                    ))
                }
            };
            let Value::Array(list) = list else {
                return Err(format!("rules.{surface}: expected an array of rules"));
            };
            if list.len() > MAX_RULES_PER_SURFACE {
                return Err(format!(
                    "rules.{surface}: {} rules; at most {MAX_RULES_PER_SURFACE} per surface",
                    list.len()
                ));
            }
            for (index, rule) in list.iter().enumerate() {
                let rule = parse_rule(surface, rule)
                    .map_err(|message| format!("rules.{surface}[{index}]: {message}"))?;
                slot.push(rule);
            }
        }
        Ok((!rules.is_empty()).then_some(rules))
    }

    pub fn is_empty(&self) -> bool {
        self.surfaces().iter().all(|(_, rules)| rules.is_empty())
    }

    fn surfaces(&self) -> [(&'static str, &Vec<PermissionRule>); 4] {
        [
            ("shell", &self.shell),
            ("path", &self.path),
            ("outside_workspace", &self.outside_workspace),
            ("tools", &self.tools),
        ]
    }

    /// The first `allow` rule as `surface[index]` and its pattern.
    pub(super) fn first_allow(&self) -> Option<(String, &str)> {
        self.surfaces().into_iter().find_map(|(surface, rules)| {
            rules
                .iter()
                .position(|rule| rule.state == RuleState::Allow)
                .map(|index| (format!("{surface}[{index}]"), rules[index].pattern.as_str()))
        })
    }

    /// These rules with every `allow` removed: what a layer contributes where
    /// only `ask` and `deny` apply.
    fn without_allow(&self) -> Self {
        let keep = |rules: &Vec<PermissionRule>| {
            rules
                .iter()
                .filter(|rule| rule.state != RuleState::Allow)
                .cloned()
                .collect()
        };
        Self {
            shell: keep(&self.shell),
            path: keep(&self.path),
            outside_workspace: keep(&self.outside_workspace),
            tools: keep(&self.tools),
        }
    }
}

fn parse_rule(surface: &str, value: &Value) -> Result<PermissionRule, String> {
    let rule = PermissionRule::deserialize(value).map_err(|error| error.to_string())?;
    let directional = matches!(surface, "path" | "outside_workspace");
    if rule.access.is_some() && !directional {
        return Err("access applies only to path and outside_workspace rules".to_owned());
    }
    if rule
        .reason
        .as_ref()
        .is_some_and(|reason| reason.len() > MAX_REASON_BYTES)
    {
        return Err(format!("reason is longer than {MAX_REASON_BYTES} bytes"));
    }
    let pattern = rule.pattern.as_str();
    if pattern.trim().is_empty() {
        return Err("pattern is empty".to_owned());
    }
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(format!("pattern is longer than {MAX_PATTERN_BYTES} bytes"));
    }
    if pattern.chars().any(char::is_control) {
        return Err(format!(
            "pattern \"{}\" has a control character",
            pattern.escape_default()
        ));
    }
    match surface {
        "path" => check_path_pattern(pattern),
        "outside_workspace" => check_outside_workspace_pattern(pattern),
        "tools" => check_tools_pattern(pattern),
        _ => Ok(()),
    }?;
    Ok(rule)
}

/// A `path` glob: absolute, `~/`, or workspace-relative without `..`, with
/// `**` only as a whole component.
fn check_path_pattern(pattern: &str) -> Result<(), String> {
    let relative = pattern.strip_prefix("~/").unwrap_or(pattern);
    if relative.starts_with('~') {
        return Err(format!(
            "path pattern \"{pattern}\" must be workspace-relative, absolute, or start with ~/"
        ));
    }
    if Path::new(relative)
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(format!("path pattern \"{pattern}\" may not contain .."));
    }
    if relative
        .split('/')
        .any(|component| component.contains("**") && component != "**")
    {
        return Err(format!(
            "path pattern \"{pattern}\": ** must be a whole path component"
        ));
    }
    Ok(())
}

/// An `outside_workspace` directory prefix: absolute or `~/`, literal.
fn check_outside_workspace_pattern(pattern: &str) -> Result<(), String> {
    if !(Path::new(pattern).is_absolute() || pattern.starts_with("~/")) {
        return Err(format!(
            "outside_workspace pattern \"{pattern}\" must be an absolute path or start with ~/"
        ));
    }
    if pattern.contains('*') {
        return Err(format!(
            "outside_workspace pattern \"{pattern}\" is a directory prefix, not a glob"
        ));
    }
    if Path::new(pattern)
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(format!(
            "outside_workspace pattern \"{pattern}\" may not contain .."
        ));
    }
    Ok(())
}

/// A `tools` pattern: a core tool name, or `<extension-id>.<command>`, with
/// `*` matching any run of characters. A core pattern must match a core
/// tool, so a typo is an error rather than a rule that never applies.
fn check_tools_pattern(pattern: &str) -> Result<(), String> {
    if let Some((extension, command)) = pattern.split_once('.') {
        if extension.is_empty() || command.is_empty() {
            return Err(format!(
                "tools pattern \"{pattern}\" must name a core tool or <extension-id>.<command>"
            ));
        }
        return Ok(());
    }
    if CORE_TOOL_NAMES
        .iter()
        .any(|name| glob_matches(pattern, name))
    {
        return Ok(());
    }
    Err(format!(
        "tools pattern \"{pattern}\" matches no core tool ({}); name an extension command as \
         <extension-id>.<command>",
        CORE_TOOL_NAMES.join(", ")
    ))
}

/// Whether `pattern` matches all of `text`, with `*` matching any run of
/// characters.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

/// Parse `ask_timeout`: whole seconds from 0 to one day.
pub(super) fn parse_ask_timeout(value: &Value) -> Result<u32, String> {
    value
        .as_u64()
        .filter(|seconds| *seconds <= MAX_ASK_TIMEOUT_SECONDS)
        .map(|seconds| u32::try_from(seconds).expect("at most one day of seconds"))
        .ok_or_else(|| {
            format!("ask_timeout: expected a whole number of seconds from 0 to {MAX_ASK_TIMEOUT_SECONDS}")
        })
}

/// Parse `extension_reviewers`: distinct `<extension-id>.<command>` names.
/// An empty list is unset.
pub(super) fn parse_extension_reviewers(value: &Value) -> Result<Option<Vec<String>>, String> {
    let names: Vec<String> =
        Vec::deserialize(value).map_err(|error| format!("extension_reviewers: {error}"))?;
    for (index, name) in names.iter().enumerate() {
        let valid = name.split_once('.').is_some_and(|(extension, command)| {
            !extension.is_empty() && !command.is_empty() && !name.contains('*')
        });
        if !valid {
            return Err(format!(
                "extension_reviewers[{index}]: \"{name}\" must be <extension-id>.<command>"
            ));
        }
        if names[..index].contains(name) {
            return Err(format!(
                "extension_reviewers[{index}]: \"{name}\" is listed twice"
            ));
        }
    }
    Ok((!names.is_empty()).then_some(names))
}

/// Add the `ask` and `deny` rules of `layer` to `layers` as a further layer,
/// unless they are empty or a layer with the same label already holds
/// exactly those `ask` and `deny` rules. Adding only `ask` and `deny` means a
/// joined layer can only tighten.
pub(super) fn join_layer(layers: &mut Vec<RuleLayer>, layer: &RuleLayer) {
    let rules = layer.rules.without_allow();
    let present = layers
        .iter()
        .any(|existing| existing.layer == layer.layer && existing.rules.without_allow() == rules);
    if !rules.is_empty() && !present {
        layers.push(RuleLayer {
            layer: layer.layer,
            rules,
        });
    }
}

/// The lower of two `ask_timeout` values, an absent one meaning no limit.
pub(super) fn lower_ask_timeout(left: Option<u32>, right: Option<u32>) -> Option<u32> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (one, None) | (None, one) => one,
    }
}
