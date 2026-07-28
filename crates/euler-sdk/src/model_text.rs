/// Returns whether extension-authored text is free of Unicode formatting and
/// separator characters that can hide, reorder, or split model-facing text.
///
/// This deliberately does not validate ordinary control characters, length,
/// or emptiness: those policies differ between host APIs. Every extension
/// boundary that admits text to the model canvas should combine this shared
/// predicate with its own structural rules.
pub fn extension_model_text_is_format_safe(text: &str) -> bool {
    !text.chars().any(is_format_spoof)
}

/// Unicode 17.0.0 `General_Category=Format` (`Cf`) ranges plus the line and
/// paragraph separators (`Zl`/`Zp`). These characters can alter, hide,
/// reorder, or split terminal/model-facing text while surviving
/// `char::is_control`.
fn is_format_spoof(character: char) -> bool {
    matches!(
        character,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{2029}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

#[cfg(test)]
mod tests {
    use super::extension_model_text_is_format_safe;

    #[test]
    fn rejects_every_unicode_17_format_code_point() {
        let ranges = [
            (0x00AD, 0x00AD),
            (0x0600, 0x0605),
            (0x061C, 0x061C),
            (0x06DD, 0x06DD),
            (0x070F, 0x070F),
            (0x0890, 0x0891),
            (0x08E2, 0x08E2),
            (0x180E, 0x180E),
            (0x200B, 0x200F),
            (0x202A, 0x202E),
            (0x2060, 0x2064),
            (0x2066, 0x206F),
            (0xFEFF, 0xFEFF),
            (0xFFF9, 0xFFFB),
            (0x110BD, 0x110BD),
            (0x110CD, 0x110CD),
            (0x13430, 0x1343F),
            (0x1BCA0, 0x1BCA3),
            (0x1D173, 0x1D17A),
            (0xE0001, 0xE0001),
            (0xE0020, 0xE007F),
        ];
        let mut count = 0;
        for (start, end) in ranges {
            for code_point in start..=end {
                let character = char::from_u32(code_point).expect("Unicode scalar");
                assert!(
                    !extension_model_text_is_format_safe(&character.to_string()),
                    "accepted U+{code_point:04X}"
                );
                count += 1;
            }
        }
        assert_eq!(count, 170);
        for separator in ['\u{2028}', '\u{2029}'] {
            assert!(!extension_model_text_is_format_safe(&separator.to_string()));
        }
        assert!(
            extension_model_text_is_format_safe("\u{2065}"),
            "reserved U+2065 is not Cf"
        );
    }
}
