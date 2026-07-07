use super::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub(super) fn parse_errors(value: Option<&Value>) -> Result<ErrorCounts, String> {
    let values = value
        .ok_or_else(|| "errors is required".to_owned())?
        .as_array()
        .ok_or_else(|| "errors must be an array".to_owned())?;
    let mut counts = ErrorCounts::default();
    for error in values {
        let object = error
            .as_object()
            .ok_or_else(|| "each error must be a JSON object".to_owned())?;
        reject_output_fields(
            object,
            &["location", "description", "severity"],
            "verdict error",
        )
        .map_err(|error| error.to_string())?;
        required_value_string(object, "location").map_err(|error| error.to_string())?;
        required_value_string(object, "description").map_err(|error| error.to_string())?;
        match required_value_string(object, "severity").map_err(|error| error.to_string())? {
            "fatal" => counts.fatal += 1,
            "major" => counts.major += 1,
            "minor" => counts.minor += 1,
            other => return Err(format!("unknown severity `{other}`")),
        }
    }
    Ok(counts)
}

pub(super) fn required_pairs(
    object: &Map<String, Value>,
) -> Result<Vec<TournamentPairInput>, ExtensionError> {
    let values = object
        .get("pairs")
        .ok_or_else(|| input_error("pairs is required"))?
        .as_array()
        .ok_or_else(|| input_error("pairs must be an array"))?;
    if values.is_empty() {
        return Err(input_error("pairs must not be empty"));
    }
    values
        .iter()
        .map(|value| {
            let object = value
                .as_object()
                .ok_or_else(|| input_error("each pair must be a JSON object"))?;
            reject_unknown_fields(
                object,
                &["candidate_spawn_event_id", "verdict_spawn_event_id"],
            )?;
            Ok(TournamentPairInput {
                candidate_spawn_event_id: required_bounded_string(
                    object,
                    "candidate_spawn_event_id",
                    128,
                )?,
                verdict_spawn_event_id: required_bounded_string(
                    object,
                    "verdict_spawn_event_id",
                    128,
                )?,
            })
        })
        .collect()
}

pub(super) fn validate_unique_pair_ids(
    pairs: &[TournamentPairInput],
) -> Result<(), ExtensionError> {
    let mut candidates = BTreeSet::new();
    let mut verdicts = BTreeSet::new();
    for pair in pairs {
        if !candidates.insert(pair.candidate_spawn_event_id.as_str()) {
            return Err(input_error(format!(
                "duplicate candidate_spawn_event_id `{}` in tournament pairs",
                pair.candidate_spawn_event_id
            )));
        }
        if !verdicts.insert(pair.verdict_spawn_event_id.as_str()) {
            return Err(input_error(format!(
                "duplicate verdict_spawn_event_id `{}` in tournament pairs",
                pair.verdict_spawn_event_id
            )));
        }
    }
    Ok(())
}

pub(super) fn validate_tournament_pair(
    input: &TournamentPairInput,
    candidate_pair: &AgentPair,
    verdict_pair: &AgentPair,
) -> Result<(), ExtensionError> {
    require_persona_prefix(
        &candidate_pair.spawn,
        GENERATOR_PERSONA_PREFIX,
        &input.candidate_spawn_event_id,
        "candidate",
    )?;
    require_persona_exact(
        &verdict_pair.spawn,
        VERIFIER_PERSONA,
        &input.verdict_spawn_event_id,
        "verdict",
    )?;
    if let Ok(candidate) = Candidate::from_result(candidate_pair) {
        let verdict_proof = extract_proof(payload_string(&verdict_pair.spawn, "task")).ok_or_else(
            || {
                input_error(format!(
                    "verdict_spawn_event_id `{}` task is missing MaxProof proof markers for candidate_spawn_event_id `{}`",
                    input.verdict_spawn_event_id, input.candidate_spawn_event_id
                ))
            },
        )?;
        let candidate_digest = sha256_hex(candidate.proof.as_bytes());
        let verdict_digest = sha256_hex(verdict_proof.as_bytes());
        if candidate_digest != verdict_digest {
            return Err(input_error(format!(
                "verdict_spawn_event_id `{}` proof digest does not match candidate_spawn_event_id `{}` proof digest",
                input.verdict_spawn_event_id, input.candidate_spawn_event_id
            )));
        }
    }
    Ok(())
}

pub(super) fn required_problem_digest(
    candidate_pair: &AgentPair,
    candidate_spawn_event_id: &str,
) -> Result<String, ExtensionError> {
    candidate_pair
        .spawn
        .payload
        .get("task")
        .and_then(Value::as_str)
        .and_then(|task| extract_problem(Some(task)))
        .map(|problem| sha256_hex(problem.as_bytes()))
        .ok_or_else(|| {
            input_error(format!(
                "candidate_spawn_event_id `{candidate_spawn_event_id}` task is missing MaxProof problem markers"
            ))
        })
}

fn require_persona_prefix(
    event: &EventEnvelope,
    prefix: &str,
    spawn_event_id: &str,
    role: &str,
) -> Result<(), ExtensionError> {
    let persona = payload_string(event, "persona").unwrap_or_default();
    if !persona.starts_with(prefix) {
        return Err(input_error(format!(
            "{role} spawn_event_id `{spawn_event_id}` persona must start with `{prefix}`"
        )));
    }
    Ok(())
}

fn require_persona_exact(
    event: &EventEnvelope,
    expected: &str,
    spawn_event_id: &str,
    role: &str,
) -> Result<(), ExtensionError> {
    let persona = payload_string(event, "persona").unwrap_or_default();
    if persona != expected {
        return Err(input_error(format!(
            "{role} spawn_event_id `{spawn_event_id}` persona must be `{expected}`"
        )));
    }
    Ok(())
}

pub(super) fn required_string_array(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Vec<String>, ExtensionError> {
    let values = object
        .get(field)
        .ok_or_else(|| input_error(format!("{field} is required")))?
        .as_array()
        .ok_or_else(|| input_error(format!("{field} must be an array of strings")))?;
    if values.is_empty() {
        return Err(input_error(format!("{field} must not be empty")));
    }
    values
        .iter()
        .map(|value| {
            let string = value
                .as_str()
                .ok_or_else(|| input_error(format!("{field} must be an array of strings")))?;
            if string.is_empty() {
                return Err(input_error(format!("{field} entries must not be empty")));
            }
            Ok(string.to_owned())
        })
        .collect()
}

pub(super) fn parse_population_size(value: Option<&Value>) -> Result<usize, ExtensionError> {
    let Some(value) = value else {
        return Ok(DEFAULT_POPULATION_SIZE);
    };
    if value.is_null() {
        return Ok(DEFAULT_POPULATION_SIZE);
    }
    let parsed = value
        .as_u64()
        .ok_or_else(|| input_error("population_size must be an unsigned integer"))?;
    let parsed =
        usize::try_from(parsed).map_err(|_| input_error("population_size is too large"))?;
    if !(MIN_POPULATION_SIZE..=MAX_POPULATION_SIZE).contains(&parsed) {
        return Err(input_error(format!(
            "population_size must be in range {MIN_POPULATION_SIZE}..={MAX_POPULATION_SIZE}"
        )));
    }
    Ok(parsed)
}

pub(super) fn parse_positive_u64(
    object: &Map<String, Value>,
    field: &'static str,
    default: u64,
) -> Result<u64, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    let parsed = value
        .as_u64()
        .ok_or_else(|| input_error(format!("{field} must be a positive integer")))?;
    if parsed == 0 {
        return Err(input_error(format!("{field} must be greater than zero")));
    }
    Ok(parsed)
}

pub(super) fn parse_optional_positive_usize(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<usize>, ExtensionError> {
    if object.get(field).is_none_or(Value::is_null) {
        return Ok(None);
    }
    let parsed = parse_positive_u64(object, field, 1)?;
    usize::try_from(parsed)
        .map(Some)
        .map_err(|_| input_error(format!("{field} is too large")))
}

pub(super) fn optional_string(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, ExtensionError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| input_error(format!("{field} must be a string")))
}

pub(super) fn required_bounded_string(
    object: &Map<String, Value>,
    field: &'static str,
    max_bytes: usize,
) -> Result<String, ExtensionError> {
    let value = object
        .get(field)
        .ok_or_else(|| input_error(format!("{field} is required")))?
        .as_str()
        .ok_or_else(|| input_error(format!("{field} must be a string")))?;
    if value.is_empty() {
        return Err(input_error(format!("{field} must not be empty")));
    }
    if value.len() > max_bytes {
        return Err(input_error(format!(
            "{field} exceeds maximum of {max_bytes} bytes"
        )));
    }
    Ok(value.to_owned())
}

pub(super) fn required_value_string<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, ExtensionError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| input_error(format!("{field} must be a string")))
}

pub(super) fn bounded_value_string<'a>(
    value: &'a str,
    field: &'static str,
    max_bytes: usize,
) -> Result<&'a str, ExtensionError> {
    if value.len() > max_bytes {
        return Err(input_error(format!(
            "{field} exceeds maximum of {max_bytes} bytes"
        )));
    }
    Ok(value)
}

pub(super) fn require_schema(
    object: &Map<String, Value>,
    schema: &'static str,
) -> Result<(), ExtensionError> {
    let actual = required_value_string(object, "schema")?;
    if actual != schema {
        return Err(input_error(format!("schema must be {schema}")));
    }
    Ok(())
}

pub(super) fn reject_unknown_fields(
    object: &Map<String, Value>,
    allowed: &[&'static str],
) -> Result<(), ExtensionError> {
    reject_value_fields(object, allowed)
}

fn reject_value_fields(
    object: &Map<String, Value>,
    allowed: &[&'static str],
) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(input_error(format!("unknown input field `{key}`")));
        }
    }
    Ok(())
}

pub(super) fn reject_output_fields(
    object: &Map<String, Value>,
    allowed: &[&'static str],
    context: &'static str,
) -> Result<(), ExtensionError> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(input_error(format!("unknown field `{key}` in {context}")));
        }
    }
    Ok(())
}

pub(super) fn payload_string<'a>(event: &'a EventEnvelope, field: &'static str) -> Option<&'a str> {
    event.payload.get(field).and_then(Value::as_str)
}

pub(super) fn payload_bool(event: &EventEnvelope, field: &'static str) -> Option<bool> {
    event.payload.get(field).and_then(Value::as_bool)
}

pub(super) fn extract_problem(task: Option<&str>) -> Option<&str> {
    let task = task?;
    let start = task.find(PROBLEM_BEGIN)? + PROBLEM_BEGIN.len();
    let rest = &task[start..];
    let end = rest.find(PROBLEM_END)?;
    Some(&rest[..end])
}

fn extract_proof(task: Option<&str>) -> Option<&str> {
    let task = task?;
    let start = task.find(PROOF_BEGIN)? + PROOF_BEGIN.len();
    let rest = &task[start..];
    let end = rest.find(PROOF_END)?;
    Some(&rest[..end])
}

pub(super) fn validation_message(
    candidate: Result<Candidate, String>,
    verdict: Result<Verdict, String>,
) -> String {
    match (candidate.err(), verdict.err()) {
        (Some(candidate), Some(verdict)) => {
            format!("candidate invalid: {candidate}; verdict invalid: {verdict}")
        }
        (Some(candidate), None) => format!("candidate invalid: {candidate}"),
        (None, Some(verdict)) => format!("verdict invalid: {verdict}"),
        (None, None) => "unknown validation error".to_owned(),
    }
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

pub(super) fn unpaired_error(spawn_event_id: &str) -> ExtensionError {
    input_error(format!(
        "agent.spawn `{spawn_event_id}` and its paired agent.result were not both found in the bounded provenance page; widen the window with limit, scan_limit, or after_event_id so the complete spawn/result pair is visible"
    ))
}

pub(super) fn input_error(message: impl Into<String>) -> ExtensionError {
    ExtensionError::Message(message.into())
}
