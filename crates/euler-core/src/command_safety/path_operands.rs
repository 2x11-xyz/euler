//! The paths a shell command names in a recognized position, as `path` and
//! `outside_workspace` permission rules read them (ADR 0027 resolved
//! question 3). Writes are redirection targets and the write positions of a
//! short table of commands; reads are their other operands and the operands
//! of a short list of readers; operands of any other command have unknown
//! direction. This is not a shell effects interpreter: a program that
//! writes a path it was not given in a recognized position is not seen.

use super::{edits_in_place, literal_word};
use tree_sitter::Node;

/// The direction in which a request uses a path. `Unknown` matches `read`,
/// `write`, and `any` rules alike.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathAccess {
    Read,
    Write,
    Unknown,
}

/// One path a request names. `path` is as written (workspace-relative or
/// absolute), or `None` when Euler cannot resolve it statically: a dynamic
/// word, an unreadable wrapper, or a line that does not parse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathTarget {
    pub path: Option<String>,
    pub access: PathAccess,
}

impl PathTarget {
    pub fn new(path: impl Into<String>, access: PathAccess) -> Self {
        Self {
            path: Some(path.into()),
            access,
        }
    }

    pub(super) fn unresolved() -> Self {
        Self {
            path: None,
            access: PathAccess::Unknown,
        }
    }
}

/// The target of one `file_redirect` node: `<` reads, the `>` family
/// writes, and a descriptor duplication or close (`2>&1`, `>&-`) names no
/// path. A dynamic target is unresolved.
pub(super) fn redirect_target(node: Node<'_>, src: &str) -> Option<PathTarget> {
    let mut cursor = node.walk();
    let operator = node
        .children(&mut cursor)
        .find(|child| !child.is_named())
        .map(|child| child.kind())?;
    let mut cursor = node.walk();
    let destination = node
        .children_by_field_name("destination", &mut cursor)
        .map(|word| literal_word(word, src))
        .collect::<Option<Vec<String>>>();
    let Some(destination) = destination else {
        return Some(PathTarget::unresolved());
    };
    let destination = destination.concat();
    let duplicates_descriptor = matches!(operator, "<&" | ">&")
        && (destination == "-" || destination.bytes().all(|byte| byte.is_ascii_digit()));
    if destination.is_empty() || duplicates_descriptor || operator.ends_with("&-") {
        return None;
    }
    let access = if operator.starts_with('<') {
        PathAccess::Read
    } else {
        PathAccess::Write
    };
    Some(PathTarget::new(destination, access))
}

/// The path operands of one literal command (`words[0]` is its name).
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
            .map(|operand| PathTarget::new(operand, access))
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
                    Some(PathTarget::new(path, PathAccess::Write))
                } else {
                    arg.strip_prefix("if=")
                        .map(|path| PathTarget::new(path, PathAccess::Read))
                }
            })
            .filter(|target| target.path.as_deref() != Some(""))
            .collect(),
        "curl" => output_targets(&args, 'o', "--output"),
        "wget" => output_targets(&args, 'O', "--output-document"),
        _ => all(PathAccess::Unknown),
    }
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
/// writes a link named after it).
fn copy_targets(args: &[&str], target_option: bool) -> Vec<PathTarget> {
    let mut target = None;
    let mut rest = Vec::new();
    let mut index = 0;
    let mut options_end = false;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if options_end || arg == "-" || !arg.starts_with('-') {
            rest.push(arg);
        } else if arg == "--" {
            options_end = true;
        } else if target_option {
            if let Some(value) = option_value(args, &mut index, arg, 't', "--target-directory") {
                target = Some(value);
            }
        }
    }
    let read = |operand: &&str| PathTarget::new(*operand, PathAccess::Read);
    match (target, rest.as_slice()) {
        (Some(target), _) => std::iter::once(PathTarget::new(target, PathAccess::Write))
            .chain(rest.iter().map(read))
            .collect(),
        (None, [only]) => vec![PathTarget::new(*only, PathAccess::Unknown)],
        (None, [sources @ .., destination]) => sources
            .iter()
            .map(read)
            .chain(std::iter::once(PathTarget::new(
                *destination,
                PathAccess::Write,
            )))
            .collect(),
        (None, []) => Vec::new(),
    }
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
            targets.push(PathTarget::new(arg, PathAccess::Read));
        } else if arg == "--" {
            options_end = true;
        } else if let Some(value) = option_value(args, &mut index, arg, short, long) {
            targets.push(PathTarget::new(value, PathAccess::Write));
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
    use super::{PathAccess, PathTarget};

    type Target = (Option<String>, PathAccess);

    fn targets(command: &str) -> Vec<Target> {
        shell_path_targets(command)
            .into_iter()
            .map(|PathTarget { path, access }| (path, access))
            .collect()
    }

    fn read(path: &str) -> Target {
        (Some(path.to_owned()), PathAccess::Read)
    }

    fn write(path: &str) -> Target {
        (Some(path.to_owned()), PathAccess::Write)
    }

    fn unknown(path: &str) -> Target {
        (Some(path.to_owned()), PathAccess::Unknown)
    }

    const UNRESOLVED: Target = (None, PathAccess::Unknown);

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
            ("sudo cp a /etc/x", vec![read("a"), write("/etc/x")]),
            ("bash -c 'tee /opt/y'", vec![write("/opt/y")]),
            ("echo $(cat secret)", vec![UNRESOLVED, read("secret")]),
            ("cat $FILE", vec![UNRESOLVED]),
            ("cat ~/.ssh/config", vec![UNRESOLVED]),
            ("rm *.o", vec![UNRESOLVED]),
            ("echo x > \"$OUT\"", vec![unknown("x"), UNRESOLVED]),
            ("bash -s", vec![UNRESOLVED]),
            ("cat 'unterminated", vec![UNRESOLVED]),
            ("export A=1 && cargo test", vec![unknown("test")]),
        ];
        for (command, expected) in cases {
            assert_eq!(targets(command), expected, "{command}");
        }
    }
}
