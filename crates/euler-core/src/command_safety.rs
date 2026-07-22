//! Static shell-command safety analysis (issue #78).
//!
//! `run_shell` executes via `sh -c <command>`, so any reasoning about a
//! command line must reason about the *whole* line. This module decomposes a
//! command line into plain pipeline/list segments and classifies each segment
//! against a behavioral allowlist of read-only binaries with flag inspection.
//! A command is **statically safe** iff it parses into plain segments and
//! every segment is safe; statically-safe commands may run under `ask`
//! without a prompt (recorded as a `permission.decision` with mode
//! `static-safe`) and segments feed scoped-grant coverage (see
//! `docs/contracts/capabilities.md`, "Static command safety").
//!
//! ## Parser limits (deliberately conservative)
//!
//! This is a small purpose-built tokenizer, not a shell grammar. It only
//! accepts command lines made of bare words and single/double-quoted strings
//! joined by `&&`, `||`, `;`, `|`, or newlines. Quoted metacharacters are
//! literal text, never operators. Everything else makes the whole command
//! **not statically analyzable** and the caller falls back to the ask path:
//!
//! - any redirect (`>`, `<`, `>>`, `<<`, fd forms — every unquoted `>`/`<`);
//! - subshells, grouping, and brace expansion (unquoted `(`, `)`, `{`, `}`);
//! - substitution and expansion (any unquoted or double-quoted `$` or
//!   backtick — parameter expansion could rewrite the command);
//! - background execution (single unquoted `&`);
//! - comments (unquoted `#` at word start), unterminated quotes, trailing
//!   backslashes, carriage returns, and empty segments (`;;`, leading or
//!   trailing `&&`/`||`/`|`/`;`).
//!
//! False negatives (safe commands classified unsafe) only cost a prompt;
//! false positives are the failure mode this module must never have.
//!
//! Binary names match the first token exactly: `/bin/ls` or `env ls` do not
//! match `ls`. Unquoted glob characters (`*`, `?`, `[`) stay literal words
//! for the read-only binaries (their flags cannot make them write), but any
//! unquoted glob rejects the flag-inspected binaries (`find`, `rg`,
//! `base64`, `sed`, `git`) because runtime expansion could inject
//! flag-shaped tokens (a file named `-delete` in `find . *`).
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
//! - a small sensitive-basename denylist (`.env*`, `secret`/`credential`
//!   names, `id_rsa`, `id_ed25519`, `*.pem`, `*.key`) rejects even inside
//!   the workspace;
//! - argument positions are classified conservatively: only the grep/rg
//!   pattern position is exempt, and only when no `-e`/`-f`-style flag can
//!   shift it; everything else — including `--flag=value` values — is
//!   treated as a potential path.
//!
//! A rejected segment is simply not statically safe: the command falls to
//! the ordinary ask path (no new denial surface).

use std::path::Path;

/// One word of a parsed segment, quotes resolved to literal text.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Word {
    text: String,
    /// Word contained at least one unquoted glob character (`*`, `?`, `[`),
    /// so runtime expansion may replace it with arbitrary file names.
    has_unquoted_glob: bool,
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
    /// "Workspace confinement").
    pub fn is_statically_safe(&self, workspace_root: &Path) -> bool {
        self.is_read_only_invocation() && self.paths_confined(workspace_root)
    }

    /// Behavioral allowlist check: known read-only binary, flags inspected.
    fn is_read_only_invocation(&self) -> bool {
        let first = &self.words[0];
        // A glob in the command-name position expands to file names at
        // runtime; never trust it.
        if first.has_unquoted_glob {
            return false;
        }
        if READ_ONLY_BINARIES.contains(&first.text.as_str()) {
            return true;
        }
        // Flag-inspected binaries: unquoted globs anywhere reject the
        // segment because expansion could inject flag-shaped tokens.
        if self.words.iter().any(|word| word.has_unquoted_glob) {
            return false;
        }
        let args: Vec<&str> = self.words[1..]
            .iter()
            .map(|word| word.text.as_str())
            .collect();
        match first.text.as_str() {
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
            .any(|component| matches!(component, std::path::Component::ParentDir))
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

/// Names whose contents are categorically sensitive, denied even inside the
/// workspace (security review F1).
///
/// The single sensitive-name list (one list, not two): statically-safe shell
/// analysis rejects these path arguments outright, and the fs-tool permission
/// braid escalates a blanket `session-allow` to an explicit ask when a tool
/// path names one (deep review P1-b — `read_file .env` must not run
/// unprompted while `cat .env` asks).
pub fn sensitive_basename(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.starts_with(".env")
        || lower.contains("secret")
        || lower.contains("credential")
        || lower == "id_rsa"
        || lower == "id_ed25519"
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
}

/// Decompose a command line into plain segments across `&&`, `||`, `;`,
/// `|`, and newlines. Returns `None` when the line is not statically
/// analyzable (see the module docs for the full rejection list).
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
            // literal (`file#1`).
            '#' if !builder.in_word => return None,
            '>' | '<' | '(' | ')' | '{' | '}' | '`' | '$' | '\r' => return None,
            glob @ ('*' | '?' | '[') => builder.push_glob_char(glob),
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
    glob: bool,
    in_word: bool,
}

impl SegmentBuilder {
    fn push_char(&mut self, c: char) {
        self.text.push(c);
        self.in_word = true;
    }

    fn push_glob_char(&mut self, c: char) {
        self.push_char(c);
        self.glob = true;
    }

    fn flush_word(&mut self) {
        if self.in_word {
            self.words.push(Word {
                text: std::mem::take(&mut self.text),
                has_unquoted_glob: self.glob,
            });
            self.glob = false;
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
pub fn is_statically_safe_command(command: &str, workspace_root: &Path) -> bool {
    parse_plain_segments(command).is_some_and(|segments| {
        segments
            .iter()
            .all(|segment| segment.is_statically_safe(workspace_root))
    })
}

/// Read-only regardless of flags: nothing these binaries accept makes them
/// write, execute other programs, or mutate state beyond the shell process.
const READ_ONLY_BINARIES: &[&str] = &[
    "cat", "cd", "cut", "echo", "expr", "false", "grep", "head", "id", "ls", "nl", "paste", "pwd",
    "rev", "seq", "stat", "tail", "tr", "true", "uname", "uniq", "wc", "which", "whoami",
];

fn is_safe_find(args: &[&str]) -> bool {
    // Actions that execute commands, delete files, or write pathnames.
    const UNSAFE_FIND_ARGS: &[&str] = &[
        "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fls", "-fprint", "-fprint0", "-fprintf",
    ];
    !args.iter().any(|arg| UNSAFE_FIND_ARGS.contains(arg))
}

fn is_safe_rg(args: &[&str]) -> bool {
    // --pre / --hostname-bin execute external commands; --search-zip / -z
    // shell out to decompression tools. Short flags may be bundled
    // (`-zn`), so any single-dash cluster containing `z` rejects — a
    // false-unsafe on flag values (`-ezoo`) only costs a prompt.
    const UNSAFE_RG_VALUE_FLAGS: &[&str] = &["--pre", "--hostname-bin"];
    !args.iter().any(|arg| {
        *arg == "--search-zip"
            || UNSAFE_RG_VALUE_FLAGS
                .iter()
                .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))
            || arg
                .strip_prefix('-')
                .is_some_and(|rest| !rest.starts_with('-') && rest.contains('z'))
    })
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
            "grep -R Cargo.toml -n",
            "head -n 50 src/lib.rs",
            "wc -l file",
            "which cargo",
            "nl -nrz Cargo.toml",
            "echo hello world",
            "true",
            "cd src",
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
            "grep -R Cargo.toml -n || true",
            "ls && pwd",
            "echo hi ; ls",
            "cd src && ls\nwc -l lib.rs",
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
    fn glob_rules_differ_by_binary_class() {
        // Read-only binaries stay read-only whatever expansion produces.
        assert!(safe("ls *.rs"));
        assert!(safe("wc -l src/*.rs"));
        // Flag-inspected binaries reject unquoted globs: expansion could
        // inject flag-shaped tokens (a file literally named `-delete`).
        assert!(!safe("find . -name *.rs"));
        assert!(!safe("rg pattern *"));
        assert!(!safe("git status *"));
        // Quoted globs are literal text.
        assert!(safe("find . -name '*.rs'"));
        assert!(safe("rg pattern \"*.rs\""));
        // A glob in the command-name position never matches anything.
        assert!(!safe("l? -la"));
    }

    #[test]
    fn safe_flag_rules_hold_inside_pipelines() {
        assert!(safe("find . -name '*.rs' | head -3"));
        assert!(!safe("find . -delete | head -3"));
        assert!(!safe("ls | base64 -o out"));
    }
}
