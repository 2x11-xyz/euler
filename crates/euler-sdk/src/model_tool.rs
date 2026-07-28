use serde_json::{Map, Number, Value};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use thiserror::Error;

use crate::extension_model_text_is_format_safe;

pub const MAX_MODEL_TOOL_NAME_BYTES: usize = 64;
pub const MAX_MODEL_TOOL_DESCRIPTION_BYTES: usize = 1024;
pub const MAX_MODEL_TOOL_SCHEMA_BYTES: usize = 16 * 1024;
pub const MAX_MODEL_TOOL_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_MODEL_TOOL_OUTPUT_BYTES: usize = 256 * 1024;

const MAX_SCHEMA_DEPTH: usize = 8;
const MAX_SCHEMA_PROPERTIES: usize = 64;
const MAX_ENUM_VALUES: usize = 64;
const SUPPORTED_FIELDS: &[&str] = &[
    "type",
    "description",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
];

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct ModelToolValidationError {
    message: String,
}

impl ModelToolValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub fn validate_model_tool_descriptor(
    descriptor: &crate::ModelToolDescriptor,
) -> Result<(), ModelToolValidationError> {
    validate_tool_name(&descriptor.name)?;
    validate_description(&descriptor.description)?;
    let schema_bytes = serde_json::to_vec(&descriptor.input_schema)
        .map_err(|_| ModelToolValidationError::new("model tool schema is not valid JSON"))?
        .len();
    if schema_bytes > MAX_MODEL_TOOL_SCHEMA_BYTES {
        return Err(ModelToolValidationError::new(format!(
            "model tool schema is too large: {schema_bytes} bytes exceeds {MAX_MODEL_TOOL_SCHEMA_BYTES}"
        )));
    }
    let mut properties = 0usize;
    validate_schema(&descriptor.input_schema, "$", 0, &mut properties)?;
    let root = descriptor
        .input_schema
        .as_object()
        .expect("validated schema root");
    if root.get("type").and_then(Value::as_str) != Some("object") {
        return Err(ModelToolValidationError::new(
            "model tool schema root type must be object",
        ));
    }
    Ok(())
}

pub fn validate_model_tool_input(
    descriptor: &crate::ModelToolDescriptor,
    input: &Value,
) -> Result<(), ModelToolValidationError> {
    validate_model_tool_descriptor(descriptor)?;
    let input_bytes = serde_json::to_vec(input)
        .map_err(|_| ModelToolValidationError::new("model tool input is not valid JSON"))?
        .len();
    if input_bytes > MAX_MODEL_TOOL_INPUT_BYTES {
        return Err(ModelToolValidationError::new(format!(
            "model tool input is too large: {input_bytes} bytes exceeds {MAX_MODEL_TOOL_INPUT_BYTES}"
        )));
    }
    validate_value(&descriptor.input_schema, input, "$")
}

fn validate_tool_name(name: &str) -> Result<(), ModelToolValidationError> {
    if name.is_empty() || name.len() > MAX_MODEL_TOOL_NAME_BYTES {
        return Err(ModelToolValidationError::new(format!(
            "model tool name must be 1..={MAX_MODEL_TOOL_NAME_BYTES} bytes"
        )));
    }
    let bytes = name.as_bytes();
    if !(bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        || !bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(*byte, b'_' | b'-')
        })
    {
        return Err(ModelToolValidationError::new(
            "model tool name must begin with a lowercase letter or `_` and contain only lowercase ASCII letters, digits, `_`, or `-`",
        ));
    }
    Ok(())
}

fn validate_description(description: &str) -> Result<(), ModelToolValidationError> {
    if description.trim().is_empty() || description.len() > MAX_MODEL_TOOL_DESCRIPTION_BYTES {
        return Err(ModelToolValidationError::new(format!(
            "model tool description must be nonempty and at most {MAX_MODEL_TOOL_DESCRIPTION_BYTES} bytes"
        )));
    }
    if !model_definition_text_is_safe(description) {
        return Err(ModelToolValidationError::new(
            "model tool description contains unsafe characters",
        ));
    }
    Ok(())
}

fn validate_schema(
    schema: &Value,
    path: &str,
    depth: usize,
    property_count: &mut usize,
) -> Result<(), ModelToolValidationError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(ModelToolValidationError::new(format!(
            "model tool schema exceeds maximum depth {MAX_SCHEMA_DEPTH} at {path}"
        )));
    }
    let object = schema.as_object().ok_or_else(|| {
        ModelToolValidationError::new(format!("model tool schema at {path} must be an object"))
    })?;
    for field in object.keys() {
        if !SUPPORTED_FIELDS.contains(&field.as_str()) {
            return Err(ModelToolValidationError::new(format!(
                "unsupported model tool schema field `{field}` at {path}"
            )));
        }
    }
    validate_schema_description(object, path)?;
    let schema_type = object.get("type").and_then(Value::as_str).ok_or_else(|| {
        ModelToolValidationError::new(format!(
            "model tool schema at {path} must declare one string type"
        ))
    })?;
    if !matches!(
        schema_type,
        "object" | "array" | "string" | "integer" | "number" | "boolean" | "null"
    ) {
        return Err(ModelToolValidationError::new(format!(
            "unsupported model tool schema type `{schema_type}` at {path}"
        )));
    }
    validate_enum(object, schema_type, path)?;
    match schema_type {
        "object" => validate_object_schema(object, path, depth, property_count)?,
        "array" => validate_array_schema(object, path, depth, property_count)?,
        "string" => validate_string_schema(object, path)?,
        "integer" | "number" => validate_number_schema(object, path)?,
        "boolean" | "null" => reject_fields(
            object,
            &[
                "properties",
                "required",
                "additionalProperties",
                "items",
                "minimum",
                "maximum",
                "minLength",
                "maxLength",
                "minItems",
                "maxItems",
            ],
            path,
        )?,
        _ => unreachable!("validated schema type"),
    }
    Ok(())
}

fn validate_schema_description(
    object: &Map<String, Value>,
    path: &str,
) -> Result<(), ModelToolValidationError> {
    let Some(description) = object.get("description") else {
        return Ok(());
    };
    let description = description.as_str().ok_or_else(|| {
        ModelToolValidationError::new(format!("schema description at {path} must be a string"))
    })?;
    if description.len() > MAX_MODEL_TOOL_DESCRIPTION_BYTES
        || !model_definition_text_is_safe(description)
    {
        return Err(ModelToolValidationError::new(format!(
            "schema description at {path} is invalid"
        )));
    }
    Ok(())
}

fn validate_object_schema(
    object: &Map<String, Value>,
    path: &str,
    depth: usize,
    property_count: &mut usize,
) -> Result<(), ModelToolValidationError> {
    reject_fields(
        object,
        &[
            "items",
            "minimum",
            "maximum",
            "minLength",
            "maxLength",
            "minItems",
            "maxItems",
        ],
        path,
    )?;
    if object.get("additionalProperties").and_then(Value::as_bool) != Some(false) {
        return Err(ModelToolValidationError::new(format!(
            "object schema at {path} must set additionalProperties to false"
        )));
    }
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            ModelToolValidationError::new(format!(
                "object schema at {path} must declare properties"
            ))
        })?;
    *property_count = property_count.saturating_add(properties.len());
    if *property_count > MAX_SCHEMA_PROPERTIES {
        return Err(ModelToolValidationError::new(format!(
            "model tool schema has more than {MAX_SCHEMA_PROPERTIES} properties"
        )));
    }
    for (name, child) in properties {
        if name.is_empty()
            || name.len() > MAX_MODEL_TOOL_NAME_BYTES
            || !model_definition_text_is_safe(name)
        {
            return Err(ModelToolValidationError::new(format!(
                "invalid property name at {path}"
            )));
        }
        validate_schema(
            child,
            &format!("{path}.properties.{name}"),
            depth + 1,
            property_count,
        )?;
    }
    let required = object
        .get("required")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ModelToolValidationError::new(format!("object schema at {path} must declare required"))
        })?;
    let mut seen = BTreeSet::new();
    for value in required {
        let name = value.as_str().ok_or_else(|| {
            ModelToolValidationError::new(format!("required entries at {path} must be strings"))
        })?;
        if !properties.contains_key(name) || !seen.insert(name) {
            return Err(ModelToolValidationError::new(format!(
                "required entry `{name}` at {path} is missing or duplicated"
            )));
        }
    }
    Ok(())
}

fn validate_array_schema(
    object: &Map<String, Value>,
    path: &str,
    depth: usize,
    property_count: &mut usize,
) -> Result<(), ModelToolValidationError> {
    reject_fields(
        object,
        &[
            "properties",
            "required",
            "additionalProperties",
            "minimum",
            "maximum",
            "minLength",
            "maxLength",
        ],
        path,
    )?;
    let items = object.get("items").ok_or_else(|| {
        ModelToolValidationError::new(format!("array schema at {path} must declare items"))
    })?;
    validate_schema(items, &format!("{path}.items"), depth + 1, property_count)?;
    validate_u64_range(object, "minItems", "maxItems", path)
}

fn validate_string_schema(
    object: &Map<String, Value>,
    path: &str,
) -> Result<(), ModelToolValidationError> {
    reject_fields(
        object,
        &[
            "properties",
            "required",
            "additionalProperties",
            "items",
            "minimum",
            "maximum",
            "minItems",
            "maxItems",
        ],
        path,
    )?;
    validate_u64_range(object, "minLength", "maxLength", path)
}

fn validate_number_schema(
    object: &Map<String, Value>,
    path: &str,
) -> Result<(), ModelToolValidationError> {
    reject_fields(
        object,
        &[
            "properties",
            "required",
            "additionalProperties",
            "items",
            "minLength",
            "maxLength",
            "minItems",
            "maxItems",
        ],
        path,
    )?;
    let minimum = optional_number(object, "minimum", path)?;
    let maximum = optional_number(object, "maximum", path)?;
    if minimum
        .zip(maximum)
        .is_some_and(|(min, max)| compare_json_numbers(min, max).is_gt())
    {
        return Err(ModelToolValidationError::new(format!(
            "schema minimum exceeds maximum at {path}"
        )));
    }
    Ok(())
}

fn validate_enum(
    object: &Map<String, Value>,
    schema_type: &str,
    path: &str,
) -> Result<(), ModelToolValidationError> {
    let Some(values) = object.get("enum") else {
        return Ok(());
    };
    let values = values.as_array().ok_or_else(|| {
        ModelToolValidationError::new(format!("schema enum at {path} must be an array"))
    })?;
    if values.is_empty() || values.len() > MAX_ENUM_VALUES {
        return Err(ModelToolValidationError::new(format!(
            "schema enum at {path} must have 1..={MAX_ENUM_VALUES} entries"
        )));
    }
    for value in values {
        if !value_matches_type(value, schema_type) {
            return Err(ModelToolValidationError::new(format!(
                "schema enum value at {path} does not match type {schema_type}"
            )));
        }
        if schema_type == "string"
            && value
                .as_str()
                .is_some_and(|value| !model_definition_text_is_safe(value))
        {
            return Err(ModelToolValidationError::new(format!(
                "schema enum value at {path} contains unsafe characters"
            )));
        }
    }
    Ok(())
}

fn model_definition_text_is_safe(text: &str) -> bool {
    !text.chars().any(char::is_control) && extension_model_text_is_format_safe(text)
}

fn validate_u64_range(
    object: &Map<String, Value>,
    minimum: &str,
    maximum: &str,
    path: &str,
) -> Result<(), ModelToolValidationError> {
    let min = optional_u64(object, minimum, path)?;
    let max = optional_u64(object, maximum, path)?;
    if min.zip(max).is_some_and(|(min, max)| min > max) {
        return Err(ModelToolValidationError::new(format!(
            "schema {minimum} exceeds {maximum} at {path}"
        )));
    }
    Ok(())
}

fn optional_u64(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<Option<u64>, ModelToolValidationError> {
    object
        .get(field)
        .map(|value| {
            value.as_u64().ok_or_else(|| {
                ModelToolValidationError::new(format!(
                    "schema {field} at {path} must be a nonnegative integer"
                ))
            })
        })
        .transpose()
}

fn optional_number<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<Option<&'a Number>, ModelToolValidationError> {
    object
        .get(field)
        .map(|value| {
            value.as_number().ok_or_else(|| {
                ModelToolValidationError::new(format!(
                    "schema {field} at {path} must be a finite number"
                ))
            })
        })
        .transpose()
}

fn reject_fields(
    object: &Map<String, Value>,
    fields: &[&str],
    path: &str,
) -> Result<(), ModelToolValidationError> {
    if let Some(field) = fields.iter().find(|field| object.contains_key(**field)) {
        return Err(ModelToolValidationError::new(format!(
            "schema field `{field}` is not valid for this type at {path}"
        )));
    }
    Ok(())
}

fn validate_value(
    schema: &Value,
    value: &Value,
    path: &str,
) -> Result<(), ModelToolValidationError> {
    let object = schema
        .as_object()
        .expect("descriptor validation precedes input validation");
    let schema_type = object
        .get("type")
        .and_then(Value::as_str)
        .expect("descriptor validation precedes input validation");
    if !value_matches_type(value, schema_type) {
        return Err(ModelToolValidationError::new(format!(
            "model tool input at {path} must be {schema_type}"
        )));
    }
    if object
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|values| !values.contains(value))
    {
        return Err(ModelToolValidationError::new(format!(
            "model tool input at {path} is not an allowed enum value"
        )));
    }
    match schema_type {
        "object" => validate_object_value(object, value, path)?,
        "array" => validate_array_value(object, value, path)?,
        "string" => validate_string_value(object, value, path)?,
        "integer" | "number" => validate_number_value(object, value, path)?,
        "boolean" | "null" => {}
        _ => unreachable!("validated schema type"),
    }
    Ok(())
}

fn validate_object_value(
    schema: &Map<String, Value>,
    value: &Value,
    path: &str,
) -> Result<(), ModelToolValidationError> {
    let value = value.as_object().expect("validated object input");
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("validated object schema");
    if let Some(extra) = value.keys().find(|name| !properties.contains_key(*name)) {
        return Err(ModelToolValidationError::new(format!(
            "model tool input has unknown field `{extra}` at {path}"
        )));
    }
    for name in schema
        .get("required")
        .and_then(Value::as_array)
        .expect("validated required list")
        .iter()
        .filter_map(Value::as_str)
    {
        if !value.contains_key(name) {
            return Err(ModelToolValidationError::new(format!(
                "model tool input is missing required field `{name}` at {path}"
            )));
        }
    }
    for (name, child) in value {
        validate_value(
            properties.get(name).expect("unknown fields rejected"),
            child,
            &format!("{path}.{name}"),
        )?;
    }
    Ok(())
}

fn validate_array_value(
    schema: &Map<String, Value>,
    value: &Value,
    path: &str,
) -> Result<(), ModelToolValidationError> {
    let value = value.as_array().expect("validated array input");
    enforce_usize_bounds(schema, "minItems", "maxItems", value.len(), path)?;
    let items = schema.get("items").expect("validated array schema");
    for (index, child) in value.iter().enumerate() {
        validate_value(items, child, &format!("{path}[{index}]"))?;
    }
    Ok(())
}

fn validate_string_value(
    schema: &Map<String, Value>,
    value: &Value,
    path: &str,
) -> Result<(), ModelToolValidationError> {
    let value = value.as_str().expect("validated string input");
    enforce_usize_bounds(
        schema,
        "minLength",
        "maxLength",
        value.chars().count(),
        path,
    )
}

fn validate_number_value(
    schema: &Map<String, Value>,
    value: &Value,
    path: &str,
) -> Result<(), ModelToolValidationError> {
    let value = value.as_number().expect("validated numeric input");
    if schema
        .get("minimum")
        .and_then(Value::as_number)
        .is_some_and(|minimum| compare_json_numbers(value, minimum).is_lt())
    {
        return Err(ModelToolValidationError::new(format!(
            "model tool input at {path} is below minimum"
        )));
    }
    if schema
        .get("maximum")
        .and_then(Value::as_number)
        .is_some_and(|maximum| compare_json_numbers(value, maximum).is_gt())
    {
        return Err(ModelToolValidationError::new(format!(
            "model tool input at {path} is above maximum"
        )));
    }
    Ok(())
}

fn compare_json_numbers(left: &Number, right: &Number) -> Ordering {
    // Compare the canonical JSON decimals that are advertised to the provider.
    // With serde_json's default (non-arbitrary-precision) Number domain these
    // renderings are bounded i64/u64 or finite-f64 strings. Normalization keeps
    // integer precision while making exponent and fractional forms comparable
    // without coercing either side through f64.
    let left = NormalizedDecimal::from_number(left);
    let right = NormalizedDecimal::from_number(right);
    match (left.negative, right.negative) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (true, true) => right.compare_magnitude(&left),
        (false, false) => left.compare_magnitude(&right),
    }
}

struct NormalizedDecimal {
    negative: bool,
    digits: Vec<u8>,
    exponent: i32,
}

impl NormalizedDecimal {
    fn from_number(number: &Number) -> Self {
        let rendered = number.to_string();
        let (negative, unsigned) = rendered
            .strip_prefix('-')
            .map_or((false, rendered.as_str()), |value| (true, value));
        let exponent_at = unsigned.find(['e', 'E']);
        let (mantissa, explicit_exponent) = exponent_at.map_or((unsigned, 0_i32), |index| {
            let exponent = unsigned[index + 1..]
                .parse()
                .expect("serde_json::Number exponent is valid");
            (&unsigned[..index], exponent)
        });
        let fractional_digits = mantissa
            .find('.')
            .map_or(0, |decimal| mantissa.len() - decimal - 1);
        let mut digits = mantissa
            .bytes()
            .filter(u8::is_ascii_digit)
            .collect::<Vec<_>>();
        let first_nonzero = digits.iter().position(|digit| *digit != b'0');
        let Some(first_nonzero) = first_nonzero else {
            return Self {
                negative: false,
                digits: vec![b'0'],
                exponent: 0,
            };
        };
        digits.drain(..first_nonzero);
        let mut exponent = explicit_exponent
            - i32::try_from(fractional_digits).expect("JSON number length fits i32");
        while digits.last() == Some(&b'0') {
            digits.pop();
            exponent += 1;
        }
        Self {
            negative,
            digits,
            exponent,
        }
    }

    fn compare_magnitude(&self, other: &Self) -> Ordering {
        let self_zero = self.digits.as_slice() == b"0";
        let other_zero = other.digits.as_slice() == b"0";
        match (self_zero, other_zero) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
        let self_rank = i64::try_from(self.digits.len()).expect("JSON number length fits i64")
            + i64::from(self.exponent);
        let other_rank = i64::try_from(other.digits.len()).expect("JSON number length fits i64")
            + i64::from(other.exponent);
        self_rank.cmp(&other_rank).then_with(|| {
            let width = self.digits.len().max(other.digits.len());
            (0..width)
                .map(|index| self.digits.get(index).copied().unwrap_or(b'0'))
                .cmp((0..width).map(|index| other.digits.get(index).copied().unwrap_or(b'0')))
        })
    }
}

fn enforce_usize_bounds(
    schema: &Map<String, Value>,
    minimum: &str,
    maximum: &str,
    actual: usize,
    path: &str,
) -> Result<(), ModelToolValidationError> {
    let actual = u64::try_from(actual).unwrap_or(u64::MAX);
    if schema
        .get(minimum)
        .and_then(Value::as_u64)
        .is_some_and(|minimum| actual < minimum)
    {
        return Err(ModelToolValidationError::new(format!(
            "model tool input at {path} is below {minimum}"
        )));
    }
    if schema
        .get(maximum)
        .and_then(Value::as_u64)
        .is_some_and(|maximum| actual > maximum)
    {
        return Err(ModelToolValidationError::new(format!(
            "model tool input at {path} exceeds {maximum}"
        )));
    }
    Ok(())
}

fn value_matches_type(value: &Value, schema_type: &str) -> bool {
    match schema_type {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn descriptor() -> crate::ModelToolDescriptor {
        crate::ModelToolDescriptor {
            name: "update_plan".to_owned(),
            description: "Replace the current workflow-owned plan.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "explanation": {"type": "string", "maxLength": 1024},
                    "items": {
                        "type": "array",
                        "maxItems": 64,
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": {"type": "string", "minLength": 1, "maxLength": 1024},
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["step", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        }
    }

    #[test]
    fn validates_closed_nested_schema_and_input() {
        let descriptor = descriptor();
        validate_model_tool_descriptor(&descriptor).expect("descriptor");
        validate_model_tool_input(
            &descriptor,
            &json!({
                "items": [
                    {"step": "Inspect", "status": "completed"},
                    {"step": "Implement", "status": "in_progress"}
                ]
            }),
        )
        .expect("input");
    }

    #[test]
    fn rejects_open_objects_and_unsupported_keywords() {
        let mut open_descriptor = descriptor();
        open_descriptor.input_schema["additionalProperties"] = json!(true);
        assert!(validate_model_tool_descriptor(&open_descriptor)
            .expect_err("open object")
            .to_string()
            .contains("additionalProperties"));

        let mut unsupported_descriptor = descriptor();
        unsupported_descriptor.input_schema["oneOf"] = json!([]);
        assert!(validate_model_tool_descriptor(&unsupported_descriptor)
            .expect_err("unsupported")
            .to_string()
            .contains("unsupported"));
    }

    #[test]
    fn rejects_unknown_missing_and_wrong_enum_inputs() {
        let descriptor = descriptor();
        for input in [
            json!({"items": [], "other": true}),
            json!({}),
            json!({"items": [{"step": "Inspect", "status": "blocked"}]}),
        ] {
            assert!(validate_model_tool_input(&descriptor, &input).is_err());
        }
    }

    #[test]
    fn input_validation_rejects_an_invalid_descriptor_without_panicking() {
        let mut descriptor = descriptor();
        descriptor.input_schema = json!({"type": "object"});
        assert!(validate_model_tool_input(&descriptor, &json!({})).is_err());
    }

    #[test]
    fn descriptor_and_input_resource_bounds_are_enforced() {
        let mut oversized_name = descriptor();
        oversized_name.name = "n".repeat(MAX_MODEL_TOOL_NAME_BYTES + 1);
        assert!(validate_model_tool_descriptor(&oversized_name).is_err());

        let mut oversized_description = descriptor();
        oversized_description.description = "d".repeat(MAX_MODEL_TOOL_DESCRIPTION_BYTES + 1);
        assert!(validate_model_tool_descriptor(&oversized_description).is_err());

        let mut oversized_schema = descriptor();
        oversized_schema.input_schema["description"] =
            json!("s".repeat(MAX_MODEL_TOOL_SCHEMA_BYTES));
        assert!(validate_model_tool_descriptor(&oversized_schema).is_err());

        let unbounded_string = crate::ModelToolDescriptor {
            name: "store_value".to_owned(),
            description: "Store one value.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": false
            }),
        };
        assert!(validate_model_tool_input(
            &unbounded_string,
            &json!({"value": "v".repeat(MAX_MODEL_TOOL_INPUT_BYTES)})
        )
        .is_err());
    }

    #[test]
    fn numeric_bounds_are_exact_beyond_f64_integer_precision() {
        const F64_EXACT_INTEGER_EDGE: u64 = 9_007_199_254_740_992;
        let bounded_integer = crate::ModelToolDescriptor {
            name: "store_integer".to_owned(),
            description: "Store one exactly bounded integer.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "value": {
                        "type": "integer",
                        "maximum": F64_EXACT_INTEGER_EDGE
                    }
                },
                "required": ["value"],
                "additionalProperties": false
            }),
        };
        validate_model_tool_descriptor(&bounded_integer).expect("valid exact bound");
        validate_model_tool_input(&bounded_integer, &json!({"value": F64_EXACT_INTEGER_EDGE}))
            .expect("boundary is accepted");
        let above = validate_model_tool_input(
            &bounded_integer,
            &json!({"value": F64_EXACT_INTEGER_EDGE + 1}),
        )
        .expect_err("adjacent integer above bound must be rejected");
        assert!(above.to_string().contains("above maximum"));

        let mut reversed = bounded_integer;
        reversed.input_schema["properties"]["value"]["minimum"] = json!(F64_EXACT_INTEGER_EDGE + 1);
        assert!(validate_model_tool_descriptor(&reversed)
            .expect_err("exactly reversed bounds")
            .to_string()
            .contains("minimum exceeds maximum"));
    }

    #[test]
    fn numeric_ordering_handles_mixed_and_exponent_boundaries() {
        let cases = [
            (json!(-0.25), json!(0), Ordering::Less),
            (json!(-3), json!(-2.5), Ordering::Less),
            (json!(1.125), json!(1.25), Ordering::Less),
            (json!(1.0), json!(1), Ordering::Equal),
            (json!(-0.0), json!(0), Ordering::Equal),
            (json!(u64::MAX), json!(i64::MAX), Ordering::Greater),
            (json!(-42_i64), json!(-42.0_f64), Ordering::Equal),
            (
                json!(9_007_199_254_740_993_u64),
                json!(9_007_199_254_740_992.0_f64),
                Ordering::Greater,
            ),
            (json!(1e100_f64), json!(u64::MAX), Ordering::Greater),
            (json!(1e-100_f64), json!(9e-101_f64), Ordering::Greater),
            (json!(-1e100_f64), json!(-9e99_f64), Ordering::Less),
        ];
        for (left, right, expected) in cases {
            assert_eq!(
                compare_json_numbers(
                    left.as_number().expect("left number"),
                    right.as_number().expect("right number")
                ),
                expected,
                "{left} compared with {right}"
            );
        }
    }

    #[test]
    fn extension_authored_model_definition_text_rejects_controls_and_format_spoofs() {
        let unsafe_text = ["line\nbreak", "hidden\u{00AD}text", "split\u{2028}text"];

        for text in unsafe_text {
            let mut top_level = descriptor();
            top_level.description = text.to_owned();
            assert!(
                validate_model_tool_descriptor(&top_level).is_err(),
                "accepted top-level description {text:?}"
            );

            let mut schema_description = descriptor();
            schema_description.input_schema["description"] = json!(text);
            assert!(
                validate_model_tool_descriptor(&schema_description).is_err(),
                "accepted schema description {text:?}"
            );

            let mut property_name = descriptor();
            let value = property_name.input_schema["properties"]
                .as_object_mut()
                .expect("properties")
                .remove("explanation")
                .expect("property");
            property_name.input_schema["properties"]
                .as_object_mut()
                .expect("properties")
                .insert(text.to_owned(), value);
            property_name.input_schema["required"] = json!(["items"]);
            assert!(
                validate_model_tool_descriptor(&property_name).is_err(),
                "accepted property name {text:?}"
            );

            let mut enum_value = descriptor();
            enum_value.input_schema["properties"]["items"]["items"]["properties"]["status"]
                ["enum"] = json!(["pending", text]);
            assert!(
                validate_model_tool_descriptor(&enum_value).is_err(),
                "accepted enum value {text:?}"
            );
        }
    }
}
