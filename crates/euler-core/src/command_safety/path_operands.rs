//! The paths a shell command names in a recognized position, as `path` and
//! `outside_workspace` permission rules read them (ADR 0027 resolved
//! question 3). Writes are redirection targets and the write positions of a
//! short table of commands; reads are their other operands and the operands
//! of a short list of readers; operands of any other command have unknown
//! direction. This is not a shell effects interpreter: a program that
//! writes a path it was not given in a recognized position is not seen.
//!
//! A word the shell expands (a variable, a substitution, a glob) is read as
//! far as its fixed text goes (resolved question 3, amended 2026-11-06): a
//! leading `~` is `HOME`, and the directory written before the first
//! expansion bounds the paths the word can name.

use super::edits_in_place;
use tree_sitter::Node;

/// The direction in which a request uses a path. `Unknown` matches `read`,
/// `write`, and `any` rules alike.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathAccess {
    Read,
    Write,
    Unknown,
}

/// What Euler knows statically about one path a request names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetPath {
    /// The path as written: workspace-relative or absolute, with a shell
    /// `~` already expanded to `HOME`.
    Literal(String),
    /// A word the shell expands at run time (a variable, a substitution, a
    /// glob) that names only paths under this directory, as written (`""`
    /// is the workspace root). A value containing `..` is not seen.
    Under(String),
    /// A word that may name any path: one that starts with an expansion, a
    /// dynamic command name, an unreadable wrapper, or a line that does not
    /// parse.
    Anywhere,
}

/// One path a request names and the direction in which it uses it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathTarget {
    pub path: TargetPath,
    pub access: PathAccess,
}

impl PathTarget {
    pub fn new(path: impl Into<String>, access: PathAccess) -> Self {
        Self {
            path: TargetPath::Literal(path.into()),
            access,
        }
    }

    pub(super) fn unresolved() -> Self {
        Self {
            path: TargetPath::Anywhere,
            access: PathAccess::Unknown,
        }
    }
}

/// Stands in, in the text [`operand_word`] reads, for a shell expansion: a
/// variable, a substitution, or a `~` that is not `HOME`. The text it
/// stands for may be anything, an absolute path too.
const EXPANSION: char = '\0';
/// Stands in for a glob character: the shell matches names, so the word
/// stays under the directory written before it.
const GLOB: char = '\u{1}';
/// Stand in for an unquoted `{` and `}`: brace expansion is static text,
/// so a brace whose body has no `.` or `/` cannot climb out of the
/// directory written before it.
const BRACE_OPEN: char = '\u{2}';
const BRACE_CLOSE: char = '\u{3}';
/// Stands in for quoted text Euler does not decode (`$'\x2e'`): it may be
/// any path, and may climb out of any directory.
const OPAQUE: char = '\u{4}';
const DYNAMIC: [char; 5] = [EXPANSION, GLOB, BRACE_OPEN, BRACE_CLOSE, OPAQUE];

/// The text of one shell word as the path parse reads it: literal parts as
/// the program receives them, a leading unquoted `~` or `~/` replaced by
/// `HOME`, unquoted escapes removed, and each expansion, unquoted glob
/// character, or unquoted brace replaced by [`EXPANSION`], [`GLOB`],
/// [`BRACE_OPEN`], or [`BRACE_CLOSE`], and `$'...'` text with an escape
/// replaced by [`OPAQUE`].
pub(super) fn operand_word(node: Node<'_>, src: &str) -> String {
    let mut text = String::new();
    push_word(node, src, true, &mut text);
    text
}

fn push_word(node: Node<'_>, src: &str, first: bool, text: &mut String) {
    let Ok(raw) = node.utf8_text(src.as_bytes()) else {
        text.push(EXPANSION);
        return;
    };
    match node.kind() {
        "word" | "number" if node.named_child_count() == 0 => push_unquoted(raw, first, text),
        "raw_string" => match raw
            .strip_prefix('\'')
            .and_then(|raw| raw.strip_suffix('\''))
        {
            Some(content) => text.push_str(content),
            None => text.push(EXPANSION),
        },
        "ansi_c_string" => match raw
            .strip_prefix("$'")
            .and_then(|raw| raw.strip_suffix('\''))
        {
            Some(content) if !content.contains('\\') => text.push_str(content),
            _ => text.push(OPAQUE),
        },
        // A sequence such as `{1..3}` (zsh also reads `{-../}`).
        "brace_expression" => push_unquoted(raw, false, text),
        "string" => push_double_quoted(node, src, text),
        "concatenation" => {
            let mut cursor = node.walk();
            let mut end = node.start_byte();
            for part in node.named_children(&mut cursor) {
                if part.start_byte() != end {
                    // Text no named part covers: read it as an expansion.
                    text.push(EXPANSION);
                }
                push_word(part, src, first && end == node.start_byte(), text);
                end = part.end_byte();
            }
            if end != node.end_byte() {
                text.push(EXPANSION);
            }
        }
        _ => text.push(EXPANSION),
    }
}

/// An unquoted word: `~` and `~/` at the start of the whole word are
/// `HOME` (`~user` is not read), `=` there is zsh equals expansion, an
/// escaped character is literal, and the shell may rewrite globs, braces,
/// and later `~`s.
fn push_unquoted(raw: &str, first: bool, text: &mut String) {
    let mut rest = raw;
    if first {
        if let Some(after) = rest.strip_prefix('~') {
            match std::env::var("HOME") {
                Ok(home) if !home.is_empty() && (after.is_empty() || after.starts_with('/')) => {
                    text.push_str(&home);
                    rest = after;
                }
                _ => {
                    text.push(EXPANSION);
                    return;
                }
            }
        } else if rest.starts_with('=') {
            text.push(EXPANSION);
            return;
        }
    }
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        text.push(match c {
            // Escape removal is static: `\.\.` is a literal `..`.
            '\\' => match chars.next() {
                Some(escaped) if escaped != '\n' => escaped,
                _ => EXPANSION,
            },
            '*' | '?' | '[' | ']' | '^' | '#' => GLOB,
            '{' => BRACE_OPEN,
            '}' => BRACE_CLOSE,
            '$' | '`' | '~' => EXPANSION,
            c => c,
        });
    }
}

/// A double-quoted string: its literal content, with each expansion (and
/// any `$`, backquote, or escape the parse left in the content) replaced by
/// [`EXPANSION`]. Quotes suppress globbing.
fn push_double_quoted(node: Node<'_>, src: &str, text: &mut String) {
    let literal = |content: &str, text: &mut String| {
        text.extend(content.chars().map(|c| match c {
            '$' | '`' | '\\' => EXPANSION,
            c => c,
        }));
    };
    let (start, end) = (node.start_byte() + 1, node.end_byte().saturating_sub(1));
    if start > end || src.get(start - 1..start) != Some("\"") || src.get(end..=end) != Some("\"") {
        text.push(EXPANSION);
        return;
    }
    let mut at = start;
    let mut cursor = node.walk();
    for part in node.named_children(&mut cursor) {
        literal(src.get(at..part.start_byte()).unwrap_or_default(), text);
        if part.kind() == "string_content" {
            literal(part.utf8_text(src.as_bytes()).unwrap_or_default(), text);
        } else {
            text.push(EXPANSION);
        }
        at = part.end_byte();
    }
    literal(src.get(at..end).unwrap_or_default(), text);
}

/// Whether an [`operand_word`] text has an expansion or a brace (not only
/// globs).
fn has_expansion(word: &str) -> bool {
    word.contains([EXPANSION, BRACE_OPEN, BRACE_CLOSE, OPAQUE])
}

/// Whether an [`operand_word`] text has an expansion, a glob, or a brace.
pub(super) fn is_dynamic(word: &str) -> bool {
    word.contains(DYNAMIC)
}

/// The target an [`operand_word`] text names with `access`. A word with no
/// expansion, glob, or brace is a literal path. Any other word is bounded
/// by the directory written before its first such part, unless it starts
/// with an expansion or a brace (`{~,x}` and `{/etc,x}` name any path), has
/// [`OPAQUE`] text, or a later part [`climbs_out`] of that directory.
fn operand_target(word: &str, access: PathAccess) -> PathTarget {
    let Some(first_dynamic) = word.find(DYNAMIC) else {
        return PathTarget::new(word, access);
    };
    let dir_len = word[..first_dynamic]
        .rfind('/')
        .map_or(0, |slash| slash + 1);
    let path = if word.starts_with([EXPANSION, BRACE_OPEN])
        || word.contains(OPAQUE)
        || climbs_out(&word[dir_len..])
    {
        TargetPath::Anywhere
    } else {
        TargetPath::Under(word[..dir_len].to_owned())
    };
    PathTarget { path, access }
}

/// Whether the part of an [`operand_word`] text after its fixed directory
/// can yield a `..` component. A brace body with a `.` or `/` can. Its
/// other literal text cannot help form `..`, so each component is judged by
/// its text outside braces plus the expansions and globs inside them: it
/// climbs when that text starts with `..`, starts with `.` beside a glob
/// (`.*` matches `..`) or an expansion (`.$D` with `D=.`), or is only dots
/// and expansions (`$D.`). A `..` inside a variable's value is not seen.
fn climbs_out(rest: &str) -> bool {
    let climbs = |outside: &str| {
        outside.starts_with("..")
            || (outside.starts_with('.') && outside.contains([GLOB, EXPANSION]))
            || (outside.contains('.')
                && outside.contains(EXPANSION)
                && outside.chars().all(|c| matches!(c, '.' | EXPANSION)))
    };
    let mut depth = 0usize;
    let mut outside = String::new();
    for c in rest.chars().chain(['/']) {
        match c {
            BRACE_OPEN => depth += 1,
            BRACE_CLOSE => depth = depth.saturating_sub(1),
            '.' | '/' if depth > 0 => return true,
            '/' => {
                if climbs(&outside) {
                    return true;
                }
                outside.clear();
            }
            EXPANSION | GLOB => outside.push(c),
            _ if depth > 0 => {}
            c => outside.push(c),
        }
    }
    false
}

/// The targets of one `file_redirect` node. Its first destination word is
/// the redirect target: `<` reads, the `>` family writes, and a descriptor
/// duplication or close (`2>&1`, `>&-`) names no path. `tree-sitter-bash`
/// also hangs every later word of the command on the redirect as further
/// destinations (`cat > out /etc/shadow`); those are operands of the
/// command, so each is its own target of unknown direction. A dynamic word
/// is read as [`operand_target`] reads it.
pub(super) fn redirect_targets(node: Node<'_>, src: &str) -> Vec<PathTarget> {
    let mut cursor = node.walk();
    let Some(operator) = node
        .children(&mut cursor)
        .find(|child| !child.is_named())
        .map(|child| child.kind())
    else {
        return Vec::new();
    };
    let mut cursor = node.walk();
    let mut words = node
        .children_by_field_name("destination", &mut cursor)
        .map(|word| operand_word(word, src));
    let mut targets = Vec::new();
    if let Some(destination) = words.next() {
        let duplicates_descriptor = matches!(operator, "<&" | ">&")
            && (destination == "-" || destination.bytes().all(|byte| byte.is_ascii_digit()));
        if !destination.is_empty() && !duplicates_descriptor && !operator.ends_with("&-") {
            let access = if operator.starts_with('<') {
                PathAccess::Read
            } else {
                PathAccess::Write
            };
            targets.push(operand_target(&destination, access));
        }
    }
    targets.extend(words.map(|word| operand_target(&word, PathAccess::Unknown)));
    targets
}

/// The path operands of one command whose name is literal (`words[0]`),
/// each word a literal or an [`operand_word`] text.
pub(super) fn command_targets(words: &[String]) -> Vec<PathTarget> {
    let Some((name, args)) = words.split_first() else {
        return Vec::new();
    };
    let name = name
        .rsplit_once('/')
        .map_or(name.as_str(), |(_, base)| base)
        .to_ascii_lowercase();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let all = |access| {
        operands(&args)
            .into_iter()
            .map(|operand| operand_target(operand, access))
            .collect()
    };
    match name.as_str() {
        "cat" | "ls" | "head" | "tail" | "grep" | "rg" | "wc" | "stat" | "diff" => {
            all(PathAccess::Read)
        }
        "mv" | "tee" | "touch" | "mkdir" | "rm" | "mktemp" | "patch" | "ed" => {
            all(PathAccess::Write)
        }
        // `install -d` creates every operand as a directory.
        "install" if args.iter().any(|arg| creates_directories(arg)) => all(PathAccess::Write),
        "cp" | "install" | "ln" => copy_targets(&args, true),
        "rsync" => copy_targets(&args, false),
        "sed" | "perl" | "ruby" if edits_in_place(&args) => all(PathAccess::Write),
        "awk" | "gawk" if args.iter().any(|arg| arg.contains("inplace")) => all(PathAccess::Write),
        "sed" | "perl" | "ruby" | "awk" | "gawk" => all(PathAccess::Read),
        "dd" => args
            .iter()
            .filter_map(|arg| {
                if let Some(path) = arg.strip_prefix("of=") {
                    Some(operand_target(path, PathAccess::Write))
                } else {
                    arg.strip_prefix("if=")
                        .map(|path| operand_target(path, PathAccess::Read))
                }
            })
            .filter(|target| target.path != TargetPath::Literal(String::new()))
            .collect(),
        "curl" => output_targets(&args, 'o', "--output"),
        "wget" => output_targets(&args, 'O', "--output-document"),
        _ => unknown_targets(&args),
    }
}

/// The operands of a command whose direction is unknown. A word with an
/// expansion names a path only where it looks like one (it contains `/` or
/// starts with `.`; a leading `~` already reads as `HOME`, which contains
/// `/`) and is not the value of the option before it,
/// so `echo $HOME`, `git commit -m "$MSG"`, and `python x.py $OUT` name no
/// path (resolved question 3, amended 2026-11-06).
fn unknown_targets(args: &[&str]) -> Vec<PathTarget> {
    let mut targets = Vec::new();
    let mut options_end = false;
    let mut after_option = false;
    for &arg in args {
        if !options_end && arg.starts_with('-') {
            options_end = arg == "--";
            // `--title=$X` carries its own value.
            after_option = arg.len() > 1 && !options_end && !arg.contains('=');
            continue;
        }
        let looks_like_path = arg.contains('/') || arg.starts_with('.');
        if !has_expansion(arg) || (looks_like_path && !after_option) {
            targets.push(operand_target(arg, PathAccess::Unknown));
        }
        after_option = false;
    }
    targets
}

/// The operands of `args`: every word that is not an option, and every word
/// after `--`. A bare `-` is standard input, not a path. An option's
/// separate value (`-m 755`) counts as an operand, since which options take
/// one is not known here; that only adds targets.
fn operands<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut operands = Vec::new();
    let mut options_end = false;
    for &arg in args {
        if !options_end && arg == "--" {
            options_end = true;
        } else if options_end || !arg.starts_with('-') {
            operands.push(arg);
        }
    }
    operands
}

/// `cp`, `install`, `ln`, and `rsync` write their destination and read the
/// other operands. The destination is the last operand, or the value of
/// `-t`/`--target-directory` where `target_option` (not for `rsync`, whose
/// `-t` keeps times). A lone operand has unknown direction (`ln -s target`
/// writes a link named after it). An option after an operand makes every
/// target's direction unknown: GNU tools read it as an option, whose value
/// may then look like the last operand (`cp a dst -S .bak`), while BSD
/// tools read it as an operand.
fn copy_targets(args: &[&str], target_option: bool) -> Vec<PathTarget> {
    let mut target = None;
    let mut rest = Vec::new();
    let mut index = 0;
    let mut options_end = false;
    let mut option_after_operand = false;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if options_end || arg == "-" || !arg.starts_with('-') {
            rest.push(arg);
            continue;
        }
        if arg == "--" {
            options_end = true;
            continue;
        }
        option_after_operand |= !rest.is_empty();
        if target_option {
            if let Some(value) = option_value(args, &mut index, arg, 't', "--target-directory") {
                target = Some(value);
            }
        }
    }
    let read = |operand: &&str| operand_target(operand, PathAccess::Read);
    let targets: Vec<PathTarget> = match (target, rest.as_slice()) {
        (Some(target), _) => std::iter::once(operand_target(target, PathAccess::Write))
            .chain(rest.iter().map(read))
            .collect(),
        (None, [only]) => vec![operand_target(only, PathAccess::Unknown)],
        (None, [sources @ .., destination]) => sources
            .iter()
            .map(read)
            .chain(std::iter::once(operand_target(
                destination,
                PathAccess::Write,
            )))
            .collect(),
        (None, []) => Vec::new(),
    };
    if !option_after_operand {
        return targets;
    }
    targets
        .into_iter()
        .map(|target| PathTarget {
            access: PathAccess::Unknown,
            ..target
        })
        .collect()
}

/// `-d` or `--directory`, alone or in a short cluster.
fn creates_directories(arg: &str) -> bool {
    arg == "--directory"
        || arg
            .strip_prefix('-')
            .is_some_and(|cluster| !cluster.starts_with('-') && cluster.contains('d'))
}

/// `curl -o FILE` and `wget -O FILE` write FILE (`-oFILE`, `-sSo FILE`,
/// `--output FILE`, `--output=FILE` too); their other operands are reads.
fn output_targets(args: &[&str], short: char, long: &str) -> Vec<PathTarget> {
    let mut targets = Vec::new();
    let mut index = 0;
    let mut options_end = false;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if options_end || arg == "-" || !arg.starts_with('-') {
            targets.push(operand_target(arg, PathAccess::Read));
        } else if arg == "--" {
            options_end = true;
        } else if let Some(value) = option_value(args, &mut index, arg, short, long) {
            targets.push(operand_target(value, PathAccess::Write));
        }
    }
    targets
}

/// The value of option `arg` when it is `short` or `long`: attached
/// (`-oFILE`, `--output=FILE`), or the next word (`-o FILE`, `-sSo FILE`,
/// `--output FILE`), which `index` then steps past. `short` may end a
/// cluster of other letters.
fn option_value<'a>(
    args: &[&'a str],
    index: &mut usize,
    arg: &'a str,
    short: char,
    long: &str,
) -> Option<&'a str> {
    let attached = match arg.strip_prefix(long) {
        Some("") => "",
        Some(rest) => rest.strip_prefix('=')?,
        None => {
            let cluster = arg
                .strip_prefix('-')
                .filter(|rest| !rest.starts_with('-'))?;
            &cluster[cluster.find(short)? + short.len_utf8()..]
        }
    };
    if !attached.is_empty() {
        return Some(attached);
    }
    let next = args.get(*index).copied()?;
    *index += 1;
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::super::shell_path_targets;
    use super::{PathAccess, PathTarget, TargetPath};

    type Target = (TargetPath, PathAccess);

    fn targets(command: &str) -> Vec<Target> {
        shell_path_targets(command)
            .into_iter()
            .map(|PathTarget { path, access }| (path, access))
            .collect()
    }

    fn read(path: &str) -> Target {
        (TargetPath::Literal(path.to_owned()), PathAccess::Read)
    }

    fn write(path: &str) -> Target {
        (TargetPath::Literal(path.to_owned()), PathAccess::Write)
    }

    fn unknown(path: &str) -> Target {
        (TargetPath::Literal(path.to_owned()), PathAccess::Unknown)
    }

    fn under(dir: &str, access: PathAccess) -> Target {
        (TargetPath::Under(dir.to_owned()), access)
    }

    fn anywhere(access: PathAccess) -> Target {
        (TargetPath::Anywhere, access)
    }

    const UNRESOLVED: Target = (TargetPath::Anywhere, PathAccess::Unknown);

    /// Resolved question 3: the write positions, their reads, the readers,
    /// and unknown direction for any other command.
    #[test]
    fn ordinary_writers_readers_and_other_commands() {
        let cases: Vec<(&str, Vec<Target>)> = vec![
            ("cp a b dir/", vec![read("a"), read("b"), write("dir/")]),
            ("cp -t dir/ a b", vec![write("dir/"), read("a"), read("b")]),
            ("cp -rtdir a", vec![write("dir"), read("a")]),
            (
                "install -m 755 a /opt/a",
                vec![read("755"), read("a"), write("/opt/a")],
            ),
            (
                "install -d /opt/x /opt/y",
                vec![write("/opt/x"), write("/opt/y")],
            ),
            ("ln -s target link", vec![read("target"), write("link")]),
            ("ln -s target", vec![unknown("target")]),
            // An option after an operand: GNU reads `-S .bak` as an option
            // and BSD as operands, so no direction is known.
            (
                "cp a /x/profiles.json -S .bak",
                vec![unknown("a"), unknown("/x/profiles.json"), unknown(".bak")],
            ),
            (
                "install a /opt/a -m 755",
                vec![unknown("a"), unknown("/opt/a"), unknown("755")],
            ),
            (
                "ln -s target link --suffix=.old",
                vec![unknown("target"), unknown("link")],
            ),
            (
                "rsync -a src/ dst/ -e ssh",
                vec![unknown("src/"), unknown("dst/"), unknown("ssh")],
            ),
            ("cp -- a -S", vec![read("a"), write("-S")]),
            ("cp a -- b", vec![read("a"), write("b")]),
            (
                "rsync -t src/ /backup/",
                vec![read("src/"), write("/backup/")],
            ),
            ("mv old new", vec![write("old"), write("new")]),
            (
                "tee /opt/result /tmp/result",
                vec![write("/opt/result"), write("/tmp/result")],
            ),
            (
                "touch a && mkdir -p b && rm -f c",
                vec![write("a"), write("b"), write("c")],
            ),
            ("mktemp -p /tmp", vec![write("/tmp")]),
            ("sed -i s/a/b/ f", vec![write("s/a/b/"), write("f")]),
            ("sed s/a/b/ f", vec![read("s/a/b/"), read("f")]),
            ("perl -pi -e s/a/b/ f", vec![write("s/a/b/"), write("f")]),
            (
                "awk -i inplace 1 f",
                vec![write("inplace"), write("1"), write("f")],
            ),
            ("patch f d.diff", vec![write("f"), write("d.diff")]),
            ("ed f", vec![write("f")]),
            (
                "dd if=/dev/zero of=disk.img bs=1M",
                vec![read("/dev/zero"), write("disk.img")],
            ),
            ("dd of= if=x", vec![read("x")]),
            (
                "curl -sSo out.html https://example.com",
                vec![write("out.html"), read("https://example.com")],
            ),
            (
                "curl --output=x https://example.com",
                vec![write("x"), read("https://example.com")],
            ),
            (
                "wget -O x https://example.com",
                vec![write("x"), read("https://example.com")],
            ),
            ("cat a - b", vec![read("a"), read("b")]),
            ("grep -r needle src", vec![read("needle"), read("src")]),
            (
                "python3 x.py /etc/hosts",
                vec![unknown("x.py"), unknown("/etc/hosts")],
            ),
            (
                "git -C /other status",
                vec![unknown("/other"), unknown("status")],
            ),
            ("ls -- -weird", vec![read("-weird")]),
        ];
        for (command, expected) in cases {
            assert_eq!(targets(command), expected, "{command}");
        }
    }

    /// Redirection targets are writes (`<` reads); descriptor duplication
    /// names no path; a wrapper's command and a substitution are walked; a
    /// dynamic operand or unreadable line is one unresolved target.
    #[test]
    fn redirects_wrappers_and_unresolved_operands() {
        let cases: Vec<(&str, Vec<Target>)> = vec![
            (
                "echo hi > out.txt 2>&1",
                vec![unknown("hi"), write("out.txt")],
            ),
            ("sort < in.txt >> log", vec![read("in.txt"), write("log")]),
            ("> /tmp/x", vec![write("/tmp/x")]),
            // Later words hang on the redirect but are command operands.
            (
                "echo x > .env foo",
                vec![unknown("x"), write(".env"), unknown("foo")],
            ),
            (
                "cat > out /etc/shadow",
                vec![write("out"), unknown("/etc/shadow")],
            ),
            ("cat < a b", vec![read("a"), unknown("b")]),
            (
                "echo x > /home/me/.bashrc /../../../tmp/y",
                vec![
                    unknown("x"),
                    write("/home/me/.bashrc"),
                    unknown("/../../../tmp/y"),
                ],
            ),
            ("echo x 2>&1 foo", vec![unknown("x"), unknown("foo")]),
            (
                "echo x > out $Y",
                vec![unknown("x"), write("out"), anywhere(PathAccess::Unknown)],
            ),
            ("sudo cp a /etc/x", vec![read("a"), write("/etc/x")]),
            ("bash -c 'tee /opt/y'", vec![write("/opt/y")]),
            // `xargs` adds operands from standard input.
            ("echo /x | xargs rm", vec![unknown("/x"), UNRESOLVED]),
            ("xargs -0 rm -f a", vec![UNRESOLVED, write("a")]),
            ("ls | xargs", vec![]),
            (
                "ls | xargs -n 1 cp -t /dst",
                vec![UNRESOLVED, write("/dst")],
            ),
            ("echo $(cat secret)", vec![read("secret")]),
            ("cat $FILE", vec![anywhere(PathAccess::Read)]),
            ("rm *.o", vec![under("", PathAccess::Write)]),
            (
                "echo x > \"$OUT\"",
                vec![unknown("x"), anywhere(PathAccess::Write)],
            ),
            ("bash -s", vec![UNRESOLVED]),
            ("cat 'unterminated", vec![UNRESOLVED]),
            ("export A=1 && cargo test", vec![unknown("test")]),
        ];
        for (command, expected) in cases {
            assert_eq!(targets(command), expected, "{command}");
        }
    }

    /// Resolved question 3, amended 2026-11-06: a leading `~` is `HOME`; a
    /// glob or an expansion is bounded by the directory written before it,
    /// or may be any path when it starts the word or a later part climbs
    /// out; recognized positions read every dynamic word, while a command of
    /// unknown direction reads one only where it looks like a path and is
    /// not an option's value; a dynamic command name or wrapper stays one
    /// target that may be any path.
    #[test]
    fn dynamic_words_are_bounded_by_their_fixed_text() {
        let home = std::env::var("HOME").expect("HOME");
        let read_home = |rest: &str| read(&format!("{home}{rest}"));
        let cases: Vec<(&str, Vec<Target>)> = vec![
            ("cat ~/notes.txt", vec![read_home("/notes.txt")]),
            ("ls ~", vec![read_home("")]),
            (
                "cat '~/x' ~user/x",
                vec![read("~/x"), anywhere(PathAccess::Read)],
            ),
            (
                "cp x ~/.ssh/",
                vec![read("x"), write(&format!("{home}/.ssh/"))],
            ),
            ("cat \"$HOME\"/.ssh/*", vec![anywhere(PathAccess::Read)]),
            ("ls src/*.rs", vec![under("src/", PathAccess::Read)]),
            ("ls *.rs", vec![under("", PathAccess::Read)]),
            (
                "cat ~/.ssh/*",
                vec![under(&format!("{home}/.ssh/"), PathAccess::Read)],
            ),
            ("cat src/$X/y", vec![under("src/", PathAccess::Read)]),
            ("cat /etc/\"$X\"", vec![under("/etc/", PathAccess::Read)]),
            ("cat src/*/../../x", vec![anywhere(PathAccess::Read)]),
            ("cat src/.*/x", vec![anywhere(PathAccess::Read)]),
            ("cat {/etc/passwd,x}", vec![anywhere(PathAccess::Read)]),
            // A brace or an escape that can form `..` climbs out; one that
            // cannot stays under the fixed directory.
            ("cat ~/x/{..,y}/.ssh/id", vec![anywhere(PathAccess::Read)]),
            ("cat src/{a/..,b}/x", vec![anywhere(PathAccess::Read)]),
            ("cat src/{x,}../y", vec![anywhere(PathAccess::Read)]),
            ("cat src/.{x,}.", vec![anywhere(PathAccess::Read)]),
            ("cat src/.{*,x}", vec![anywhere(PathAccess::Read)]),
            ("cat src/{x,$D}.", vec![anywhere(PathAccess::Read)]),
            ("cat {~,x}/.ssh/id", vec![anywhere(PathAccess::Read)]),
            ("cat src/{a,b}.rs", vec![under("src/", PathAccess::Read)]),
            ("cat src/{1..3}/x", vec![anywhere(PathAccess::Read)]),
            ("cat src/$'..'/x", vec![read("src/../x")]),
            ("cat src/$'\\x2e\\x2e'/x", vec![anywhere(PathAccess::Read)]),
            ("cat src/{a,b}/./x", vec![under("src/", PathAccess::Read)]),
            ("cat ~/x/\\.\\./.ssh/id", vec![read_home("/x/../.ssh/id")]),
            ("cat ~/x/.\\./.ssh/id", vec![read_home("/x/../.ssh/id")]),
            ("cat src/\\*.rs", vec![read("src/*.rs")]),
            ("cat \\~/x", vec![read("~/x")]),
            // A fixed `.` beside a variable may form `..`.
            ("cat ~/x/.$D/.ssh/id", vec![anywhere(PathAccess::Read)]),
            ("cat src/$D./y", vec![anywhere(PathAccess::Read)]),
            ("cat src/$NAME.rs", vec![under("src/", PathAccess::Read)]),
            ("rm -rf $DIR", vec![anywhere(PathAccess::Write)]),
            (
                "echo hi > $OUT",
                vec![unknown("hi"), anywhere(PathAccess::Write)],
            ),
            (
                "curl -o \"$OUT\" https://example.com",
                vec![anywhere(PathAccess::Write), read("https://example.com")],
            ),
            (
                "dd if=$IN of=out.img",
                vec![anywhere(PathAccess::Read), write("out.img")],
            ),
            // Unknown direction: a bare expansion or an option's value
            // names no path; one that looks like a path does.
            ("echo $HOME", vec![]),
            ("python x.py $OUT", vec![unknown("x.py")]),
            ("git commit -m \"$MSG\"", vec![unknown("commit")]),
            (
                "gh pr create --title=$X --body \"$B/x\"",
                vec![unknown("pr"), unknown("create")],
            ),
            (
                "python x.py \"$DIR\"/out ./$X",
                vec![
                    unknown("x.py"),
                    anywhere(PathAccess::Unknown),
                    under("./", PathAccess::Unknown),
                ],
            ),
            (
                "python x.py -- -$X/y",
                vec![unknown("x.py"), under("", PathAccess::Unknown)],
            ),
            (
                "cargo fmt -- *.rs",
                vec![unknown("fmt"), under("", PathAccess::Unknown)],
            ),
            // A dynamic command name or a wrapper with a dynamic word.
            ("$EDITOR notes.txt", vec![UNRESOLVED]),
            ("sudo cat $F", vec![UNRESOLVED]),
            ("xargs rm $F", vec![UNRESOLVED]),
        ];
        for (command, expected) in cases {
            assert_eq!(targets(command), expected, "{command}");
        }
    }
}
