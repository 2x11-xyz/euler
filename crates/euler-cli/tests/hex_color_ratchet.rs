//! Lint-style ratchet for docs/contracts/ui.md: "Renderers must not hardcode
//! palette hex." The theme module is the single legitimate home for concrete
//! colors; every other UI renderer must go through a theme role/token. This
//! turns that MUST from convention into an enforced boundary, in the same
//! shape as allow_ratchet.rs / architecture.rs.
//!
//! Detection: a hardcoded palette color is a `Color::Rgb(<literal>, ...)`
//! construction (decimal or `0x` hex first argument). Color-space conversion
//! code that matches/destructures `Color::Rgb(red, green, blue)` uses
//! variable arguments and is deliberately NOT flagged — it translates an
//! already-chosen color between spaces, it does not define palette.
//!
//! Baseline is ZERO outside the allowlist: at time of writing every literal
//! `Color::Rgb(..)` lives in ui/theme.rs. If a legitimate new home for
//! palette tokens is ever added, extend `is_allowlisted`, not the baseline.

use std::path::{Path, PathBuf};

/// Files permitted to construct concrete palette colors. Keep this to the
/// theme system only; renderers consume roles/tokens, never raw hex.
fn is_allowlisted(relative: &Path) -> bool {
    relative.ends_with("ui/theme.rs")
}

#[test]
fn renderers_do_not_hardcode_palette_hex() {
    let ui_root = ui_source_root();
    let violations = collect_hardcoded_colors(&ui_root);
    assert!(
        violations.is_empty(),
        "docs/contracts/ui.md forbids hardcoded palette hex outside the theme \
         module; found {} occurrence(s):\n{}\n\nUse a theme role/token instead, \
         or (if this is a new palette home) extend `is_allowlisted`.",
        violations.len(),
        violations
            .iter()
            .map(HexColorMatch::failure_line)
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

#[test]
fn matcher_flags_a_decimal_literal_color() {
    let matches = find_hardcoded_colors(
        Path::new("crates/euler-cli/src/ui/widget.rs"),
        "let c = Color::Rgb(40, 40, 40);\n",
    );
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].line, 1);
}

#[test]
fn matcher_flags_a_hex_literal_color() {
    let matches = find_hardcoded_colors(
        Path::new("crates/euler-cli/src/ui/widget.rs"),
        "background: Color::Rgb(0x26, 0x23, 0x19),\n",
    );
    assert_eq!(matches.len(), 1);
}

#[test]
fn matcher_flags_ratatui_color_alias() {
    // `RatatuiColor::Rgb(..)` ends in the same `Color::Rgb(` substring.
    let matches = find_hardcoded_colors(
        Path::new("crates/euler-cli/src/ui/widget.rs"),
        "RatatuiColor::Rgb(10, 20, 30)\n",
    );
    assert_eq!(matches.len(), 1);
}

#[test]
fn matcher_ignores_variable_argument_conversion() {
    // Color-space conversion / destructuring, not a palette literal.
    let matches = find_hardcoded_colors(
        Path::new("crates/euler-cli/src/ui/terminal/render.rs"),
        "RatatuiColor::Rgb(red, green, blue) => CrosstermColor::Rgb { r: red },\n",
    );
    assert!(matches.is_empty());
}

#[test]
fn matcher_ignores_wildcard_pattern() {
    let matches = find_hardcoded_colors(
        Path::new("crates/euler-cli/src/ui/widget.rs"),
        "assert!(matches!(c, Color::Rgb(_, _, _)));\n",
    );
    assert!(matches.is_empty());
}

#[derive(Debug, Eq, PartialEq)]
struct HexColorMatch {
    file: String,
    line: usize,
    snippet: String,
}

impl HexColorMatch {
    fn failure_line(&self) -> String {
        format!("{}:{} {}", self.file, self.line, self.snippet)
    }
}

fn ui_source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn collect_hardcoded_colors(ui_root: &Path) -> Vec<HexColorMatch> {
    let root = workspace_root();
    let mut matches = Vec::new();
    collect_from_dir(&root, ui_root, &mut matches);
    matches.sort_by(|left, right| left.file.cmp(&right.file).then(left.line.cmp(&right.line)));
    matches
}

fn collect_from_dir(root: &Path, path: &Path, matches: &mut Vec<HexColorMatch>) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).expect("read ui dir") {
            collect_from_dir(root, &entry.expect("ui entry").path(), matches);
        }
        return;
    }
    if !is_production_rust_file(path) {
        return;
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    if is_allowlisted(relative) {
        return;
    }
    let text = std::fs::read_to_string(path).expect("read rust source");
    matches.extend(find_hardcoded_colors(relative, &text));
}

fn is_production_rust_file(path: &Path) -> bool {
    if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
        return false;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    !(name == "tests.rs" || name.ends_with("_test.rs") || name.ends_with("_tests.rs"))
}

fn find_hardcoded_colors(file: &Path, text: &str) -> Vec<HexColorMatch> {
    const NEEDLE: &str = "Color::Rgb(";
    let mut matches = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let mut search_from = 0;
        while let Some(offset) = line[search_from..].find(NEEDLE) {
            let arg_start = search_from + offset + NEEDLE.len();
            let first_arg = line[arg_start..].trim_start();
            if first_arg
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_digit())
            {
                matches.push(HexColorMatch {
                    file: file.to_string_lossy().into_owned(),
                    line: index + 1,
                    snippet: trimmed.to_owned(),
                });
                break;
            }
            search_from = arg_start;
        }
    }
    matches
}
