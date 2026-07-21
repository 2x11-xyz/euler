//! Deterministic context-budget accounting for pinned project context.
//!
//! The frozen proxy is four rendered UTF-8 bytes per token. Equality with the
//! known limit fits; one token over does not. Arithmetic overflow or an
//! unrepresentable configured value fails before provider invocation, and
//! pinned content is never truncated or demoted to make an equation pass.

use euler_provider::{ModelInputItem, ModelRequest};

/// Fixed admission margin (tokens) added on top of the byte proxy at
/// snapshot admission (project-context contract, "Framing and canvas
/// admission").
const ADMISSION_MARGIN_TOKENS: u64 = 1024;

pub(crate) const BYTES_PER_TOKEN: u64 = 4;

/// Tokens required at snapshot admission, per the contract formula
/// `ceil((fixed + framed) / 4) + 1024 + output_reserve`. `None` signals
/// arithmetic overflow, which must fail before provider invocation.
pub(crate) fn admission_required_tokens(
    fixed_instruction_bytes: usize,
    framed_project_context_bytes: usize,
    output_reserve: u64,
) -> Option<u64> {
    let bytes =
        (fixed_instruction_bytes as u64).checked_add(framed_project_context_bytes as u64)?;
    bytes
        .div_ceil(BYTES_PER_TOKEN)
        .checked_add(ADMISSION_MARGIN_TOKENS)?
        .checked_add(output_reserve)
}

/// Tokens required at request time: the same checked proxy over fixed
/// instructions, every provider-neutral input item, and serialized tool
/// definitions, plus the output reserve.
pub(crate) fn request_required_tokens(request: &ModelRequest, output_reserve: u64) -> Option<u64> {
    let mut bytes = request.instructions.len() as u64;
    for item in &request.input {
        bytes = bytes.checked_add(input_item_bytes(item))?;
    }
    for tool in &request.tools {
        let serialized = serde_json::json!({
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        })
        .to_string();
        bytes = bytes.checked_add(serialized.len() as u64)?;
    }
    bytes.div_ceil(BYTES_PER_TOKEN).checked_add(output_reserve)
}

/// Equality with the known limit fits; one token over does not.
pub(crate) fn fits_context_limit(required_tokens: u64, limit_tokens: u64) -> bool {
    required_tokens <= limit_tokens
}

fn input_item_bytes(item: &ModelInputItem) -> u64 {
    match item {
        ModelInputItem::Message { content, .. } => content.len() as u64,
        ModelInputItem::ProjectContext { rendered } => rendered.len() as u64,
        ModelInputItem::ToolCall {
            name, arguments, ..
        } => name.len() as u64 + arguments.to_string().len() as u64,
        ModelInputItem::ToolOutput {
            name,
            output,
            error,
            ..
        } => {
            name.len() as u64
                + output.as_deref().map_or(0, str::len) as u64
                + error.as_deref().map_or(0, str::len) as u64
        }
        ModelInputItem::Reasoning { content, .. } => content.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use euler_provider::{ReasoningEffort, ToolDefinition};

    fn request(instruction_bytes: usize, rendered_bytes: usize) -> ModelRequest {
        ModelRequest {
            model: "m".to_owned(),
            instructions: "i".repeat(instruction_bytes),
            input: vec![ModelInputItem::ProjectContext {
                rendered: "r".repeat(rendered_bytes),
            }],
            tools: Vec::new(),
            reasoning_effort: ReasoningEffort::Medium,
            max_output_tokens: None,
        }
    }

    #[test]
    fn admission_formula_matches_contract() {
        // ceil((100 + 300) / 4) + 1024 + 16 = 100 + 1024 + 16
        assert_eq!(admission_required_tokens(100, 300, 16), Some(1140));
        // ceil rounds up on a non-multiple of four.
        assert_eq!(admission_required_tokens(1, 0, 0), Some(1 + 1024));
    }

    #[test]
    fn equality_fits_and_one_over_fails() {
        let required = admission_required_tokens(100, 300, 16).expect("no overflow");
        assert!(fits_context_limit(required, required));
        assert!(!fits_context_limit(required, required - 1));
    }

    #[test]
    fn overflow_returns_none() {
        assert_eq!(admission_required_tokens(usize::MAX, 0, u64::MAX), None);
        assert_eq!(admission_required_tokens(0, 0, u64::MAX), None);
    }

    #[test]
    fn request_proxy_counts_instructions_items_and_tools() {
        let mut request = request(8, 12);
        let bare = request_required_tokens(&request, 10).expect("no overflow");
        assert_eq!(bare, (8u64 + 12).div_ceil(4) + 10);
        request.tools.push(ToolDefinition {
            name: "t".to_owned(),
            description: "d".to_owned(),
            parameters: serde_json::json!({}),
        });
        let with_tool = request_required_tokens(&request, 10).expect("no overflow");
        assert!(with_tool > bare);
    }
}
