use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyPatchDocument {
    Add {
        path: String,
        content: String,
    },
    Update {
        path: String,
        chunks: Vec<ApplyPatchChunk>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyPatchChunk {
    pub old: String,
    pub new: String,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ApplyPatchError {
    /// Parse failures reach the model verbatim as the tool error, so every
    /// message must TEACH the expected format (the #63 teaching-denial
    /// lesson): euler serves many providers and cannot assume the patch
    /// dialect is in any model's training. Name what was expected, not
    /// just what was wrong.
    #[error("invalid patch: {0}")]
    Invalid(&'static str),
    #[error("update hunk {hunk} matched {count} times; expected exactly one")]
    UpdateHunkMatchCount { hunk: usize, count: usize },
    #[error("update hunk {hunk} overlaps earlier update hunk {previous_hunk}")]
    UpdateHunkOverlap { hunk: usize, previous_hunk: usize },
}

pub fn parse_single_file_apply_patch(patch: &str) -> Result<ApplyPatchDocument, ApplyPatchError> {
    let mut lines = patch.lines();
    if lines.next() != Some("*** Begin Patch") {
        return Err(ApplyPatchError::Invalid(
            "the first line must be exactly `*** Begin Patch`",
        ));
    }
    let header = lines.next().ok_or(ApplyPatchError::Invalid(
        "line 2 must be `*** Add File: <path>` or `*** Update File: <path>`",
    ))?;
    let parsed = if let Some(path) = header.strip_prefix("*** Add File: ") {
        parse_add_patch(path, &mut lines)?
    } else if let Some(path) = header.strip_prefix("*** Update File: ") {
        parse_update_patch(path, &mut lines)?
    } else if header.starts_with("*** Delete File: ") || header.starts_with("*** Move to: ") {
        return Err(ApplyPatchError::Invalid(
            "delete and rename are not supported — only `*** Add File:` and `*** Update File:`",
        ));
    } else {
        return Err(ApplyPatchError::Invalid(
            "line 2 must be `*** Add File: <path>` or `*** Update File: <path>`",
        ));
    };
    if lines.next().is_some() {
        return Err(ApplyPatchError::Invalid(
            "nothing may follow `*** End Patch` — one file per patch",
        ));
    }
    Ok(parsed)
}

pub fn apply_patch_update_chunks(
    content: &str,
    chunks: &[ApplyPatchChunk],
) -> Result<String, ApplyPatchError> {
    let mut resolved = Vec::with_capacity(chunks.len());
    for (index, chunk) in chunks.iter().enumerate() {
        let hunk = index + 1;
        let (start, end) = single_match_range(content, &chunk.old)
            .map_err(|count| ApplyPatchError::UpdateHunkMatchCount { hunk, count })?;
        if let Some((previous_hunk, _, _, _)) =
            resolved
                .iter()
                .find(|(_, previous_start, previous_end, _)| {
                    start < *previous_end && *previous_start < end
                })
        {
            return Err(ApplyPatchError::UpdateHunkOverlap {
                hunk,
                previous_hunk: *previous_hunk,
            });
        }
        resolved.push((hunk, start, end, chunk.new.as_str()));
    }
    resolved.sort_by_key(|(_, start, _, _)| std::cmp::Reverse(*start));
    let mut updated = content.to_owned();
    for (_, start, end, new) in resolved {
        updated.replace_range(start..end, new);
    }
    Ok(updated)
}

fn parse_add_patch<'a>(
    path: &str,
    lines: &mut impl Iterator<Item = &'a str>,
) -> Result<ApplyPatchDocument, ApplyPatchError> {
    let path = non_empty_patch_path(path)?;
    let mut content = String::new();
    for line in lines {
        if line == "*** End Patch" {
            return Ok(ApplyPatchDocument::Add { path, content });
        }
        let Some(body) = line.strip_prefix('+') else {
            return Err(ApplyPatchError::Invalid(
                "every content line in an Add File must start with `+` (e.g. `+fn main() {`)",
            ));
        };
        content.push_str(body);
        content.push('\n');
    }
    Err(ApplyPatchError::Invalid(
        "the patch must end with a `*** End Patch` line",
    ))
}

fn parse_update_patch<'a>(
    path: &str,
    lines: &mut impl Iterator<Item = &'a str>,
) -> Result<ApplyPatchDocument, ApplyPatchError> {
    let path = non_empty_patch_path(path)?;
    let mut chunks = Vec::new();
    let mut old = String::new();
    let mut new = String::new();
    let mut started = false;
    let mut changed = false;
    for line in lines {
        if line == "*** End Patch" {
            if started {
                finish_update_chunk(&mut chunks, &mut old, &mut new, changed)?;
            }
            if chunks.is_empty() {
                return Err(ApplyPatchError::Invalid(
                    "an Update File needs at least one `@@` hunk with `-`/`+` lines",
                ));
            }
            return Ok(ApplyPatchDocument::Update { path, chunks });
        }
        if line.starts_with("*** ") {
            return Err(ApplyPatchError::Invalid(
                "unexpected `*** ` marker inside the update body — one file per patch",
            ));
        }
        if line.starts_with("@@") {
            if started {
                finish_update_chunk(&mut chunks, &mut old, &mut new, changed)?;
                changed = false;
            }
            started = true;
            continue;
        }
        if !started {
            return Err(ApplyPatchError::Invalid(
                "update content must come after an `@@` hunk marker line",
            ));
        }
        let (prefix, body) = line.split_at_checked(1).ok_or(ApplyPatchError::Invalid(
            "empty hunk line — a blank context line is a single space `' '`",
        ))?;
        match prefix {
            " " => push_patch_line(&mut old, &mut new, body),
            "-" => {
                old.push_str(body);
                old.push('\n');
                changed = true;
            }
            "+" => {
                new.push_str(body);
                new.push('\n');
                changed = true;
            }
            _ => {
                return Err(ApplyPatchError::Invalid(
                    "hunk lines must start with ` ` (context), `-` (remove), or `+` (add)",
                ))
            }
        }
    }
    Err(ApplyPatchError::Invalid(
        "the patch must end with a `*** End Patch` line",
    ))
}

fn finish_update_chunk(
    chunks: &mut Vec<ApplyPatchChunk>,
    old: &mut String,
    new: &mut String,
    changed: bool,
) -> Result<(), ApplyPatchError> {
    if !changed {
        return Err(ApplyPatchError::Invalid(
            "hunk has no `-` or `+` lines — every `@@` hunk must change something",
        ));
    }
    if old.is_empty() {
        return Err(ApplyPatchError::Invalid(
            "hunk has no `-` or context lines to locate it — include the lines being replaced",
        ));
    }
    chunks.push(ApplyPatchChunk {
        old: std::mem::take(old),
        new: std::mem::take(new),
    });
    Ok(())
}

fn non_empty_patch_path(path: &str) -> Result<String, ApplyPatchError> {
    if path.is_empty() {
        Err(ApplyPatchError::Invalid("the file header needs a path, e.g. `*** Add File: src/lib.rs` (relative to the workspace root)"))
    } else {
        Ok(path.to_owned())
    }
}

fn push_patch_line(old: &mut String, new: &mut String, body: &str) {
    old.push_str(body);
    old.push('\n');
    new.push_str(body);
    new.push('\n');
}

fn single_match_range(haystack: &str, needle: &str) -> Result<(usize, usize), usize> {
    let count = overlapping_match_count(haystack, needle);
    if count == 1 {
        let start = haystack.find(needle).expect("one counted match");
        Ok((start, start + needle.len()))
    } else {
        Err(count)
    }
}

fn overlapping_match_count(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return usize::MAX;
    }
    haystack
        .char_indices()
        .filter(|(index, _)| haystack[*index..].starts_with(needle))
        .count()
}

#[cfg(test)]
#[path = "apply_patch_test.rs"]
mod apply_patch_test;
