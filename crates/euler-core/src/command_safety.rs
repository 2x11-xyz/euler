//! Static shell-command safety analysis (issue #78).
//!
//! `run_shell` executes via `sh -c <command>`, so any reasoning about a
//! command line must reason about the *whole* line. This module implements
//! the **two-parser design** Codex converged on
//! (`codex-rs/shell-command/src/bash.rs`): one conservative parser that may
//! prove a command safe, and one permissive walk that may only find danger.
//! A single parser cannot be both conservative and complete, and the
//! name-keyed allowlist this module used to carry tried to be both (audit
//! F01/F02/F34).
//!
//! ## 1. Prove-safe grammar (conservative)
//!
//! [`is_statically_safe_command`] returns true only for command lines that
//! parse into a sequence of simple commands joined by `&&`, `||`, `;`, `|`,
//! or newlines, where:
//!
//! - every word is a **literal**: no unquoted `* ? [ ] { } ~ $ ` \ ^ #`,
//!   and no word beginning with `=` (zsh equals-expansion). A word whose
//!   runtime spelling the shell may rewrite is never proof of anything;
//! - there are no redirections (`>`, `<`, `>>`, here-docs, fd forms),
//!   substitutions, subshells, grouping, brace expansion, background `&`,
//!   comments, or control flow — all of these make the line unparseable;
//! - every simple command's binary is in the read-only set below **and**
//!   its arguments satisfy that binary's argument rule;
//! - every argument that may name a path is confined to the workspace root
//!   and is not a sensitive path.
//!
//! The wrapper form `[sh|bash|zsh] -c|-lc <script>` is accepted only by
//! recursively proving `<script>` under the same rules (depth-capped).
//!
//! Binary names match the first token exactly: `/bin/ls` or `env ls` do not
//! match `ls`.
//!
//! ### Read-only set and per-binary argument rules
//!
//! | Binary | Rule |
//! | --- | --- |
//! | `cat` `cut` `echo` `expr` `false` `id` `nl` `paste` `pwd` `rev` `seq` `stat` `tr` `true` `uname` `wc` `which` `whoami` `head` | no flag or operand of these writes, executes, or traverses |
//! | `ls` | no `-R`/`--recursive`, no `-L`/`--dereference` (audit F02) |
//! | `grep` | no `-R`/`--dereference-recursive` (audit F02; `-r` does not follow symlinks) |
//! | `tail` | no `-f`/`-F`/`--follow`/`--retry` |
//! | `uniq` | at most one operand — the second operand is an output file (audit F01) |
//! | `find` | no `-exec`/`-execdir`/`-ok`/`-okdir`/`-delete`/`-fls`/`-fprint*`, no `-L`/`-follow` |
//! | `rg` | no `--pre`/`--hostname-bin`/`--search-zip`/`-z`, no `-L`/`--follow` |
//! | `base64` | no `-o`/`--output` |
//! | `sed` | only the print-range form `sed -n Np [file]` |
//! | `git` | only `status`/`log`/`diff`/`show`/`branch`, no global options |
//!
//! Binaries outside the table are never provably safe, `sort` (`-o` writes
//! a file) and `tee` among them. `cd` is deliberately **not** in the set: a compound list may change the
//! directory the following commands resolve against, and confinement is
//! checked against the root the command started in (audit F02).
//!
//! ## 2. Find-danger walk (permissive)
//!
//! [`contains_dangerous_command`] walks *every* command in the input,
//! including inside control flow, substitutions, and wrappers, and flags
//! dangerous invocations. It is deliberately over-inclusive and **must
//! never be used to prove safety** — see its doc comment.
//!
//! ## Workspace confinement (security review F1)
//!
//! Read-only is not harmless: `cat ~/.aws/credentials` writes nothing and
//! still exfiltrates. A segment is only statically safe when every argument
//! that may name a filesystem path stays inside the workspace root the
//! command executes in (`sh -c` runs in that root):
//!
//! - an argument naming an existing path must canonicalize (symlinks
//!   resolved) to a location under the canonicalized root;
//! - a non-existing argument must pass textual rules: no absolute path, no
//!   leading `~`, no `$` or backtick, no `..` component — a relative path
//!   without `..` cannot leave the execution cwd;
//! - the sensitive-path denylist ([`sensitive_basename`]) rejects even
//!   inside the workspace;
//! - argument positions are classified conservatively: only the grep/rg
//!   pattern position is exempt, and only when no `-e`/`-f`-style flag can
//!   shift it; everything else — including `--flag=value` values — is
//!   treated as a potential path.
//!
//! A rejected segment is simply not statically safe: the command falls to
//! the ordinary ask path (no new denial surface). False negatives (safe
//! commands classified unsafe) only cost a prompt; false positives are the
//! failure mode this module must never have.

use std::path::{Component, Path};

/// Recursion cap for wrapper unwrapping, in both parsers (Codex uses the
/// same bound). Exceeding it fails closed: unprovable for the prove-safe
/// grammar, dangerous for the danger walk.
const MAX_WRAPPER_DEPTH: usize = 8;

/// One word of a parsed segment, quotes resolved to literal text.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Word {
    text: String,
    /// Word contained at least one unquoted character the shell may rewrite
    /// before the binary sees it (`* ? [ ] ~ ^ #`), so its source spelling
    /// is not proof of the runtime argv.
    has_unquoted_expansion: bool,
}

impl Word {
    /// Whether this word's spelling is exactly what the binary will see.
    fn is_literal(&self) -> bool {
        // A leading `=` is zsh equals-expansion (`=ls` → `/bin/ls`).
        !self.has_unquoted_expansion && !self.text.starts_with('=')
    }
}

/// One plain command of a parsed line (a pipeline or list element).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSegment {
    /// Never empty: the parser rejects empty segments.
    words: Vec<Word>,
}

impl CommandSegment {
    /// The command name of this segment (quotes already resolved, so a
    /// quoted `'git status'` is one token named `git status`).
    pub fn first_token(&self) -> &str {
        &self.words[0].text
    }

    /// Whether this segment is a known read-only invocation whose path
    /// arguments are confined to `workspace_root` (see the module docs,
    /// "Prove-safe grammar" and "Workspace confinement").
    pub fn is_statically_safe(&self, workspace_root: &Path) -> bool {
        self.is_statically_safe_at_depth(workspace_root, 0)
    }

    fn is_statically_safe_at_depth(&self, workspace_root: &Path, depth: usize) -> bool {
        // Every word must be a literal: an unquoted glob, `~`, or `^` is
        // rewritten by the shell, so neither the binary name, the flag
        // rules, nor confinement can be checked against it (audit F02).
        if !self.words.iter().all(Word::is_literal) {
            return false;
        }
        if let Some(script) = self.wrapper_script() {
            return depth < MAX_WRAPPER_DEPTH
                && is_statically_safe_at_depth(script, workspace_root, depth + 1);
        }
        self.is_read_only_invocation() && self.paths_confined(workspace_root)
    }

    /// The script of a wrapper invocation `[sh|bash|zsh] -c|-lc <script>`,
    /// which is safe exactly when the script is (checked recursively).
    fn wrapper_script(&self) -> Option<&str> {
        let [shell, flag, script] = self.words.as_slice() else {
            return None;
        };
        let is_shell = matches!(shell.text.as_str(), "sh" | "bash" | "zsh");
        let is_command_flag = matches!(flag.text.as_str(), "-c" | "-lc");
        (is_shell && is_command_flag).then_some(script.text.as_str())
    }

    /// Read-only set plus the per-binary argument rule (module docs).
    fn is_read_only_invocation(&self) -> bool {
        let args: Vec<&str> = self.words[1..]
            .iter()
            .map(|word| word.text.as_str())
            .collect();
        match self.words[0].text.as_str() {
            // Nothing these binaries accept makes them write, execute
            // another program, traverse out of the execution directory, or
            // mutate state beyond the shell process.
            "cat" | "cut" | "echo" | "expr" | "false" | "head" | "id" | "nl" | "paste" | "pwd"
            | "rev" | "seq" | "stat" | "tr" | "true" | "uname" | "wc" | "which" | "whoami" => true,
            "ls" => is_safe_ls(&args),
            "grep" => is_safe_grep(&args),
            "tail" => is_safe_tail(&args),
            "uniq" => is_safe_uniq(&args),
            "find" => is_safe_find(&args),
            "rg" => is_safe_rg(&args),
            "base64" => is_safe_base64(&args),
            "sed" => is_safe_sed(&args),
            "git" => is_safe_git(&args),
            _ => false,
        }
    }

    /// Workspace confinement (security review F1): every argument that may
    /// name a filesystem path must stay inside `workspace_root`. Position
    /// classification is conservative — when in doubt whether an argument
    /// is a path, path rules apply (over-rejection costs one ask prompt;
    /// under-rejection exfiltrates).
    fn paths_confined(&self, workspace_root: &Path) -> bool {
        // Canonicalize the root itself (macOS `/var` is a symlink):
        // unresolvable root means nothing can be proven confined.
        let Ok(root) = workspace_root.canonicalize() else {
            return false;
        };
        let args = &self.words[1..];
        let pattern_index = pattern_position(self.first_token(), args);
        args.iter().enumerate().all(|(index, word)| {
            if Some(index) == pattern_index {
                return true;
            }
            let arg = word.text.as_str();
            if let Some(rest) = arg.strip_prefix('-') {
                // `--flag=value`: the value may name a path. A no-`=` flag
                // carrying a path-ish character (`-f/etc/passwd` attached
                // value) is rejected outright — per-binary attached-value
                // grammars are exactly the ambiguity this check must not
                // guess about.
                match arg.split_once('=') {
                    Some((_, value)) => arg_confined(value, &root),
                    None => !rest.contains(['/', '~', '$', '`']),
                }
            } else {
                arg_confined(arg, &root)
            }
        })
    }
}

/// grep/rg read their pattern from the first non-flag argument — a regex is
/// not a path, so that one position is exempt from confinement — UNLESS a
/// pattern/file flag (`-e`, `-f`, `--regexp`, `--file`, or a short cluster
/// containing `e` or `f` such as `-rf`) could shift positions: then every
/// non-flag argument is treated as a path (the safe direction).
fn pattern_position(binary: &str, args: &[Word]) -> Option<usize> {
    if !matches!(binary, "grep" | "rg") {
        return None;
    }
    let has_pattern_flag = args.iter().any(|word| {
        let arg = word.text.as_str();
        arg.starts_with("--regexp")
            || arg.starts_with("--file")
            || arg
                .strip_prefix('-')
                .is_some_and(|rest| !rest.starts_with('-') && rest.contains(['e', 'f']))
    });
    if has_pattern_flag {
        return None;
    }
    args.iter().position(|word| !word.text.starts_with('-'))
}

/// One potential path argument, checked against the canonicalized root.
fn arg_confined(arg: &str, canonical_root: &Path) -> bool {
    if arg.is_empty() || arg == "-" {
        // Empty word / stdin convention: not a path.
        return true;
    }
    // Textual rejections apply regardless of existence: `~` and `$`/backtick
    // are rewritten by the shell before the binary ever sees them.
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
        // Absolute and parent-traversing forms are rejected textually even
        // when they would resolve inside the workspace — over-rejection
        // costs one prompt.
        return false;
    }
    match canonical_root.join(path).canonicalize() {
        // Existing path: symlinks resolved, must land under the root and
        // must not resolve to a sensitive name.
        Ok(resolved) => resolved.starts_with(canonical_root) && !sensitive_basename(&resolved),
        // Nonexistent/unresolvable: the textual rules above already hold,
        // and `sh -c` runs in the workspace root — a relative path without
        // `..` cannot leave it.
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

/// Decompose a command line into plain segments across `&&`, `||`, `;`,
/// `|`, and newlines. Returns `None` when the line is not statically
/// analyzable (see the module docs for the full rejection list).
///
/// Words carrying shell expansion (`*`, `~`, …) still parse — grant
/// coverage keys on the first token — but never prove safe.
pub fn parse_plain_segments(command: &str) -> Option<Vec<CommandSegment>> {
    let mut builder = SegmentBuilder::default();
    // `&&` / `||` / `|` require a command on their right (newlines may
    // intervene); end-of-input while one is pending is a syntax error.
    let mut needs_command = false;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        if !matches!(c, ' ' | '\t' | '\n' | ';' | '|' | '&') {
            needs_command = false;
        }
        match c {
            '\'' => {
                builder.in_word = true;
                scan_single_quoted(&mut chars, &mut builder.text)?;
            }
            '"' => {
                builder.in_word = true;
                scan_double_quoted(&mut chars, &mut builder.text)?;
            }
            '\\' => match chars.next()? {
                // Line continuation disappears entirely; a trailing
                // backslash is `chars.next()?` → unparseable.
                '\n' => {}
                escaped => builder.push_char(escaped),
            },
            ' ' | '\t' => builder.flush_word(),
            '\n' => builder.flush_segment(false)?,
            ';' => builder.flush_segment(true)?,
            '|' => {
                chars.next_if_eq(&'|');
                builder.flush_segment(true)?;
                needs_command = true;
            }
            '&' => {
                // A single `&` is background execution — unparseable.
                chars.next_if_eq(&'&')?;
                builder.flush_segment(true)?;
                needs_command = true;
            }
            // Comments only start at word boundaries; a mid-word `#` is
            // literal to `sh` but a glob operator under zsh's
            // `extendedglob`, so it marks the word instead.
            '#' if !builder.in_word => return None,
            '>' | '<' | '(' | ')' | '{' | '}' | '`' | '$' | '\r' => return None,
            expansion @ ('*' | '?' | '[' | ']' | '~' | '^' | '#') => {
                builder.push_expansion_char(expansion);
            }
            other => builder.push_char(other),
        }
    }

    // A dangling `&&`/`||`/`|` (`ls &&`) is a shell syntax error; a
    // trailing `;` or newline is ordinary.
    if needs_command {
        return None;
    }
    builder.finish()
}

#[derive(Default)]
struct SegmentBuilder {
    segments: Vec<CommandSegment>,
    words: Vec<Word>,
    text: String,
    expansion: bool,
    in_word: bool,
}

impl SegmentBuilder {
    fn push_char(&mut self, c: char) {
        self.text.push(c);
        self.in_word = true;
    }

    fn push_expansion_char(&mut self, c: char) {
        self.push_char(c);
        self.expansion = true;
    }

    fn flush_word(&mut self) {
        if self.in_word {
            self.words.push(Word {
                text: std::mem::take(&mut self.text),
                has_unquoted_expansion: self.expansion,
            });
            self.expansion = false;
            self.in_word = false;
        }
    }

    /// Hard separators (`;`, `|`, `&&`, `||`) require a non-empty segment
    /// on their left; newlines tolerate blank lines between commands.
    fn flush_segment(&mut self, require_words: bool) -> Option<()> {
        self.flush_word();
        if self.words.is_empty() {
            if require_words {
                return None;
            }
        } else {
            self.segments.push(CommandSegment {
                words: std::mem::take(&mut self.words),
            });
        }
        Some(())
    }

    fn finish(mut self) -> Option<Vec<CommandSegment>> {
        self.flush_word();
        if !self.words.is_empty() {
            self.segments.push(CommandSegment { words: self.words });
        }
        if self.segments.is_empty() {
            return None;
        }
        Some(self.segments)
    }
}

/// Consume through the closing `'`. Everything inside is literal.
fn scan_single_quoted(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    text: &mut String,
) -> Option<()> {
    loop {
        match chars.next()? {
            '\'' => return Some(()),
            ch => text.push(ch),
        }
    }
}

/// Consume through the closing `"`. Backslash escapes `$`, `` ` ``, `"`,
/// `\` (otherwise stays literal); an unescaped `$` or backtick is expansion
/// and rejects the whole command.
fn scan_double_quoted(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    text: &mut String,
) -> Option<()> {
    loop {
        match chars.next()? {
            '"' => return Some(()),
            '\\' => match chars.next()? {
                escaped @ ('$' | '`' | '"' | '\\') => text.push(escaped),
                other => {
                    text.push('\\');
                    text.push(other);
                }
            },
            '$' | '`' => return None,
            ch => text.push(ch),
        }
    }
}

/// Whether `command` parses into plain segments that are ALL statically
/// safe read-only invocations confined to `workspace_root`.
///
/// A command the permissive danger walk flags is never provably safe, even
/// if the grammar would otherwise accept it (defense in depth).
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

/// True when no argument is, or bundles, one of `short` (single-dash
/// cluster), and no argument equals or `=`-prefixes one of `long`.
fn rejects_flags(args: &[&str], short: &[char], long: &[&str]) -> bool {
    !args.iter().any(|arg| {
        long.iter()
            .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))
            || arg
                .strip_prefix('-')
                .is_some_and(|rest| !rest.starts_with('-') && rest.contains(short))
    })
}

/// `ls -R` walks a tree and `ls -L` dereferences: both can report paths the
/// confinement check never saw (audit F02).
fn is_safe_ls(args: &[&str]) -> bool {
    rejects_flags(args, &['R', 'L'], &["--recursive", "--dereference"])
}

/// `grep -R` follows symlinks out of the workspace; plain `-r` does not
/// (it only dereferences command-line operands, which are confined).
fn is_safe_grep(args: &[&str]) -> bool {
    rejects_flags(args, &['R'], &["--dereference-recursive"])
}

/// `tail -f`/`-F` never terminates and keeps reading a file that may be
/// replaced after the confinement check.
fn is_safe_tail(args: &[&str]) -> bool {
    rejects_flags(args, &['f', 'F'], &["--follow", "--retry"])
}

/// `uniq [input [output]]`: the SECOND operand is an output file that
/// `uniq` truncates, which is how a read-only-looking binary wrote a file
/// with no approval (audit F01). Only the stdin/one-operand forms are safe.
/// Flags with detached values (`uniq -f 1 in`) count their value as an
/// operand and simply fall to the ask path.
fn is_safe_uniq(args: &[&str]) -> bool {
    // `-` is the explicit stdin operand and occupies an operand position:
    // `uniq - out.txt` still writes `out.txt`.
    args.iter()
        .filter(|arg| **arg == "-" || !arg.starts_with('-'))
        .count()
        <= 1
}

fn is_safe_find(args: &[&str]) -> bool {
    // Actions that execute commands, delete files, or write pathnames.
    const UNSAFE_FIND_ARGS: &[&str] = &[
        "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fls", "-fprint", "-fprint0", "-fprintf",
    ];
    if args.iter().any(|arg| UNSAFE_FIND_ARGS.contains(arg)) {
        return false;
    }
    // `-L` / `-follow` descend through symlinks out of the workspace.
    !args.iter().any(|arg| matches!(*arg, "-L" | "-follow"))
}

fn is_safe_rg(args: &[&str]) -> bool {
    // --pre / --hostname-bin execute external commands; --search-zip / -z
    // shell out to decompression tools; -L / --follow descends through
    // symlinks out of the workspace. Short flags may be bundled (`-zn`), so
    // any single-dash cluster containing `z` or `L` rejects — a
    // false-unsafe on flag values (`-ezoo`) only costs a prompt.
    const UNSAFE_RG_VALUE_FLAGS: &[&str] = &["--pre", "--hostname-bin"];
    if args.iter().any(|arg| {
        *arg == "--search-zip"
            || UNSAFE_RG_VALUE_FLAGS
                .iter()
                .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))
    }) {
        return false;
    }
    rejects_flags(args, &['z', 'L'], &["--follow"])
}

fn is_safe_base64(args: &[&str]) -> bool {
    // -o / --output write to a file.
    !args
        .iter()
        .any(|arg| arg.starts_with("-o") || *arg == "--output" || arg.starts_with("--output="))
}

/// Only the print-range form `sed -n Np [file]` / `sed -n M,Np [file]` is
/// safe: no scripts, no in-place editing, no write commands.
fn is_safe_sed(args: &[&str]) -> bool {
    matches!(args.len(), 2 | 3) && args[0] == "-n" && is_sed_print_range(args[1])
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

/// Read-only `git`: the token right after `git` must be one of the allowed
/// subcommands — ANY global option (`-C`, `-c`, `-p`/`--paginate`,
/// `--git-dir`, `--exec-path`, `--work-tree`, `--config-env`,
/// `--namespace`, and every other leading flag) rejects, stricter than a
/// denylist and immune to option growth.
fn is_safe_git(args: &[&str]) -> bool {
    let Some((&subcommand, rest)) = args.split_first() else {
        return false;
    };
    if !matches!(subcommand, "status" | "log" | "diff" | "show" | "branch") {
        return false;
    }
    if rest.iter().any(|arg| is_unsafe_git_subcommand_arg(arg)) {
        return false;
    }
    if subcommand == "branch" {
        return git_branch_args_are_read_only(rest);
    }
    true
}

fn is_unsafe_git_subcommand_arg(arg: &str) -> bool {
    // --output writes files; --ext-diff / --textconv / --exec run
    // configured external commands.
    matches!(arg, "--output" | "--ext-diff" | "--textconv" | "--exec")
        || arg.starts_with("--output=")
        || arg.starts_with("--exec=")
}

/// `git branch` is safe only as a pure listing query: bare, or made
/// exclusively of read-only flags. Any positional argument or unknown flag
/// may create, rename, or delete branches.
fn git_branch_args_are_read_only(args: &[&str]) -> bool {
    args.iter().all(|arg| {
        matches!(
            *arg,
            "--list"
                | "-l"
                | "--show-current"
                | "-a"
                | "--all"
                | "-r"
                | "--remotes"
                | "-v"
                | "-vv"
                | "--verbose"
        ) || arg.starts_with("--format=")
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
/// looks inside control flow, command substitutions, quoted wrapper
/// scripts, and `sudo`/`env`/`trap`/`nohup`/`time`/`xargs` wrappers. Words
/// it cannot resolve statically are dropped rather than rejected, so
/// `false` means only "found nothing dangerous", never "safe". It
/// deliberately over-flags: a false positive costs one prompt.
///
/// Dangerous today means a forced `rm` (`-f`, `--force`, `-rf`, …), which
/// destroys user data with no undo and no prompt, plus anything nested
/// deeper than [`MAX_WRAPPER_DEPTH`] wrappers (unreadable, so fail closed).
pub fn contains_dangerous_command(command: &str) -> bool {
    script_has_dangerous_command(command, 0)
}

fn script_has_dangerous_command(script: &str, depth: usize) -> bool {
    if depth > MAX_WRAPPER_DEPTH {
        return true;
    }
    literal_commands(script)
        .iter()
        .any(|argv| command_is_dangerous(argv, depth))
}

/// Shell keywords that may precede a command inside a compound statement;
/// the danger walk splits on operators, not on grammar, so they arrive as
/// leading words.
const LEADING_KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "while", "until", "do", "done", "for", "in", "case",
    "esac", "select", "function", "!", "[[", "{", "}",
];

/// Wrappers whose operands are themselves a command to inspect.
const COMMAND_WRAPPERS: &[&str] = &["sudo", "nohup", "time", "xargs", "doas"];

fn command_is_dangerous(argv: &[String], depth: usize) -> bool {
    if depth > MAX_WRAPPER_DEPTH {
        return true;
    }
    let argv = strip_leading_keywords(argv);
    let Some(name) = argv.first().map(|word| basename(word)) else {
        return false;
    };
    if name == "rm" {
        return rm_args_include_force(&argv[1..]);
    }
    if COMMAND_WRAPPERS.contains(&name) {
        // Which operand starts the wrapped command depends on flags this
        // walk does not model (`xargs -n 1 rm -f`), so try every suffix:
        // over-flagging costs a prompt, missing one costs data.
        return (1..argv.len()).any(|start| command_is_dangerous(&argv[start..], depth + 1));
    }
    if name == "env" {
        return command_is_dangerous(env_wrapped_command(&argv[1..]), depth + 1);
    }
    if name == "trap" {
        // A trap action is shell source stored in the first operand.
        return trap_action(&argv[1..])
            .is_some_and(|action| script_has_dangerous_command(action, depth + 1));
    }
    if matches!(name, "sh" | "bash" | "zsh" | "dash" | "ksh") {
        return wrapper_scripts(&argv[1..])
            .iter()
            .any(|script| script_has_dangerous_command(script, depth + 1));
    }
    false
}

fn basename(raw: &str) -> &str {
    raw.rsplit('/').next().unwrap_or(raw)
}

fn strip_leading_keywords(argv: &[String]) -> &[String] {
    let mut rest = argv;
    while let Some(first) = rest.first() {
        if LEADING_KEYWORDS.contains(&first.as_str()) || is_assignment(first) {
            rest = &rest[1..];
        } else {
            break;
        }
    }
    rest
}

/// `NAME=value` prefixes a command with an environment assignment.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// Skip `env`'s own assignments and options to reach the wrapped command.
fn env_wrapped_command(args: &[String]) -> &[String] {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            index += 1;
            break;
        }
        if matches!(argument.as_str(), "-i" | "--ignore-environment") || is_assignment(argument) {
            index += 1;
            continue;
        }
        break;
    }
    &args[index.min(args.len())..]
}

/// `trap [--] <action> <signal>…`: the action is shell source.
fn trap_action(args: &[String]) -> Option<&str> {
    let mut index = 0;
    if args.first().is_some_and(|argument| argument == "--") {
        index = 1;
    }
    args.get(index)
        .filter(|action| !action.starts_with('-'))
        .map(String::as_str)
}

/// Scripts passed to a shell as `-c`/`-lc` operands.
fn wrapper_scripts(args: &[String]) -> Vec<&str> {
    args.iter()
        .enumerate()
        .filter(|(_, argument)| {
            argument
                .strip_prefix('-')
                .is_some_and(|rest| !rest.starts_with('-') && rest.ends_with('c'))
        })
        .filter_map(|(index, _)| args.get(index + 1).map(String::as_str))
        .collect()
}

fn rm_args_include_force(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            arg == "--force"
                || arg
                    .strip_prefix('-')
                    .is_some_and(|flags| !flags.starts_with('-') && flags.contains('f'))
        })
}

/// Permissively collect the statically known words of every command in a
/// script, including commands nested in control flow, substitutions, and
/// quoted wrapper scripts. Words whose runtime value is dynamic are
/// dropped, so a returned command is a SUBSET of what will run — usable
/// only for finding danger.
fn literal_commands(script: &str) -> Vec<Vec<String>> {
    let mut walk = DangerWalk::default();
    walk.scan(&script.chars().collect::<Vec<char>>());
    walk.finish()
}

#[derive(Default)]
struct DangerWalk {
    commands: Vec<Vec<String>>,
    argv: Vec<String>,
    text: String,
    in_word: bool,
    dynamic: bool,
}

impl DangerWalk {
    fn push_char(&mut self, c: char) {
        self.text.push(c);
        self.in_word = true;
    }

    fn mark_dynamic(&mut self) {
        self.dynamic = true;
        self.in_word = true;
    }

    fn flush_word(&mut self) {
        if self.in_word {
            let word = std::mem::take(&mut self.text);
            // A word the shell rewrites is unknowable, so it is dropped
            // rather than guessed at: `$SUDO rm -f x` still shows `rm -f`.
            if !self.dynamic {
                self.argv.push(word);
            }
            self.dynamic = false;
            self.in_word = false;
        }
    }

    fn flush_command(&mut self) {
        self.flush_word();
        let argv = std::mem::take(&mut self.argv);
        if !argv.is_empty() {
            self.commands.push(argv);
        }
    }

    fn scan(&mut self, chars: &[char]) {
        let mut index = 0;
        while index < chars.len() {
            let c = chars[index];
            index += 1;
            match c {
                '\'' => {
                    self.in_word = true;
                    let (literal, next) = capture_until(chars, index, '\'');
                    self.text.push_str(&literal);
                    index = next;
                }
                '"' => {
                    self.in_word = true;
                    index = self.scan_double_quoted(chars, index);
                }
                '\\' => {
                    if let Some(&escaped) = chars.get(index) {
                        if escaped != '\n' {
                            self.push_char(escaped);
                        }
                        index += 1;
                    }
                }
                '$' => index = self.scan_expansion(chars, index),
                '`' => {
                    let (inner, next) = capture_until(chars, index, '`');
                    self.commands.extend(literal_commands(&inner));
                    self.mark_dynamic();
                    index = next;
                }
                ' ' | '\t' => self.flush_word(),
                ';' | '\n' | '&' | '|' | '(' | ')' => self.flush_command(),
                '{' | '}' if !self.in_word => self.flush_command(),
                other => self.push_char(other),
            }
        }
        self.flush_command();
    }

    /// Inside double quotes only expansions matter; everything else is
    /// literal text of the current word.
    fn scan_double_quoted(&mut self, chars: &[char], start: usize) -> usize {
        let mut index = start;
        while index < chars.len() {
            let c = chars[index];
            index += 1;
            match c {
                '"' => return index,
                '\\' => {
                    if let Some(&escaped) = chars.get(index) {
                        self.push_char(escaped);
                        index += 1;
                    }
                }
                '$' => index = self.scan_expansion(chars, index),
                '`' => {
                    let (inner, next) = capture_until(chars, index, '`');
                    self.commands.extend(literal_commands(&inner));
                    self.mark_dynamic();
                    index = next;
                }
                other => self.push_char(other),
            }
        }
        index
    }

    /// `start` points just past a `$`. Command substitutions are walked for
    /// nested commands; every form marks the enclosing word dynamic.
    fn scan_expansion(&mut self, chars: &[char], start: usize) -> usize {
        self.mark_dynamic();
        match chars.get(start) {
            Some('(') => {
                let (inner, next) = capture_balanced(chars, start, '(', ')');
                self.commands.extend(literal_commands(&inner));
                next
            }
            Some('{') => {
                let (_, next) = capture_balanced(chars, start, '{', '}');
                next
            }
            Some(_) => {
                let mut index = start + 1;
                while chars
                    .get(index)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    index += 1;
                }
                index
            }
            None => start,
        }
    }

    fn finish(mut self) -> Vec<Vec<String>> {
        self.flush_command();
        self.commands
    }
}

/// Text from `start` up to the next `end`, and the index just past it.
fn capture_until(chars: &[char], start: usize, end: char) -> (String, usize) {
    let mut index = start;
    let mut text = String::new();
    while index < chars.len() && chars[index] != end {
        text.push(chars[index]);
        index += 1;
    }
    (text, (index + 1).min(chars.len()))
}

/// Text inside a balanced `open`/`close` pair beginning at `start`, and the
/// index just past the closing delimiter.
fn capture_balanced(chars: &[char], start: usize, open: char, close: char) -> (String, usize) {
    let mut depth = 0usize;
    let mut index = start;
    let mut text = String::new();
    while index < chars.len() {
        let c = chars[index];
        index += 1;
        if c == open {
            depth += 1;
            if depth == 1 {
                continue;
            }
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return (text, index);
            }
        }
        text.push(c);
    }
    (text, index)
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
        assert_eq!(parsed[0].words[1].text, "a && b");

        let parsed = segments(r#"echo "semi;colon" 'pipe|here'"#);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].words[1].text, "semi;colon");
        assert_eq!(parsed[0].words[2].text, "pipe|here");
    }

    #[test]
    fn adjacent_quotes_join_into_one_word() {
        let parsed = segments(r#"grep "Cargo"'.toml' file"#);
        assert_eq!(parsed[0].words[1].text, "Cargo.toml");
    }

    #[test]
    fn backslash_escapes_are_literal() {
        let parsed = segments(r"echo a\;b");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].words[1].text, "a;b");
        // Line continuation disappears.
        let parsed = segments("ls \\\n-la");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].words[1].text, "-la");
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
            "ls\r\npwd",
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
    fn mid_word_hash_is_literal() {
        let parsed = segments("cat file#1");
        assert_eq!(parsed[0].words[1].text, "file#1");
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
            "nl -nrz Cargo.toml",
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
            "grep -rn 'foo$' src",
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
    fn git_read_only_subcommands_are_safe() {
        for command in [
            "git status",
            "git log -p -1",
            "git log --oneline -n 5",
            "git diff -p",
            "git show -p HEAD",
            "git branch",
            "git branch --show-current",
            "git branch --list -v",
            "git branch --format='%(refname)'",
        ] {
            assert!(safe(command), "expected safe: {command}");
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
        assert!(safe("find . -name '*.rs' | head -3"));
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
            "rm -r directory",
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
}
