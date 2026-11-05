//! The `path` and `outside_workspace` surfaces (ADR 0027 section 3 and
//! resolved question 2). Each path a request names is a target with a
//! direction. A target and a pattern are each matched in literal and
//! canonical form (symlinks resolved up to the longest existing ancestor),
//! so `/tmp` also covers `/private/tmp` on macOS; a target takes the most
//! restrictive of its forms' last matches (the later rule on a tie). `path` patterns are globs over
//! components (`*` within one, `**` zero or more), relative to the
//! workspace root unless absolute or `~/`. `outside_workspace` patterns are
//! directory prefixes matched component by component, against targets that
//! resolve outside the workspace root.

use super::{
    glob_matches, reduce_targets, RequestPaths, RuleAccess, RuleLayer, RuleMatch, RuleState,
    RuleSurface,
};
use crate::command_safety::PathAccess;
use std::path::{Component, Path, PathBuf};

/// A request's targets, resolved once for every layer.
pub(super) struct ResolvedPaths {
    root: PathBuf,
    canonical_root: PathBuf,
    home: Option<PathBuf>,
    /// `None` for a target Euler cannot resolve.
    targets: Vec<Option<ResolvedTarget>>,
}

struct ResolvedTarget {
    /// The literal (lexically normalized) and canonical absolute forms.
    forms: [PathBuf; 2],
    /// Whether the canonical form lies outside the workspace root.
    outside: bool,
    access: PathAccess,
}

impl ResolvedPaths {
    pub(super) fn resolve(paths: RequestPaths<'_>) -> Self {
        let root = lexical(paths.workspace_root);
        let canonical_root = canonical(&root);
        let targets = paths
            .targets
            .iter()
            .map(|target| {
                let joined = root.join(target.path.as_deref()?);
                let canonical = canonical(&joined);
                Some(ResolvedTarget {
                    outside: !canonical.starts_with(&canonical_root),
                    forms: [lexical(&joined), canonical],
                    access: target.access,
                })
            })
            .collect();
        Self {
            root,
            canonical_root,
            home: std::env::var_os("HOME").map(PathBuf::from),
            targets,
        }
    }

    /// The state of `surface` (`path` or `outside_workspace`) in `layer`: the
    /// last matching rule for each target on the surface, reduced as a
    /// command's segments are. A target Euler cannot resolve counts as `ask`
    /// when the surface has an `ask` or `deny` rule (human-only with a
    /// `deny`), and as no rule state otherwise.
    pub(super) fn surface_match(
        &self,
        layer: &RuleLayer,
        surface: RuleSurface,
    ) -> Option<RuleMatch> {
        let rules = match surface {
            RuleSurface::Path => &layer.rules.path,
            RuleSurface::OutsideWorkspace => &layer.rules.outside_workspace,
            RuleSurface::Shell | RuleSurface::Tools => return None,
        };
        if rules.is_empty() {
            return None;
        }
        let restrictive = rules.iter().any(|rule| rule.state != RuleState::Allow);
        let human_only = rules.iter().any(|rule| rule.state == RuleState::Deny);
        let patterns: Vec<Vec<PathBuf>> = rules
            .iter()
            .map(|rule| self.pattern_forms(&rule.pattern))
            .collect();
        let matches = |target: &ResolvedTarget, form: &Path| {
            rules.iter().zip(&patterns).rposition(|(rule, patterns)| {
                covers(rule.access, target.access)
                    && patterns.iter().any(|pattern| match surface {
                        RuleSurface::OutsideWorkspace => form.starts_with(pattern),
                        _ => glob_path_matches(pattern, form),
                    })
            })
        };
        let results = self
            .targets
            .iter()
            .filter(|target| {
                surface == RuleSurface::Path || target.as_ref().is_none_or(|target| target.outside)
            })
            .map(|target| match target {
                None => restrictive.then_some(RuleMatch::Unresolved {
                    surface,
                    human_only,
                }),
                Some(target) => target
                    .forms
                    .iter()
                    .filter_map(|form| matches(target, form))
                    .reduce(|best, next| {
                        if (rules[next].state, next) > (rules[best].state, best) {
                            next
                        } else {
                            best
                        }
                    })
                    .map(|index| RuleMatch::Rule {
                        surface,
                        layer: layer.layer,
                        index,
                        rule: rules[index].clone(),
                    }),
            })
            .collect();
        reduce_targets(results)
    }

    /// The literal and canonical absolute forms of a pattern: `~/` under
    /// `HOME` (no forms without one), a relative pattern under the literal
    /// and canonical workspace root, and the canonical form of the literal
    /// prefix before the first component with `*`, so a symlink in that
    /// prefix (in the workspace part of a relative pattern too) resolves as
    /// it does for a target.
    fn pattern_forms(&self, pattern: &str) -> Vec<PathBuf> {
        let mut forms = Vec::new();
        let absolute = match pattern.strip_prefix("~/") {
            Some(rest) => match &self.home {
                Some(home) => home.join(rest),
                None => return Vec::new(),
            },
            None if Path::new(pattern).is_absolute() => PathBuf::from(pattern),
            None => {
                forms.push(lexical(&self.canonical_root.join(pattern)));
                self.root.join(pattern)
            }
        };
        let literal = lexical(&absolute);
        let components: Vec<Component<'_>> = literal.components().collect();
        let prefix_len = components
            .iter()
            .position(|component| component.as_os_str().to_string_lossy().contains('*'))
            .unwrap_or(components.len());
        let mut canonical_form = canonical(&components[..prefix_len].iter().collect::<PathBuf>());
        canonical_form.extend(&components[prefix_len..]);
        forms.extend([literal, canonical_form]);
        forms
    }
}

/// Whether a rule with `access` covers a target used with `access`: `any`
/// covers every target, and a target of unknown direction matches every
/// rule.
fn covers(rule: Option<RuleAccess>, target: PathAccess) -> bool {
    matches!(
        (rule.unwrap_or(RuleAccess::Any), target),
        (RuleAccess::Any, _)
            | (_, PathAccess::Unknown)
            | (RuleAccess::Read, PathAccess::Read)
            | (RuleAccess::Write, PathAccess::Write)
    )
}

/// Whether the glob `pattern` matches all of `path`, component by
/// component: `**` matches zero or more components and any other component
/// is a [`glob_matches`] pattern for one component.
fn glob_path_matches(pattern: &Path, path: &Path) -> bool {
    let path: Vec<_> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect();
    // `reachable[n]`: the pattern so far can match the first `n` components.
    let mut reachable = vec![false; path.len() + 1];
    reachable[0] = true;
    for part in pattern.components() {
        let part = part.as_os_str().to_string_lossy();
        let mut next = vec![false; path.len() + 1];
        if part == "**" {
            let mut seen = false;
            for (at, slot) in next.iter_mut().enumerate() {
                seen |= reachable[at];
                *slot = seen;
            }
        } else {
            for (at, component) in path.iter().enumerate() {
                next[at + 1] = reachable[at] && glob_matches(&part, component);
            }
        }
        reachable = next;
    }
    reachable[path.len()]
}

/// `path` with `.` removed and `..` applied lexically.
fn lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// `path` with symlinks resolved up to its longest existing ancestor, the
/// rest appended; its lexical form when no ancestor resolves.
fn canonical(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(mut resolved) = existing.canonicalize() {
            resolved.extend(missing.iter().rev());
            return lexical(&resolved);
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return lexical(path),
        }
    }
}
