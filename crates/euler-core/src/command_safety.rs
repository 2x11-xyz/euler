//! Static shell-command safety analysis (issue #78).
//!
//! `run_shell` executes via `sh -c <command>`, so any reasoning about a
//! command line must reason about the *whole* line. This module implements
//! the **two-parser design** Codex converged on
//! (`codex-rs/shell-command/src/bash.rs`), both halves over a real
//! `tree-sitter-bash` syntax tree: one conservative parser that may prove a
//! command safe, and one permissive walk that may only find danger. A
//! single parser cannot be both conservative and complete, and a lexical
//! approximation of shell cannot be either — redirections glued to words,
//! `${...}`, `$'...'`, brace expansion, and here-documents all evade a
//! tokenizer while an AST reports them as distinct nodes.
//!
//! ## 1. Prove-safe grammar (conservative)
//!
//! [`is_statically_safe_command`] returns true only when the parse
//! succeeds without error and the whole tree is made of allowed node kinds
//! ([`ALLOWED_KINDS`]) joined by `&&`, `||`, `;`, `|`, where:
//!
//! - every word is a **literal**: no `* ? [ ] { } ~ $ ` \ ^ #`, and no word
//!   beginning with `=` (zsh equals-expansion). A word whose runtime
//!   spelling the shell may rewrite is not proof of anything;
//! - redirections, substitutions, expansions, subshells, control flow,
//!   here-documents, and variable assignments are separate node kinds and
//!   are rejected by construction;
//! - every simple command's binary is in the read-only set below **and**
//!   its options satisfy that binary's rule. Options are an **allowlist of
//!   exact spellings**, never a denylist: an unknown option, a GNU long
//!   abbreviation (`--recu`), an attached short value (`-oout.bin`), or a
//!   cluster containing an unlisted letter (`-rS`) is simply not provable;
//! - every operand and every option value is confined to the workspace
//!   root and is not a sensitive path — there is no exempt position.
//!
//! The wrapper form `[sh|bash|zsh] -c|-lc <script>` is accepted only by
//! recursively proving `<script>` (depth-capped).
//!
//! Binary names match the command word exactly: `/bin/ls` or `env ls` do
//! not match `ls`.
//!
//! `git` is deliberately absent even for read-only subcommands: `git
//! status`/`diff`/`log` still execute repository-controlled programs
//! through `diff.external`, `core.fsmonitor`, `core.pager`, and clean and
//! smudge filters, so proving it safe requires the sandbox, not a parser
//! (ADR 0021 row P — it returns in Unit 2, inside the sandbox).
//!
//! `cd` is absent for the same class of reason: it moves the directory
//! later commands resolve against while confinement keeps checking the
//! root the command started in.
//!
//! ## 2. Find-danger walk (permissive)
//!
//! [`contains_dangerous_command`] visits *every* command node in the tree —
//! inside control flow, substitutions, expansions, and wrapper scripts —
//! and flags dangerous invocations. It is deliberately over-inclusive and
//! **must never be used to prove safety**; see its doc comment.
//!
//! ## Workspace confinement (security review F1)
//!
//! Read-only is not harmless: `cat ~/.aws/credentials` writes nothing and
//! still exfiltrates. A command is only statically safe when every operand
//! and option value stays inside the workspace root it executes in:
//!
//! - an argument naming an existing path must canonicalize (symlinks
//!   resolved) to a location under the canonicalized root, and neither its
//!   literal spelling nor its resolved form may be sensitive;
//! - a non-existing argument must be relative with no `..` component and
//!   no leading `~` — a relative path without `..` cannot leave the cwd.
//!
//! A rejected command is simply not statically safe: it falls to the
//! ordinary ask path (no new denial surface). False negatives cost a
//! prompt; false positives are the failure mode this module must not have.

use std::path::{Component, Path};
use tree_sitter::{Node, Parser, Tree};

/// Recursion cap for wrapper unwrapping in both parsers (Codex uses the
/// same bound). Exceeding it fails closed: not provable for the prove-safe
/// grammar, dangerous for the danger walk.
const MAX_WRAPPER_DEPTH: usize = 8;

/// Hard bound on nodes visited per tree. Both walks are iterative so a
/// pathological input (`$(` repeated thousands of times) cannot overflow
/// the stack; this bound stops it from burning time either. Exceeding it
/// fails closed the same way the depth cap does.
const MAX_NODE_VISITS: usize = 50_000;

/// Node kinds a provably-safe command line may contain (Codex's list).
const ALLOWED_KINDS: &[&str] = &[
    "program",
    "list",
    "pipeline",
    "command",
    "command_name",
    "word",
    "string",
    "string_content",
    "raw_string",
    "number",
    "concatenation",
];

/// Punctuation tokens a provably-safe command line may contain. Any other
/// token spelled with `&`, `;`, or `|` rejects the line.
const ALLOWED_PUNCT_TOKENS: &[&str] = &["&&", "||", ";", "|", "\"", "'"];

fn parse_script(script: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    parser.parse(script, None)
}

/// One simple command of a parsed line (a pipeline or list element), with
/// every word resolved to the literal text the binary will receive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSegment {
    /// Never empty: a command node without a name never becomes a segment.
    words: Vec<String>,
}

impl CommandSegment {
    /// The command name of this segment.
    pub fn first_token(&self) -> &str {
        &self.words[0]
    }

    /// Whether this segment is a known read-only invocation whose operands
    /// and option values are confined to `workspace_root`.
    pub fn is_statically_safe(&self, workspace_root: &Path) -> bool {
        self.is_statically_safe_at_depth(workspace_root, 0)
    }

    fn is_statically_safe_at_depth(&self, workspace_root: &Path, depth: usize) -> bool {
        if let Some(script) = self.wrapper_script() {
            return depth < MAX_WRAPPER_DEPTH
                && is_statically_safe_at_depth(script, workspace_root, depth + 1);
        }
        let Some(checked) = self.checked_words() else {
            return false;
        };
        let Ok(root) = workspace_root.canonicalize() else {
            // Unresolvable root: nothing can be proven confined.
            return false;
        };
        checked.iter().all(|word| arg_confined(word, &root))
    }

    /// The script of a wrapper invocation `[sh|bash|zsh] -c|-lc <script>`,
    /// which is provable exactly when the script is (checked recursively).
    fn wrapper_script(&self) -> Option<&str> {
        let [shell, flag, script] = self.words.as_slice() else {
            return None;
        };
        let is_shell = matches!(shell.as_str(), "sh" | "bash" | "zsh");
        let is_command_flag = matches!(flag.as_str(), "-c" | "-lc");
        (is_shell && is_command_flag).then_some(script.as_str())
    }

    /// Words that must pass confinement (operands and option values), or
    /// `None` when the binary is not read-only or an option is not on its
    /// allowlist.
    fn checked_words(&self) -> Option<Vec<String>> {
        let args = &self.words[1..];
        match self.words[0].as_str() {
            "find" => find_checked_words(args),
            "sed" => sed_checked_words(args),
            name => scan_args(binary_rule(name)?, args),
        }
    }
}

/// Decompose a command line into simple commands. Returns `None` when the
/// line is not statically analyzable (see the module docs).
///
/// This is the conservative parser: everything it accepts is a sequence of
/// literal-word commands joined by `&&`, `||`, `;`, `|`.
pub fn parse_plain_segments(command: &str) -> Option<Vec<CommandSegment>> {
    let tree = parse_script(command)?;
    let root = tree.root_node();
    if root.has_error() || root.is_missing() {
        return None;
    }
    let mut command_nodes = Vec::new();
    let mut stack = vec![root];
    let mut visits = 0usize;
    while let Some(node) = stack.pop() {
        visits += 1;
        if visits > MAX_NODE_VISITS {
            return None;
        }
        let kind = node.kind();
        if node.is_named() {
            if !ALLOWED_KINDS.contains(&kind) {
                return None;
            }
            if matches!(kind, "word" | "number") && !is_literal_word_or_number(node, command) {
                return None;
            }
            if kind == "command" {
                command_nodes.push(node);
            }
        } else if kind.chars().any(|c| "&;|".contains(c)) && !ALLOWED_PUNCT_TOKENS.contains(&kind) {
            return None;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    command_nodes.sort_by_key(tree_sitter::Node::start_byte);
    let mut segments = Vec::with_capacity(command_nodes.len());
    for node in command_nodes {
        let words = plain_command_words(node, command)?;
        if words.is_empty() {
            return None;
        }
        segments.push(CommandSegment { words });
    }
    (!segments.is_empty()).then_some(segments)
}

/// Every word of one command node, or `None` if the node carries anything
/// but literal words (a redirection, an assignment, an expansion).
fn plain_command_words(node: Node<'_>, src: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => words.push(literal_word(child.named_child(0)?, src)?),
            "word" | "number" | "string" | "raw_string" | "concatenation" => {
                words.push(literal_word(child, src)?);
            }
            _ => return None,
        }
    }
    Some(words)
}

/// The literal text of one word node, or `None` when the shell may rewrite
/// it before the binary sees it (Codex `parse_literal_shell_word`).
fn literal_word(node: Node<'_>, src: &str) -> Option<String> {
    match node.kind() {
        "word" | "number" if is_literal_word_or_number(node, src) => {
            Some(node.utf8_text(src.as_bytes()).ok()?.to_owned())
        }
        "string" => parse_double_quoted_string(node, src),
        "raw_string" => parse_raw_string(node, src),
        "concatenation" => {
            let mut joined = String::new();
            let mut cursor = node.walk();
            for part in node.named_children(&mut cursor) {
                joined.push_str(&literal_word(part, src)?);
            }
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

fn is_literal_word_or_number(node: Node<'_>, src: &str) -> bool {
    if !matches!(node.kind(), "word" | "number") {
        return false;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next().is_none()
        && node.utf8_text(src.as_bytes()).is_ok_and(|word| {
            // A tree-sitter word can still undergo expansion or escape
            // removal; its source spelling is not proof of runtime argv.
            // `=` leads zsh equals-expansion, `^`/`#` are zsh glob
            // operators, and the rest are POSIX expansion or quoting.
            !word.starts_with('=')
                && !word.contains(['{', '}', '*', '?', '[', ']', '\\', '~', '^', '#', '$', '`'])
        })
}

fn parse_double_quoted_string(node: Node<'_>, src: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    for part in node.named_children(&mut cursor) {
        if part.kind() != "string_content" {
            return None;
        }
    }
    let raw = node.utf8_text(src.as_bytes()).ok()?;
    let stripped = raw
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))?;
    // Double quotes suppress globbing but not escape removal; accept only
    // contents whose source spelling is already literal.
    if stripped
        .as_bytes()
        .windows(2)
        .any(|pair| pair[0] == b'\\' && matches!(pair[1], b'$' | b'`' | b'"' | b'\\' | b'\n'))
    {
        return None;
    }
    Some(stripped.to_owned())
}

fn parse_raw_string(node: Node<'_>, src: &str) -> Option<String> {
    if node.kind() != "raw_string" {
        return None;
    }
    let raw = node.utf8_text(src.as_bytes()).ok()?;
    raw.strip_prefix('\'')
        .and_then(|text| text.strip_suffix('\''))
        .map(str::to_owned)
}

/// Option grammar of one provably read-only binary.
///
/// Flags are an **allowlist of exact spellings**. Anything not listed —
/// an unknown option, a GNU long abbreviation, an attached short value, a
/// cluster containing an unlisted letter — makes the command unprovable.
/// This polarity is the point: a denylist has to enumerate every spelling
/// of every harmful option on every platform, and misses one.
struct BinaryRule {
    /// Long options taking no value (`--all`).
    long_bool: &'static [&'static str],
    /// Long options taking a value, as `--name value` or `--name=value`.
    long_value: &'static [&'static str],
    /// Long options usable bare or as `--name=value` (`--color`).
    long_optional: &'static [&'static str],
    /// Short flags taking no value; these may be bundled (`-la`).
    short_bool: &'static str,
    /// Short flags taking a value, accepted ONLY as a lone `-n value`
    /// pair: an attached value (`-n5`) or a bundle (`-ln 5`) is not
    /// provable, because attached-value grammars differ per binary.
    short_value: &'static str,
    /// Maximum operand count; `None` for unlimited.
    max_operands: Option<usize>,
}

const fn rule(
    long_bool: &'static [&'static str],
    long_value: &'static [&'static str],
    long_optional: &'static [&'static str],
    short_bool: &'static str,
    short_value: &'static str,
    max_operands: Option<usize>,
) -> BinaryRule {
    BinaryRule {
        long_bool,
        long_value,
        long_optional,
        short_bool,
        short_value,
        max_operands,
    }
}

/// The read-only set. A binary absent from this table is never provable —
/// `sort` (`-o` writes), `tee`, `git` (repo-controlled helpers) and every
/// interpreter included.
const READ_ONLY_BINARIES: &[(&str, BinaryRule)] = &[
    ("true", rule(&[], &[], &[], "LP", "", Some(0))),
    ("false", rule(&[], &[], &[], "LP", "", Some(0))),
    ("pwd", rule(&[], &[], &[], "LP", "", Some(0))),
    ("whoami", rule(&[], &[], &[], "LP", "", Some(0))),
    (
        "id",
        rule(
            &["--user", "--group", "--name", "--real", "--groups"],
            &[],
            &[],
            "ugnrG",
            "",
            None,
        ),
    ),
    (
        "uname",
        rule(
            &["--all", "--kernel-name"],
            &[],
            &[],
            "asrmnpvo",
            "",
            Some(0),
        ),
    ),
    ("echo", rule(&[], &[], &[], "neE", "", None)),
    ("expr", rule(&[], &[], &[], "", "", None)),
    ("seq", rule(&[], &[], &[], "", "", None)),
    ("which", rule(&[], &[], &[], "", "", None)),
    (
        "cat",
        rule(
            &[
                "--number",
                "--number-nonblank",
                "--show-ends",
                "--squeeze-blank",
            ],
            &[],
            &[],
            "nbsETvet",
            "",
            None,
        ),
    ),
    (
        "head",
        rule(
            &["--quiet", "--verbose"],
            &["--lines", "--bytes"],
            &[],
            "qv",
            "nc",
            None,
        ),
    ),
    // `tail -f`/`-F`/`--follow`/`--retry` never terminates and keeps
    // reading a path that may be replaced after the check, so they are
    // absent from the allowlist.
    (
        "tail",
        rule(
            &["--quiet", "--verbose"],
            &["--lines", "--bytes"],
            &[],
            "qv",
            "nc",
            None,
        ),
    ),
    (
        "wc",
        rule(
            &[
                "--lines",
                "--words",
                "--bytes",
                "--chars",
                "--max-line-length",
            ],
            &[],
            &[],
            "lwcmL",
            "",
            None,
        ),
    ),
    // No `-R`/`--recursive` and no `-L`/`--dereference`: both report paths
    // the confinement check never saw (audit F02).
    (
        "ls",
        rule(
            &[
                "--all",
                "--almost-all",
                "--human-readable",
                "--classify",
                "--reverse",
                "--size",
                "--directory",
                "--inode",
            ],
            &[],
            &["--color"],
            "laAhtSr1dFpi",
            "",
            None,
        ),
    ),
    ("nl", rule(&[], &[], &[], "", "bnwsv", None)),
    (
        "paste",
        rule(&["--serial"], &["--delimiters"], &[], "s", "d", None),
    ),
    ("rev", rule(&[], &[], &[], "", "", None)),
    (
        "cut",
        rule(
            &["--only-delimited", "--complement"],
            &["--delimiter", "--fields", "--characters", "--bytes"],
            &[],
            "s",
            "dfcb",
            None,
        ),
    ),
    (
        "tr",
        rule(
            &["--delete", "--squeeze-repeats", "--complement"],
            &[],
            &[],
            "dsc",
            "",
            Some(2),
        ),
    ),
    (
        "stat",
        rule(&["--terse"], &["--format", "--printf"], &[], "t", "f", None),
    ),
    // `uniq in out` truncates and writes `out` (audit F01): at most one
    // operand, and `-` occupies an operand position.
    (
        "uniq",
        rule(
            &["--count", "--repeated", "--unique", "--ignore-case"],
            &[],
            &[],
            "cdui",
            "",
            Some(1),
        ),
    ),
    // No `-R`/`--dereference-recursive`, no `-e`/`-f` (they move the
    // pattern operand), no macOS `-S`/`-O`, no `-Z`/`-z`.
    (
        "grep",
        rule(
            &[
                "--line-number",
                "--ignore-case",
                "--count",
                "--word-regexp",
                "--line-regexp",
                "--fixed-strings",
                "--extended-regexp",
                "--invert-match",
                "--recursive",
                "--files-with-matches",
                "--files-without-match",
                "--no-messages",
                "--only-matching",
                "--with-filename",
                "--no-filename",
                "--byte-offset",
                "--quiet",
            ],
            &["--include", "--exclude", "--max-count"],
            &["--color", "--colour"],
            "nilLcwxFEvrhHobsqam",
            "",
            None,
        ),
    ),
    // No `-L`/`--follow`, no `--pre`/`--hostname-bin`, no `-z`.
    (
        "rg",
        rule(
            &[
                "--line-number",
                "--ignore-case",
                "--files-with-matches",
                "--count",
                "--count-matches",
                "--word-regexp",
                "--line-regexp",
                "--fixed-strings",
                "--invert-match",
                "--no-ignore",
                "--hidden",
                "--no-heading",
                "--with-filename",
                "--no-filename",
                "--only-matching",
                "--files",
                "--json",
                "--quiet",
                "--smart-case",
                "--case-sensitive",
            ],
            &[
                "--glob",
                "--type",
                "--max-count",
                "--max-depth",
                "--context",
            ],
            &["--color"],
            "nilcwxFvHhoqSsu",
            "",
            None,
        ),
    ),
    // No `-o`/`--output`, and no attached values (`-Do`, `-i.env`).
    (
        "base64",
        rule(
            &["--decode", "--ignore-garbage"],
            &[],
            &[],
            "d",
            "",
            Some(1),
        ),
    ),
];

fn binary_rule(name: &str) -> Option<&'static BinaryRule> {
    READ_ONLY_BINARIES
        .iter()
        .find(|(binary, _)| *binary == name)
        .map(|(_, rule)| rule)
}

/// Walk one command's arguments against its rule, returning the words that
/// must pass confinement (operands and option values), or `None` when any
/// option is not on the allowlist.
fn scan_args(rule: &BinaryRule, args: &[String]) -> Option<Vec<String>> {
    let mut checked = Vec::new();
    let mut operands = 0usize;
    let mut operands_only = false;
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();
        index += 1;
        if operands_only {
            operands += 1;
            checked.push(arg.to_owned());
            continue;
        }
        if arg == "--" {
            operands_only = true;
            continue;
        }
        if let Some(name) = arg.strip_prefix("--") {
            let (name, value) = match name.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (name, None),
            };
            let long = format!("--{name}");
            let long = long.as_str();
            if rule.long_bool.contains(&long) {
                if value.is_some() {
                    return None;
                }
            } else if rule.long_optional.contains(&long) {
                if let Some(value) = value {
                    checked.push(value.to_owned());
                }
            } else if rule.long_value.contains(&long) {
                match value {
                    Some(value) => checked.push(value.to_owned()),
                    None => {
                        checked.push(args.get(index)?.clone());
                        index += 1;
                    }
                }
            } else {
                // Unknown option, or a GNU abbreviation of a known one.
                return None;
            }
            continue;
        }
        if let Some(cluster) = arg.strip_prefix('-') {
            if cluster.is_empty() {
                // `-` is the stdin operand and occupies an operand slot.
                operands += 1;
                continue;
            }
            let mut chars = cluster.chars();
            let first = chars.next()?;
            if chars.next().is_none() && rule.short_value.contains(first) {
                checked.push(args.get(index)?.clone());
                index += 1;
                continue;
            }
            if cluster.chars().all(|c| rule.short_bool.contains(c)) {
                continue;
            }
            // Attached value, unlisted letter, or a value flag in a bundle.
            return None;
        }
        operands += 1;
        checked.push(arg.to_owned());
    }
    if rule.max_operands.is_some_and(|max| operands > max) {
        return None;
    }
    Some(checked)
}

/// `find` is a predicate language, not an option grammar: only the
/// enumerated read-only predicates are provable, so `-exec`, `-delete`,
/// `-fprintf`, `-L`, and `-follow` are all rejected by omission.
fn find_checked_words(args: &[String]) -> Option<Vec<String>> {
    const BOOL_PREDICATES: &[&str] = &[
        "-print", "-print0", "-empty", "-not", "-o", "-a", "-or", "-and", "-true", "-false",
        "-type", "-depth",
    ];
    const VALUE_PREDICATES: &[&str] = &[
        "-name",
        "-iname",
        "-path",
        "-ipath",
        "-type",
        "-maxdepth",
        "-mindepth",
        "-size",
        "-newer",
        "-mtime",
        "-mmin",
        "-user",
        "-group",
        "-perm",
        "-regex",
    ];
    let mut checked = Vec::new();
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();
        index += 1;
        if VALUE_PREDICATES.contains(&arg) {
            // Predicate values are patterns or numbers, not paths, but
            // checking them costs nothing and never lets one through.
            checked.push(args.get(index)?.clone());
            index += 1;
            continue;
        }
        if BOOL_PREDICATES.contains(&arg) {
            continue;
        }
        if arg.starts_with('-') {
            return None;
        }
        checked.push(arg.to_owned());
    }
    Some(checked)
}

/// Only the print-range form `sed -n Np [file]` / `sed -n M,Np [file]` is
/// provable: no scripts, no in-place editing, no write commands, and the
/// optional third word must be an operand, never another option.
fn sed_checked_words(args: &[String]) -> Option<Vec<String>> {
    if !matches!(args.len(), 2 | 3) || args[0] != "-n" || !is_sed_print_range(&args[1]) {
        return None;
    }
    match args.get(2) {
        None => Some(Vec::new()),
        Some(file) if !file.starts_with('-') => Some(vec![file.clone()]),
        Some(_) => None,
    }
}

/// Matches `^(\d+,)?\d+p$`.
fn is_sed_print_range(arg: &str) -> bool {
    let Some(core) = arg.strip_suffix('p') else {
        return false;
    };
    let mut parts = core.split(',');
    let is_number = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(n), None, _) => is_number(n),
        (Some(m), Some(n), None) => is_number(m) && is_number(n),
        _ => false,
    }
}

/// One potential path argument, checked against the canonicalized root.
fn arg_confined(arg: &str, canonical_root: &Path) -> bool {
    if arg.is_empty() || arg == "-" {
        return true;
    }
    if arg.starts_with('~') || arg.contains(['$', '`']) {
        return false;
    }
    let path = Path::new(arg);
    if sensitive_basename(path) {
        return false;
    }
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return false;
    }
    match canonical_root.join(path).canonicalize() {
        // Existing path: symlinks resolved, must land under the root, and
        // the RESOLVED name must not be sensitive either — an
        // innocently-named in-workspace symlink to `.git/config` or `.env`
        // must not read through (same rule as the fs-tool gate).
        Ok(resolved) => resolved.starts_with(canonical_root) && !sensitive_basename(&resolved),
        Err(_) => true,
    }
}

/// Basenames of files whose contents are categorically sensitive.
const SENSITIVE_NAMES: &[&str] = &[
    // Git metadata an interpreter honors: config selects hooks, filters,
    // and pagers, so writing one turns a later `git status` into arbitrary
    // execution (audit F34).
    ".gitmodules",
    ".gitattributes",
    ".gitconfig",
    // Package-manager and toolchain configuration honored on the next
    // build or install.
    ".npmrc",
    ".netrc",
    // Shell startup files, honored by the next interactive or login shell.
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_logout",
    ".profile",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".zlogout",
];

/// Path components whose entire subtree is sensitive.
const SENSITIVE_COMPONENTS: &[&str] = &[".git"];

/// Whether `path` names something categorically sensitive, denied even
/// inside the workspace (security review F1, audit F34).
///
/// Despite the name this inspects the whole path, not only the final
/// component: `.git` is sensitive as a **component**, so everything under
/// `.git/` (and the worktree pointer file itself) is covered, and
/// `.cargo/config.toml` is sensitive only under `.cargo`.
///
/// The single sensitive-name list (one list, not two): statically-safe shell
/// analysis rejects these path arguments outright, and the fs-tool permission
/// braid escalates a blanket `session-allow` to an explicit ask when a tool
/// path names one (deep review P1-b — `read_file .env` must not run
/// unprompted while `cat .env` asks).
pub fn sensitive_basename(path: &Path) -> bool {
    let components: Vec<String> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => name.to_str().map(str::to_ascii_lowercase),
            _ => None,
        })
        .collect();
    if components
        .iter()
        .any(|component| SENSITIVE_COMPONENTS.contains(&component.as_str()))
    {
        return true;
    }
    let Some(name) = components.last() else {
        return false;
    };
    if SENSITIVE_NAMES.contains(&name.as_str()) {
        return true;
    }
    // `.cargo/config.toml` (and its extensionless form) selects the linker
    // and build runner for the next `cargo` invocation.
    if matches!(name.as_str(), "config.toml" | "config")
        && components.len() >= 2
        && components[components.len() - 2] == ".cargo"
    {
        return true;
    }
    name.starts_with(".env")
        || name.contains("secret")
        || name.contains("credential")
        || name == "id_rsa"
        || name == "id_ed25519"
        || name.ends_with(".pem")
        || name.ends_with(".key")
}

/// Whether `command` is provably a sequence of read-only invocations
/// confined to `workspace_root`.
///
/// A command the permissive danger walk flags is never provable, even if
/// the grammar would otherwise accept it (defense in depth).
pub fn is_statically_safe_command(command: &str, workspace_root: &Path) -> bool {
    !contains_dangerous_command(command) && is_statically_safe_at_depth(command, workspace_root, 0)
}

fn is_statically_safe_at_depth(command: &str, workspace_root: &Path, depth: usize) -> bool {
    parse_plain_segments(command).is_some_and(|segments| {
        segments
            .iter()
            .all(|segment| segment.is_statically_safe_at_depth(workspace_root, depth))
    })
}

// ── Find-danger walk (permissive) ────────────────────────────────────────

/// Whether any command anywhere in `command` is dangerous.
///
/// # This must never be used to prove that a command is safe.
///
/// This is the permissive half of the two-parser design (Codex
/// `parse_shell_lc_literal_commands` + `dangerous_command_match`). Unlike
/// [`is_statically_safe_command`] it accepts arbitrary shell syntax and
/// visits every command node in the tree: inside control flow, command
/// substitutions, process substitutions, expansions, and the scripts
/// carried by `sh -c`, `eval`, `env -S`, and `trap`. It deliberately
/// over-flags — a false positive costs one prompt — and fails closed on a
/// parse error, on a dynamic command name, on a dynamic argument of a
/// dangerous or wrapper command, and past [`MAX_WRAPPER_DEPTH`].
///
/// The dangerous set is an intentionally extensible table: commands that
/// destroy data with no undo. `run_shell` runs with stdin closed, so `rm
/// -r` never gets its interactive confirmation and is as destructive as
/// `rm -rf`; both are in.
pub fn contains_dangerous_command(command: &str) -> bool {
    script_is_dangerous(command, 0)
}

fn script_is_dangerous(script: &str, depth: usize) -> bool {
    if depth > MAX_WRAPPER_DEPTH {
        return true;
    }
    let Some(tree) = parse_script(script) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() || root.is_missing() {
        // An unparseable script is unreadable, so it is not readable as
        // harmless either.
        return true;
    }
    let mut stack = vec![root];
    let mut visits = 0usize;
    while let Some(node) = stack.pop() {
        visits += 1;
        if visits > MAX_NODE_VISITS {
            return true;
        }
        if node.kind() == "command" && command_node_is_dangerous(node, script, depth) {
            return true;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

fn command_node_is_dangerous(node: Node<'_>, src: &str, depth: usize) -> bool {
    let words = literal_or_dynamic_words(node, src);
    words_are_dangerous(&words, depth)
}

/// Every word of a command node, `None` where the word is dynamic. Unlike
/// the prove-safe extractor this keeps positions, so `rm -rf$X dir` still
/// reports three words with the second unreadable.
fn literal_or_dynamic_words(node: Node<'_>, src: &str) -> Vec<Option<String>> {
    let mut words = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => match child.named_child(0) {
                Some(name) => words.push(literal_word(name, src)),
                None => words.push(None),
            },
            _ => {
                if !words.is_empty() {
                    words.push(literal_word(child, src));
                }
            }
        }
    }
    words.extend(redirected_arguments(node, src));
    words
}

/// `tree-sitter-bash` hangs the words that FOLLOW a redirection off the
/// redirect node rather than the command (`rm 2>&1 -rf dir` parses as a
/// `redirected_statement` whose `file_redirect` carries `1`, `-rf`, and
/// `dir`). Reading only the command node would therefore see a bare `rm`.
/// Over-collecting here is harmless: a redirect target is just one more
/// operand as far as the danger table is concerned.
fn redirected_arguments(node: Node<'_>, src: &str) -> Vec<Option<String>> {
    let Some(parent) = node.parent() else {
        return Vec::new();
    };
    if parent.kind() != "redirected_statement" {
        return Vec::new();
    }
    let mut extra = Vec::new();
    let mut cursor = parent.walk();
    for redirect in parent.named_children(&mut cursor) {
        if !redirect.kind().ends_with("redirect") {
            continue;
        }
        let mut inner = redirect.walk();
        for child in redirect.named_children(&mut inner) {
            match child.kind() {
                "word" | "number" | "string" | "raw_string" | "concatenation" => {
                    extra.push(literal_word(child, src));
                }
                "simple_expansion"
                | "expansion"
                | "command_substitution"
                | "process_substitution" => extra.push(None),
                _ => {}
            }
        }
    }
    extra
}

/// Commands that destroy data with no undo. Extend this table rather than
/// reasoning about which flags are "really" harmful.
fn destructive_rule(name: &str, args: &[&str]) -> bool {
    match name {
        // `run_shell` closes stdin, so `-r` never prompts interactively.
        "rm" => args
            .iter()
            .take_while(|arg| **arg != "--")
            .any(|arg| is_short_cluster_with(arg, "rRf") || is_long_abbreviation(arg, RM_LONGS)),
        "shred" | "wipefs" | "truncate" => true,
        "dd" => args.iter().any(|arg| arg.starts_with("of=")),
        "find" => args.iter().any(|arg| {
            matches!(
                *arg,
                "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fprintf"
            )
        }),
        "git" => {
            args.first() == Some(&"clean")
                && args.iter().any(|arg| {
                    is_short_cluster_with(arg, "f") || is_long_abbreviation(arg, &["--force"])
                })
        }
        _ => name.starts_with("mkfs"),
    }
}

const RM_LONGS: &[&str] = &["--force", "--recursive", "--dir"];

fn is_short_cluster_with(arg: &str, letters: &str) -> bool {
    arg.strip_prefix('-')
        .is_some_and(|rest| !rest.starts_with('-') && rest.chars().any(|c| letters.contains(c)))
}

/// GNU long options may be abbreviated to any unambiguous prefix, so
/// `rm --forc` forces just as well as `rm --force`.
fn is_long_abbreviation(arg: &str, longs: &[&str]) -> bool {
    let candidate = match arg.split_once('=') {
        Some((name, _)) => name,
        None => arg,
    };
    candidate.len() > 2 && longs.iter().any(|long| long.starts_with(candidate))
}

/// Wrappers whose operands are another command, or shell source.
enum Wrapper {
    /// Some operand starts another command; try each, since which one
    /// depends on option grammars this walk does not model.
    Command,
    /// Every operand is shell source (`eval`, `trap`).
    Script,
    /// Shell: the operand after a `-c`-bearing flag is shell source.
    Shell,
    /// `env`: assignments and options, then a command.
    Env,
}

fn wrapper_kind(name: &str) -> Option<Wrapper> {
    Some(match name {
        "sudo" | "doas" | "nohup" | "time" | "timeout" | "nice" | "ionice" | "chrt" | "stdbuf"
        | "setsid" | "flock" | "xargs" | "command" | "builtin" | "exec" => Wrapper::Command,
        "eval" | "trap" => Wrapper::Script,
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "su" => Wrapper::Shell,
        "env" => Wrapper::Env,
        _ => return None,
    })
}

fn words_are_dangerous(words: &[Option<String>], depth: usize) -> bool {
    if depth > MAX_WRAPPER_DEPTH {
        return true;
    }
    let Some(first) = words.first() else {
        return false;
    };
    let Some(name) = first.as_deref() else {
        // A command whose own name is dynamic (`r$M -f file`) is
        // unreadable, so it is not readable as harmless.
        return true;
    };
    let name = match name.rsplit_once('/') {
        Some((_, base)) => base,
        None => name,
    };
    let dynamic_args = words[1..].iter().any(Option::is_none);
    let literal: Vec<&str> = words[1..].iter().filter_map(Option::as_deref).collect();
    if is_tabled(name) {
        // `rm $FLAGS file` may be `rm -rf file`.
        return dynamic_args || destructive_rule(name, &literal);
    }
    let Some(kind) = wrapper_kind(name) else {
        return false;
    };
    if dynamic_args {
        return true;
    }
    match kind {
        Wrapper::Command => (0..literal.len()).any(|start| {
            let suffix: Vec<Option<String>> = literal[start..]
                .iter()
                .map(|word| Some((*word).to_owned()))
                .collect();
            words_are_dangerous(&suffix, depth + 1)
        }),
        Wrapper::Script => literal
            .iter()
            .any(|script| script_is_dangerous(script, depth + 1)),
        Wrapper::Shell => shell_scripts(&literal)
            .iter()
            .any(|script| script_is_dangerous(script, depth + 1)),
        Wrapper::Env => env_is_dangerous(&literal, depth),
    }
}

/// Whether `name` has an entry in the destructive table (including the
/// `mkfs*` family).
fn is_tabled(name: &str) -> bool {
    matches!(
        name,
        "rm" | "shred" | "wipefs" | "truncate" | "dd" | "find" | "git"
    ) || name.starts_with("mkfs")
}

/// Operands a shell treats as command source: everything after the flag
/// carrying `c` (`-c`, `-lc`, `-cx`, and `su`'s `-c`), since which operand
/// is the script depends on the remaining option order.
fn shell_scripts<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let Some(flag) = args
        .iter()
        .position(|arg| is_short_cluster_with(arg, "c") || *arg == "--command")
    else {
        return Vec::new();
    };
    args[flag + 1..]
        .iter()
        .filter(|arg| **arg != "--")
        .copied()
        .collect()
}

/// `env` skips assignments and its own options before the wrapped command.
/// An unrecognized option fails closed, and `-S`/`--split-string` carries
/// shell source.
fn env_is_dangerous(args: &[&str], depth: usize) -> bool {
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if arg == "--" {
            break;
        }
        if is_assignment(arg) {
            continue;
        }
        if !arg.starts_with('-') {
            index -= 1;
            break;
        }
        let (name, attached) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (arg, None),
        };
        match name {
            "-i" | "--ignore-environment" | "-0" | "--null" | "-v" | "--debug" => {}
            "-u" | "--unset" | "-C" | "--chdir" => {
                if attached.is_none() {
                    index += 1;
                }
            }
            "-S" | "--split-string" => {
                let script = match attached {
                    Some(value) => Some(value),
                    None => {
                        let next = args.get(index).copied();
                        index += 1;
                        next
                    }
                };
                if script.is_none_or(|script| script_is_dangerous(script, depth + 1)) {
                    return true;
                }
            }
            _ => return true,
        }
    }
    let suffix: Vec<Option<String>> = args[index.min(args.len())..]
        .iter()
        .map(|word| Some((*word).to_owned()))
        .collect();
    words_are_dangerous(&suffix, depth + 1)
}

/// `NAME=value` prefixes a command with an environment assignment.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(command: &str) -> Vec<CommandSegment> {
        parse_plain_segments(command).expect("command should parse")
    }

    /// Safety against an empty temp workspace: existence-dependent checks
    /// see no files, so args are judged by the textual confinement rules.
    fn safe(command: &str) -> bool {
        let temp = tempfile::tempdir().expect("temp workspace");
        is_statically_safe_command(command, temp.path())
    }

    #[test]
    fn parses_simple_command_into_one_segment() {
        let parsed = segments("ls -la src");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].first_token(), "ls");
        assert_eq!(parsed[0].words.len(), 3);
    }

    #[test]
    fn splits_segments_on_operators_and_newlines() {
        for (command, expected_tokens) in [
            ("ls && pwd", vec!["ls", "pwd"]),
            ("ls || pwd", vec!["ls", "pwd"]),
            ("ls ; pwd", vec!["ls", "pwd"]),
            ("ls | wc -l", vec!["ls", "wc"]),
            ("ls\npwd", vec!["ls", "pwd"]),
            ("ls\n\npwd\n", vec!["ls", "pwd"]),
            ("ls &&\npwd", vec!["ls", "pwd"]),
            ("find . -name x | head -5 | wc", vec!["find", "head", "wc"]),
        ] {
            let tokens: Vec<String> = segments(command)
                .iter()
                .map(|segment| segment.first_token().to_owned())
                .collect();
            assert_eq!(tokens, expected_tokens, "command: {command}");
        }
    }

    #[test]
    fn quoted_metacharacters_are_literal_text() {
        let parsed = segments("grep 'a && b' file");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].words[1], "a && b");

        let parsed = segments(r#"echo "semi;colon" 'pipe|here'"#);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].words[1], "semi;colon");
        assert_eq!(parsed[0].words[2], "pipe|here");
    }

    #[test]
    fn adjacent_quotes_join_into_one_word() {
        let parsed = segments(r#"grep "Cargo"'.toml' file"#);
        assert_eq!(parsed[0].words[1], "Cargo.toml");
    }

    #[test]
    fn backslash_words_are_never_provable() {
        // Escape removal happens after the analysis, so a word carrying a
        // backslash is not proof of the runtime argv (Codex's rule).
        assert!(parse_plain_segments(r"echo a\;b").is_none());
        assert!(parse_plain_segments(r"cat $'-rf'").is_none());
        // A line continuation is whitespace, not a word.
        let parsed = segments("ls \\\n-la");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].words[1], "-la");
    }

    #[test]
    fn rejects_redirects_substitution_subshells_background() {
        for command in [
            "ls > out.txt",
            "ls >> out.txt",
            "sort < input",
            "cat <<EOF",
            "echo $(evil)",
            "echo `evil`",
            "echo $HOME",
            "echo \"$HOME\"",
            "echo \"`evil`\"",
            "(ls)",
            "ls || (pwd && echo hi)",
            "{ ls; }",
            "echo {a,b}",
            "sleep 5 &",
            "ls & pwd",
            "ls # comment",
            "ls 'unterminated",
            "ls \"unterminated",
            "ls \\",
            // Here-documents and here-strings are separate node kinds.
            "cat <<EOF\nbody\nEOF",
            "cat <<<payload",
            "ls > out.txt 2>&1",
            "cat ${HOME}/x",
            "echo {a,b}",
            "ls; if true; then ls; fi",
        ] {
            assert!(
                parse_plain_segments(command).is_none(),
                "expected unparseable: {command}"
            );
        }
    }

    #[test]
    fn rejects_empty_and_dangling_segments() {
        for command in [
            "",
            "   ",
            "&& ls",
            "| ls",
            "; ls",
            "ls &&",
            "ls |",
            "ls ;;",
            "ls ; ; pwd",
        ] {
            assert!(
                parse_plain_segments(command).is_none(),
                "expected unparseable: {command}"
            );
        }
        // Trailing `;` and blank lines are ordinary shell.
        assert_eq!(segments("ls;").len(), 1);
        assert_eq!(segments("ls\n").len(), 1);
    }

    #[test]
    fn zsh_glob_operators_are_never_literal() {
        // `#` and `^` are glob operators under zsh's extendedglob, so a
        // word carrying one is never proof of the runtime argv even though
        // `sh` treats it as text.
        assert!(parse_plain_segments("cat file#1").is_none());
        assert!(parse_plain_segments("cat file^1").is_none());
    }

    #[test]
    fn read_only_binaries_are_safe_with_workspace_confined_args() {
        for command in [
            "ls",
            "ls -la --color=always",
            "cat Cargo.toml",
            // Audit F02: `-r` only dereferences command-line operands
            // (which confinement already checked); `-R` follows symlinks
            // out of the workspace and is rejected below.
            "grep -r Cargo.toml -n",
            "head -n 50 src/lib.rs",
            "tail -n 20 src/lib.rs",
            "uniq input.txt",
            "wc -l file",
            "which cargo",
            "nl Cargo.toml",
            "nl -n rz Cargo.toml",
            "echo hello world",
            "true",
        ] {
            assert!(safe(command), "expected safe: {command}");
        }
    }

    #[test]
    fn path_arguments_outside_the_workspace_are_unsafe() {
        // Security review F1: read-only binaries exfiltrate; every path
        // argument must stay inside the execution workspace.
        for command in [
            "cat /etc/passwd",
            "cat ~/.aws/credentials",
            "cat ../outside.txt",
            "tail -f /var/log/system.log",
            "cd /tmp",
            "cd ..",
            "head -n 5 /etc/hosts",
            "ls /",
            "find /etc -name x",
            "git diff --no-index /etc/passwd /dev/null",
            "base64 /etc/shadow",
            "grep pattern /etc/passwd",
            // Attached flag values can smuggle a path; reject any no-`=`
            // flag carrying a path-ish character.
            "grep -f/etc/passwd .",
            "grep --file=/etc/passwd .",
            // `-e`/`-f`-style flags shift the pattern position: every
            // non-flag argument is then a potential path.
            "grep -rf /etc/passwd .",
            "grep -e x /etc/passwd",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
        // Confined relative forms stay safe (the pattern position is
        // exempt; `.` and workspace-relative paths are inside).
        for command in [
            "ls",
            "grep -r pattern .",
            "cat README.md",
            "tail -n 20 logs/output.txt",
        ] {
            assert!(safe(command), "expected safe: {command}");
        }
    }

    #[test]
    fn sensitive_basenames_are_unsafe_even_inside_the_workspace() {
        for command in [
            "cat .env",
            "cat .envrc",
            "cat .env.local",
            "cat config/secrets.yaml",
            "cat aws_credentials.json",
            "cat id_rsa",
            "cat keys/id_ed25519",
            "cat server.pem",
            "cat private.key",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn symlink_escaping_the_workspace_is_unsafe() {
        // A symlink INSIDE the workspace pointing outside must not be
        // readable via static-safe approval: existence-based confinement
        // resolves symlinks before the boundary check.
        let temp = tempfile::tempdir().expect("temp");
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("target.txt"), "beyond").expect("seed outside");
        std::fs::write(workspace.join("inside.txt"), "within").expect("seed inside");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("target.txt"), workspace.join("link.txt"))
            .expect("symlink");

        assert!(is_statically_safe_command("cat inside.txt", &workspace));
        #[cfg(unix)]
        assert!(!is_statically_safe_command("cat link.txt", &workspace));
    }

    #[test]
    fn unknown_binaries_are_unsafe() {
        for command in [
            "cargo check",
            "rm -rf /",
            "touch x",
            "foo",
            "npm install",
            "python3 x.py",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn quoted_single_token_command_name_is_unsafe() {
        // `'git status'` is a program NAMED "git status", not git.
        assert!(!safe("'git status'"));
        assert!(!safe("\"git status\""));
    }

    #[test]
    fn exact_name_match_only() {
        // Path-qualified and env-wrapped invocations do not match.
        assert!(!safe("/bin/ls"));
        assert!(!safe("env ls"));
        assert!(!safe("GIT_DIR=.evil git status"));
    }

    #[test]
    fn pipelines_and_lists_of_safe_segments_are_safe() {
        for command in [
            "ls | wc -l",
            "find . -name file.txt | head",
            "grep -r Cargo.toml -n || true",
            "ls && pwd",
            "echo hi ; ls",
            "ls src\nwc -l src/lib.rs",
        ] {
            assert!(safe(command), "expected safe: {command}");
        }
    }

    #[test]
    fn one_unsafe_segment_poisons_the_whole_command() {
        for command in [
            "ls && rm -rf /",
            "rm -rf / && ls",
            "ls | sh",
            "find . -name x | xargs rm",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn find_flag_rules() {
        assert!(safe("find . -name file.txt"));
        assert!(safe("find . -type f -newer ref"));
        for command in [
            "find . -name file.txt -exec rm {} ;",
            "find . -name file.txt -execdir chmod +x {} ;",
            "find . -name file.txt -ok rm {} ;",
            "find . -name file.txt -okdir rm {} ;",
            "find . -delete -name file.txt",
            "find . -fls /etc/passwd",
            "find . -fprint /etc/passwd",
            "find . -fprint0 /etc/passwd",
            "find . -fprintf /root/out.txt %p",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn rg_flag_rules() {
        assert!(safe("rg Cargo.toml -n"));
        assert!(safe("rg --no-ignore pattern src"));
        for command in [
            "rg --pre pwned files",
            "rg --pre=pwned files",
            "rg --hostname-bin pwned files",
            "rg --hostname-bin=pwned files",
            "rg --search-zip files",
            "rg -z files",
            "rg -zn files", // bundled short flags
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn base64_flag_rules() {
        assert!(safe("base64 file"));
        assert!(safe("base64 -d file"));
        for command in [
            "base64 -o out.bin file",
            "base64 -oout.bin file",
            "base64 --output out.bin file",
            "base64 --output=out.bin file",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn sed_print_range_rules() {
        assert!(safe("sed -n 10p file.txt"));
        assert!(safe("sed -n 1,5p file.txt"));
        assert!(safe("sed -n '1,5p' file.txt"));
        assert!(safe("sed -n 1,5p")); // stdin in a pipeline
        for command in [
            "sed -n xp file.txt",
            "sed -n 1,5,9p file.txt",
            "sed -n p file.txt",
            "sed -n 1,p file.txt",
            "sed s/a/b/ file.txt",
            "sed -i s/a/b/ file.txt",
            "sed -n 1,5p a.txt b.txt",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn git_is_never_provably_safe() {
        // Even read-only subcommands run repository-controlled programs:
        // `diff.external`, `core.fsmonitor`, `core.pager`, and clean and
        // smudge filters all come from `.git/config`, which is exactly the
        // file audit F34 showed a "read-only" command could write. `git`
        // returns in Unit 2 behind the sandbox (ADR 0021 row P).
        for command in [
            "git status",
            "git log -p -1",
            "git diff -p",
            "git show -p HEAD",
            "git branch",
            "git branch --show-current",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn git_mutating_and_global_forms_are_unsafe() {
        for command in [
            "git fetch",
            "git checkout status", // first positional is the subcommand
            "git branch -d feature",
            "git branch new-branch",
            "git branch --list pattern", // positional alongside flags
            "git -C . status",
            "git -C. status",
            "git -c core.pager=cat log -n 1",
            "git -p log -1",
            "git --paginate log -1",
            "git --config-env=core.pager=P show HEAD",
            "git --git-dir=.evil-git diff HEAD~1..HEAD",
            "git --exec-path=.git/helpers show HEAD",
            "git --work-tree=. status",
            "git --namespace=attacker show HEAD",
            "git --no-pager log", // any global flag rejects (conservative)
            "git log --output=/tmp/out -n 1",
            "git diff --output /tmp/out",
            "git show --output=/tmp/out HEAD",
            "git log --ext-diff",
            "git diff --textconv",
            "git log --exec=evil",
            "git", // bare git prints help; no subcommand to allow
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn unquoted_expansion_is_never_provably_safe() {
        // Audit F02: confinement used to run on the LITERAL word, so
        // `cat *.txt` was approved against the spelling `*.txt` while the
        // shell expanded it to whatever the directory held — including a
        // symlink pointing outside the workspace. A word the shell may
        // rewrite is not proof of anything, for ANY binary.
        for command in [
            "ls *.rs",
            "wc -l src/*.rs",
            "cat *.txt",
            "cat file?.txt",
            "cat [abc].txt",
            "cat ~/notes.txt",
            "find . -name *.rs",
            "rg pattern *",
            "git status *",
            // A glob in the command-name position never matches anything.
            "l? -la",
            // zsh equals-expansion resolves `=ls` to a binary path.
            "=ls",
            // zsh extendedglob treats `#` as an operator.
            "cat file#1",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
        // Quoted globs are literal text the binary receives verbatim.
        assert!(safe("find . -name '*.rs'"));
        assert!(safe("rg pattern \"*.rs\""));
    }

    #[test]
    fn safe_flag_rules_hold_inside_pipelines() {
        assert!(safe("find . -name '*.rs' | head -n 3"));
        // Legacy attached forms are not on the allowlist.
        assert!(!safe("head -3 Cargo.toml"));
        assert!(!safe("find . -delete | head -3"));
        assert!(!safe("ls | base64 -o out"));
    }

    #[test]
    fn cd_is_never_provably_safe() {
        // Audit F02: `cd` changed the directory every LATER segment
        // resolved against, while confinement kept checking the ORIGINAL
        // root — `cd nested && cat view.txt` was approved against
        // `<root>/view.txt` and read `<root>/nested/view.txt`. The segment
        // grammar has no notion of a moving cwd, so `cd` leaves the
        // read-only set entirely.
        for command in [
            "cd src",
            "cd .",
            "cd nested && cat view.txt",
            "ls && cd src",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn uniq_output_operand_is_never_safe() {
        // Audit F01: `uniq in out` TRUNCATES and writes `out`, and `uniq`
        // used to sit in the flagless read-only list.
        assert!(safe("uniq input.txt"));
        assert!(safe("uniq -c input.txt"));
        assert!(safe("uniq"));
        for command in [
            "uniq input.txt output.txt",
            "uniq -c input.txt output.txt",
            "ls | uniq - output.txt",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn write_and_traversal_flags_are_never_safe() {
        for command in [
            // Output operands and in-place writes: `sort -o` and `tee`
            // are simply not in the read-only set.
            "sort -o out.txt in.txt",
            "sort -n in.txt",
            "tee out.txt",
            "sed -i s/a/b/ file.txt",
            // Audit F02: traversal that dereferences symlinks can read
            // files the confinement check never saw.
            "ls -R",
            "ls -laR",
            "ls -L link",
            "ls --recursive",
            "grep -R pattern .",
            "grep --dereference-recursive pattern .",
            "rg --follow pattern .",
            "rg -L pattern .",
            "find -L . -name x",
            "find . -follow -name x",
            "tail -f log.txt",
            "tail --follow log.txt",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
        // The non-dereferencing forms stay safe.
        for command in ["grep -r pattern .", "rg pattern .", "find . -name x"] {
            assert!(safe(command), "expected safe: {command}");
        }
    }

    #[test]
    fn shell_wrapper_is_safe_exactly_when_its_script_is() {
        for command in [
            "sh -c 'ls -la'",
            "bash -lc 'cat Cargo.toml | wc -l'",
            "zsh -c 'sh -c pwd'",
        ] {
            assert!(safe(command), "expected safe: {command}");
        }
        for command in [
            "sh -c 'cat /etc/passwd'",
            "bash -lc 'rm -rf .'",
            "sh -c 'cat *.txt'",
            "sh -c 'uniq in out'",
            // Only the three-word wrapper form is recognized.
            "sh -c ls extra",
            "sh -x -c ls",
            "/bin/sh -c ls",
            "sh --norc -c ls",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
    }

    #[test]
    fn git_metadata_and_interpreter_config_are_sensitive() {
        // Audit F34: `.git/config` selects hooks, filters, and pagers, so
        // writing one turns a later `git status` into arbitrary execution.
        for path in [
            ".git",
            ".git/config",
            ".git/hooks/pre-commit",
            "vendor/.git/config",
            ".gitmodules",
            ".gitattributes",
            ".gitconfig",
            ".cargo/config.toml",
            ".cargo/config",
            ".npmrc",
            ".netrc",
            ".bashrc",
            ".zshrc",
            ".zshenv",
            ".profile",
            ".env",
            "config/secrets.yaml",
        ] {
            assert!(
                sensitive_basename(Path::new(path)),
                "expected sensitive: {path}"
            );
        }
        for path in [
            "Cargo.toml",
            "config.toml",
            "src/config",
            "README.md",
            ".gitignore",
        ] {
            assert!(
                !sensitive_basename(Path::new(path)),
                "expected ordinary: {path}"
            );
        }
        assert!(!safe("cat .git/config"));
        assert!(!safe("grep -r url .git"));
    }

    /// Fixture from `audit/2026-09-05` `reproduces_uniq_write_without_approval`,
    /// asserting the FIXED behavior: the write never reaches static
    /// approval, so it takes an ordinary permission decision (audit F01).
    #[test]
    fn audit_f01_uniq_write_fixture_now_requires_approval() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let root = temp.path();
        std::fs::write(root.join("input.txt"), "a\na\nb\n").expect("seed input");
        std::fs::write(root.join("output.txt"), "user-owned original\n").expect("seed output");
        assert!(!is_statically_safe_command(
            "uniq input.txt output.txt",
            root
        ));
        assert!(is_statically_safe_command("uniq input.txt", root));
        // Nothing ran: the user's file is untouched by the analysis.
        assert_eq!(
            std::fs::read_to_string(root.join("output.txt")).expect("read output"),
            "user-owned original\n"
        );
    }

    /// Fixture from `audit/2026-09-05`
    /// `reproduces_glob_and_cd_read_scope_bypass`, asserting the FIXED
    /// behavior: each of the three approved reads of a file outside the
    /// workspace now falls to the ask path (audit F02).
    #[test]
    #[cfg(unix)]
    fn audit_f02_glob_cd_and_follow_fixture_now_requires_approval() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(root.join("nested")).expect("workspace");
        let outside = temp.path().join("outside.txt");
        std::fs::write(&outside, "SYNTHETIC_OUTSIDE_MARKER\n").expect("seed outside");
        std::os::unix::fs::symlink(&outside, root.join("public.txt")).expect("symlink");
        std::os::unix::fs::symlink(&outside, root.join("nested/view.txt")).expect("symlink");
        std::fs::write(root.join("view.txt"), "inside\n").expect("seed inside");

        assert!(!is_statically_safe_command("cat public.txt", &root));
        for command in [
            "cat *.txt",
            "cd nested && cat view.txt",
            "rg --follow SYNTHETIC .",
        ] {
            assert!(
                !is_statically_safe_command(command, &root),
                "expected unsafe: {command}"
            );
        }
        // The confined read of the real file inside the workspace still
        // runs without a prompt.
        assert!(is_statically_safe_command("cat view.txt", &root));
    }

    #[test]
    fn danger_walk_finds_forced_rm_through_wrappers_and_control_flow() {
        for command in [
            "rm -f file",
            "rm -rf /",
            "rm --force file",
            "sudo rm -f file",
            "sudo -u root rm -rf .",
            "env FOO=1 rm -f file",
            "env -i -- rm -rf .",
            "nohup rm -f file",
            "time rm -rf .",
            "find . -name x | xargs rm -f",
            "ls | xargs -n 1 rm -f",
            "trap 'rm -rf .' EXIT",
            "sh -c 'rm -f file'",
            "bash -lc \"rm -rf .\"",
            "if true; then rm -f file; fi",
            "for f in *; do rm -f $f; done",
            "echo $(rm -f file)",
            "echo `rm -f file`",
            "FOO=bar rm -f file",
            "ls && rm -f file",
            "/bin/rm -f file",
            "$SUDO rm -f file",
        ] {
            assert!(
                contains_dangerous_command(command),
                "expected dangerous: {command}"
            );
        }
    }

    #[test]
    fn danger_walk_leaves_ordinary_commands_alone() {
        for command in [
            "ls -la",
            "rm file",
            "rm -- -f",
            "git status",
            "cargo test --all-features",
            "echo rm -f",
            "grep -rf patterns.txt .",
            "find . -name x | xargs ls",
            "sh -c 'ls -la'",
        ] {
            assert!(
                !contains_dangerous_command(command),
                "expected not dangerous: {command}"
            );
        }
    }

    #[test]
    fn danger_walk_fails_closed_past_the_wrapper_depth_cap() {
        let deep = "sudo ".repeat(MAX_WRAPPER_DEPTH + 2) + "ls";
        assert!(contains_dangerous_command(&deep));
        let shallow = "sudo sudo ls";
        assert!(!contains_dangerous_command(shallow));
    }

    #[test]
    fn dangerous_commands_are_never_statically_safe() {
        // Defense in depth: the grammar already rejects `rm`, but a
        // dangerous command must never be provable by any future rule.
        let temp = tempfile::tempdir().expect("temp workspace");
        assert!(!is_statically_safe_command(
            "sh -c 'rm -f file'",
            temp.path()
        ));
    }

    #[test]
    fn every_operand_is_confined_with_no_exempt_position() {
        // Review round 2, finding 1: the old parser exempted "the pattern
        // position", computed as the first non-flag word, so `--` or a
        // dash-leading pattern shifted the exemption onto the FILE operand
        // and read it. There is no exempt position now.
        for command in [
            "grep -v -- -zzz /etc/passwd",
            "grep -- -x /etc/passwd",
            "grep -- -x .env",
            "rg -- -x /etc/passwd",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
        assert!(safe("grep -r pattern ."));
    }

    #[test]
    fn options_are_an_allowlist_not_a_denylist() {
        // Review round 2, finding 2: every one of these evaded a
        // per-binary denylist — an attached value, a bundle carrying an
        // unlisted letter, a GNU long abbreviation, or an operand after
        // `--`. With an exact-spelling allowlist they are all unprovable.
        for command in [
            "uniq -- Cargo.toml -x",
            "base64 -Do out.txt",
            "base64 -i.env",
            "sed -n 1p -ewCargo.toml",
            "grep --dereference-rec pattern .",
            "ls --recu",
            "tail --fol Cargo.toml",
            "grep -rS pattern .",
            "ls -laR",
            "rg -L pattern .",
            "head -n50 Cargo.toml",
            "cut -d, -f1 Cargo.toml",
        ] {
            assert!(!safe(command), "expected unsafe: {command}");
        }
        // The exact spellings still work, including detached values.
        for command in [
            "ls -la",
            "ls --color=always",
            "grep -rn pattern src",
            "head -n 50 Cargo.toml",
            "cut -d , -f 1 Cargo.toml",
            "uniq -c input.txt",
        ] {
            assert!(safe(command), "expected safe: {command}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn in_workspace_symlink_to_a_sensitive_file_is_unsafe() {
        // Review round 2, finding 4: confinement resolves symlinks, so the
        // RESOLVED name must face the sensitive list too — otherwise an
        // innocent name reads `.git/config` (which selects hooks) or
        // `.env`.
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path();
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::write(root.join(".git/config"), "[core]\n").expect("config");
        std::fs::write(root.join(".env"), "TOKEN=1\n").expect("env");
        std::fs::write(root.join("plain.txt"), "ok\n").expect("plain");
        std::os::unix::fs::symlink(root.join(".git/config"), root.join("link")).expect("link");
        std::os::unix::fs::symlink(root.join(".env"), root.join("envlink")).expect("link");

        assert!(!is_statically_safe_command("cat link", root));
        assert!(!is_statically_safe_command("cat envlink", root));
        assert!(is_statically_safe_command("cat plain.txt", root));
    }

    #[test]
    fn danger_walk_unwraps_every_wrapper_family() {
        // Review round 2, finding 6.
        for command in [
            "exec rm -f file",
            "eval 'rm -f file'",
            "command rm -f file",
            "builtin rm -f file",
            "timeout 5 rm -f file",
            "nice rm -f file",
            "stdbuf -o0 rm -f file",
            "setsid rm -f file",
            "flock /tmp/lock rm -f file",
            "ionice rm -f file",
            "chrt 1 rm -f file",
            "su user -c 'rm -f file'",
            "find . -name x -exec rm -f {} +",
            "find . -delete",
            "env -u FOO rm -f file",
            "env -S 'rm -f file'",
            "env --split-string='rm -f file'",
            "env --bogus-option rm file",
            "sh -cx 'rm -f file'",
            "sh -c -- 'rm -f file'",
            "sh -c -e 'rm -f file'",
            "rm --forc file",
            "rm --recursi dir",
        ] {
            assert!(
                contains_dangerous_command(command),
                "expected dangerous: {command}"
            );
        }
    }

    #[test]
    fn danger_walk_sees_past_redirections() {
        // Review round 2, finding 7: a redirection glued to the command
        // word hid the argv from a tokenizer. In the AST it is a sibling
        // node, so the argv is intact.
        for command in [
            "rm 2>&1 -rf dir",
            "rm>/dev/null -rf dir",
            "rm &>/dev/null -rf dir",
            "rm -rf dir >/dev/null 2>&1",
        ] {
            assert!(
                contains_dangerous_command(command),
                "expected dangerous: {command}"
            );
        }
    }

    #[test]
    fn danger_walk_fails_closed_on_dynamic_words() {
        // Review round 2, finding 8: a dropped word could be the flag that
        // makes the command destructive, and a dynamic command name could
        // be anything.
        for command in [
            "M=m r$M -f file",
            "rm $FLAGS file",
            "rm -rf$X dir",
            "${RM} -rf dir",
            "$(which rm) -rf dir",
            "sudo $CMD",
        ] {
            assert!(
                contains_dangerous_command(command),
                "expected dangerous: {command}"
            );
        }
        // A dynamic word in an ordinary command is not by itself danger:
        // over-flagging every variable would prompt on `echo $HOME`.
        assert!(!contains_dangerous_command("echo $HOME"));
        assert!(!contains_dangerous_command("cat \"$file\""));
    }

    #[test]
    fn danger_table_covers_destructive_commands_beyond_forced_rm() {
        // Review round 2, finding 9: `run_shell` closes stdin, so `rm -r`
        // never gets its confirmation prompt.
        for command in [
            "rm -r dir",
            "rm -R dir",
            "rm --recursive dir",
            "rm -f file",
            "rm --force file",
            "git clean -fd",
            "git clean --force",
            "shred secrets.bin",
            "truncate -s 0 Cargo.toml",
            "dd if=/dev/zero of=/dev/sda",
            "mkfs.ext4 /dev/sda1",
            "wipefs /dev/sda",
        ] {
            assert!(
                contains_dangerous_command(command),
                "expected dangerous: {command}"
            );
        }
        for command in [
            "rm file",
            "git clean --dry-run",
            "dd if=in of.txt",
            "ls -la",
        ] {
            assert!(
                !contains_dangerous_command(command),
                "expected not dangerous: {command}"
            );
        }
    }

    #[test]
    fn deeply_nested_substitution_neither_overflows_nor_passes() {
        // Review round 2: `$(` repeated thousands of times crashed the
        // recursive walk on a 2 MiB worker stack. Both walks are iterative
        // and node-bounded now.
        let bomb = "$(".repeat(2048);
        assert!(contains_dangerous_command(&bomb));
        let temp = tempfile::tempdir().expect("temp workspace");
        assert!(!is_statically_safe_command(&bomb, temp.path()));
        let nested = format!("{}ls{}", "$(".repeat(64), ")".repeat(64));
        assert!(!is_statically_safe_command(&nested, temp.path()));
    }
}
