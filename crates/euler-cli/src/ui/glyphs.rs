const USER_RAIL_PREFIX: &str = "\u{258c} ";
const COMPANION_GLYPH: &str = "\u{25c6}"; // ◆
const COMPANION_RAIL_PREFIX: &str = "\u{258c} "; // ▌ (teal via theme, not user green)

pub(crate) fn user_line_prefix(_first_visual_row: bool) -> &'static str {
    USER_RAIL_PREFIX
}

pub(crate) fn companion_glyph() -> &'static str {
    COMPANION_GLYPH
}

pub(crate) fn companion_rail_prefix() -> &'static str {
    COMPANION_RAIL_PREFIX
}
