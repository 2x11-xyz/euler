use std::ffi::{OsStr, OsString};

// Issue #27: animated braille spinner (10 frames, cycled at 80-100ms by the
// caller) — previously a single frozen glyph.
const UNICODE_SPINNER: &[&str] = &[
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];
const ASCII_SPINNER: &[&str] = &["-", "\\", "|", "/"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GlyphCapability {
    Unicode,
    Ascii,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GlyphSet {
    user_rail: &'static str,
    user_rail_prefix: &'static str,
    thinking: &'static str,
    /// Default anchor for the v2 spine (prose, tools).
    bullet: &'static str,
    spinner_frames: &'static [&'static str],
    check: &'static str,
    cross: &'static str,
    interrupt: &'static str,
    companion: &'static str,
    companion_rail_prefix: &'static str,
    revert: &'static str,
    warning: &'static str,
    prompt: &'static str,
    tree_mid: &'static str,
    tree_last: &'static str,
}

impl GlyphSet {
    pub(crate) const UNICODE: Self = Self {
        user_rail: "\u{258c}",
        user_rail_prefix: "\u{258c} ",
        thinking: "\u{2731}",
        bullet: "\u{2022}",
        spinner_frames: UNICODE_SPINNER,
        check: "\u{2713}",
        cross: "\u{2717}",
        interrupt: "\u{25a0}",
        companion: "\u{25c6}",
        companion_rail_prefix: "\u{258c} ",
        revert: "\u{21a9}",
        warning: "\u{26a0}",
        prompt: "\u{276f}",
        tree_mid: "\u{251c}",
        tree_last: "\u{2514}",
    };

    pub(crate) const ASCII: Self = Self {
        user_rail: "|",
        user_rail_prefix: "| ",
        thinking: "*",
        bullet: ".",
        spinner_frames: ASCII_SPINNER,
        check: "ok",
        cross: "x",
        interrupt: "#",
        companion: "&",
        companion_rail_prefix: "| ",
        revert: "<-",
        warning: "!",
        prompt: ">",
        tree_mid: "+-",
        tree_last: "\\-",
    };

    pub(crate) fn current() -> Self {
        let set = match glyph_capability() {
            GlyphCapability::Unicode => Self::UNICODE,
            GlyphCapability::Ascii => Self::ASCII,
        };
        debug_assert!(set.table_is_complete());
        set
    }

    pub(crate) const fn user_rail(self) -> &'static str {
        self.user_rail
    }

    pub(crate) const fn user_rail_prefix(self) -> &'static str {
        self.user_rail_prefix
    }

    pub(crate) const fn thinking(self) -> &'static str {
        self.thinking
    }

    pub(crate) const fn bullet(self) -> &'static str {
        self.bullet
    }

    pub(crate) fn spinner(self, frame: usize) -> &'static str {
        self.spinner_frames[frame % self.spinner_frames.len()]
    }

    pub(crate) const fn check(self) -> &'static str {
        self.check
    }

    pub(crate) const fn cross(self) -> &'static str {
        self.cross
    }

    pub(crate) const fn interrupt(self) -> &'static str {
        self.interrupt
    }

    pub(crate) const fn companion(self) -> &'static str {
        self.companion
    }

    pub(crate) const fn companion_rail_prefix(self) -> &'static str {
        self.companion_rail_prefix
    }

    pub(crate) const fn revert(self) -> &'static str {
        self.revert
    }

    pub(crate) const fn warning(self) -> &'static str {
        self.warning
    }

    pub(crate) const fn prompt(self) -> &'static str {
        self.prompt
    }

    pub(crate) const fn tree_mid(self) -> &'static str {
        self.tree_mid
    }

    pub(crate) const fn tree_last(self) -> &'static str {
        self.tree_last
    }

    fn table_is_complete(self) -> bool {
        !self.user_rail().is_empty()
            && !self.user_rail_prefix().is_empty()
            && !self.thinking().is_empty()
            && !self.bullet().is_empty()
            && !self.spinner(0).is_empty()
            && !self.check().is_empty()
            && !self.cross().is_empty()
            && !self.interrupt().is_empty()
            && !self.companion().is_empty()
            && !self.companion_rail_prefix().is_empty()
            && !self.revert().is_empty()
            && !self.warning().is_empty()
            && !self.prompt().is_empty()
            && !self.tree_mid().is_empty()
            && !self.tree_last().is_empty()
    }
}

pub(crate) fn glyph_set() -> GlyphSet {
    GlyphSet::current()
}

pub(crate) fn bullet() -> &'static str {
    glyph_set().bullet()
}

pub(crate) fn thinking() -> &'static str {
    glyph_set().thinking()
}

pub(crate) fn check() -> &'static str {
    glyph_set().check()
}

pub(crate) fn cross() -> &'static str {
    glyph_set().cross()
}

pub(crate) fn interrupt() -> &'static str {
    glyph_set().interrupt()
}

pub(crate) fn revert() -> &'static str {
    glyph_set().revert()
}

pub(crate) fn glyph_capability() -> GlyphCapability {
    if unicode_supported() {
        GlyphCapability::Unicode
    } else {
        GlyphCapability::Ascii
    }
}

pub(crate) fn unicode_supported() -> bool {
    unicode_supported_from_env(|key| std::env::var_os(key))
}

fn unicode_supported_from_env(mut env: impl FnMut(&str) -> Option<OsString>) -> bool {
    if env("NO_UNICODE").is_some() {
        return false;
    }

    let term = env("TERM");
    let lang = env("LANG");
    let lc_all = env("LC_ALL");
    if [&term, &lang, &lc_all]
        .into_iter()
        .flatten()
        .any(|value| contains_ascii_marker(value))
    {
        return false;
    }

    let locale = lc_all
        .as_ref()
        .filter(|value| !value.is_empty())
        .or(lang.as_ref());
    locale.is_none_or(|value| contains_utf8_marker(value))
}

fn contains_ascii_marker(value: &OsStr) -> bool {
    value
        .to_string_lossy()
        .to_ascii_uppercase()
        .contains("ASCII")
}

fn contains_utf8_marker(value: &OsStr) -> bool {
    let value = value.to_string_lossy().to_ascii_uppercase();
    value.contains("UTF-8") || value.contains("UTF8")
}

pub(crate) fn user_line_prefix(_first_visual_row: bool) -> &'static str {
    glyph_set().user_rail_prefix()
}

/// Bare rail glyph (no trailing pad space) for the shared spine-anchor slot
/// (review v3 §R4) — `stamp_first_line` pads it to `SPINE_WIDTH` itself, the
/// same as every other anchor glyph.
pub(crate) fn user_rail() -> &'static str {
    glyph_set().user_rail()
}

pub(crate) fn companion_glyph() -> &'static str {
    glyph_set().companion()
}

pub(crate) fn companion_rail_prefix() -> &'static str {
    glyph_set().companion_rail_prefix()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_supported(pairs: &[(&str, &str)]) -> bool {
        let env = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(value)))
            .collect::<HashMap<_, _>>();
        unicode_supported_from_env(|key| env.get(key).cloned())
    }

    #[test]
    fn default_detection_prefers_unicode() {
        assert!(env_supported(&[]));
    }

    #[test]
    fn no_unicode_env_forces_ascii() {
        assert!(!env_supported(&[
            ("NO_UNICODE", "1"),
            ("LANG", "en_US.UTF-8")
        ]));
    }

    #[test]
    fn ascii_marker_in_terminal_or_locale_forces_ascii() {
        assert!(!env_supported(&[
            ("TERM", "ascii"),
            ("LANG", "en_US.UTF-8")
        ]));
        assert!(!env_supported(&[("LANG", "en_US.ASCII")]));
        assert!(!env_supported(&[
            ("LC_ALL", "C.ASCII"),
            ("LANG", "en_US.UTF-8")
        ]));
    }

    #[test]
    fn locale_without_utf8_forces_ascii_when_locale_is_set() {
        assert!(!env_supported(&[("LANG", "C")]));
        assert!(!env_supported(&[
            ("LC_ALL", "POSIX"),
            ("LANG", "en_US.UTF-8")
        ]));
    }

    #[test]
    fn utf8_locale_allows_unicode() {
        assert!(env_supported(&[
            ("TERM", "xterm-256color"),
            ("LANG", "en_US.UTF-8")
        ]));
        assert!(env_supported(&[("LANG", "en_US.UTF8")]));
        assert!(env_supported(&[("LC_ALL", ""), ("LANG", "en_US.UTF-8")]));
    }

    #[test]
    fn unicode_set_matches_warm_ledger_table() {
        let glyphs = GlyphSet::UNICODE;
        assert_eq!(glyphs.user_rail(), "▌");
        assert_eq!(glyphs.user_rail_prefix(), "▌ ");
        assert_eq!(glyphs.thinking(), "✱");
        // Issue #27: 10-frame animated braille spinner (never frozen).
        assert_eq!(glyphs.spinner(0), "⠋");
        assert_eq!(glyphs.spinner(1), "⠙");
        assert_eq!(glyphs.spinner(2), "⠹");
        assert_eq!(glyphs.spinner(3), "⠸");
        assert_eq!(glyphs.spinner(4), "⠼");
        assert_eq!(glyphs.spinner(5), "⠴");
        assert_eq!(glyphs.spinner(6), "⠦");
        assert_eq!(glyphs.spinner(7), "⠧");
        assert_eq!(glyphs.spinner(8), "⠇");
        assert_eq!(glyphs.spinner(9), "⠏");
        assert_eq!(glyphs.spinner(10), "⠋", "frame index wraps around");
        assert_eq!(glyphs.check(), "✓");
        assert_eq!(glyphs.cross(), "✗");
        assert_eq!(glyphs.interrupt(), "■");
        assert_eq!(glyphs.companion(), "◆");
        assert_eq!(glyphs.companion_rail_prefix(), "▌ ");
        assert_eq!(glyphs.revert(), "↩");
        assert_eq!(glyphs.warning(), "⚠");
        assert_eq!(glyphs.prompt(), "❯");
        assert_eq!(glyphs.tree_mid(), "├");
        assert_eq!(glyphs.tree_last(), "└");
    }

    #[test]
    fn ascii_set_matches_warm_ledger_table() {
        let glyphs = GlyphSet::ASCII;
        assert_eq!(glyphs.user_rail(), "|");
        assert_eq!(glyphs.user_rail_prefix(), "| ");
        assert_eq!(glyphs.thinking(), "*");
        assert_eq!(glyphs.spinner(0), "-");
        assert_eq!(glyphs.spinner(1), "\\");
        assert_eq!(glyphs.spinner(2), "|");
        assert_eq!(glyphs.spinner(3), "/");
        assert_eq!(glyphs.spinner(4), "-");
        assert_eq!(glyphs.check(), "ok");
        assert_eq!(glyphs.cross(), "x");
        assert_eq!(glyphs.interrupt(), "#");
        assert_eq!(glyphs.companion(), "&");
        assert_eq!(glyphs.companion_rail_prefix(), "| ");
        assert_eq!(glyphs.revert(), "<-");
        assert_eq!(glyphs.warning(), "!");
        assert_eq!(glyphs.prompt(), ">");
        assert_eq!(glyphs.tree_mid(), "+-");
        assert_eq!(glyphs.tree_last(), "\\-");
    }
}
