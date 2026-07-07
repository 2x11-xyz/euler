const USER_RAIL_PREFIX: &str = "\u{258c} ";

pub(crate) fn user_line_prefix(_first_visual_row: bool) -> &'static str {
    USER_RAIL_PREFIX
}
