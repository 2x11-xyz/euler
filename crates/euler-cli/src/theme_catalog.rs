#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThemeChoice {
    #[default]
    GruvboxDark,
    GruvboxLight,
    WarmLedger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeMode {
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThemeProfile {
    pub choice: ThemeChoice,
    pub id: &'static str,
    pub label: &'static str,
    pub mode: ThemeMode,
    aliases: &'static [&'static str],
}

impl ThemeChoice {
    pub fn parse(value: &str) -> Option<Self> {
        let normalized = normalize_theme_id(value);
        THEME_PROFILES
            .iter()
            .find(|profile| profile.matches(&normalized))
            .map(|profile| profile.choice)
    }

    pub fn as_str(self) -> &'static str {
        self.profile().id
    }

    pub fn label(self) -> &'static str {
        self.profile().label
    }

    pub fn profile(self) -> &'static ThemeProfile {
        THEME_PROFILES
            .iter()
            .find(|profile| profile.choice == self)
            .expect("theme choice must have a profile")
    }

    pub fn all() -> &'static [ThemeProfile] {
        THEME_PROFILES
    }

    pub fn canonical_ids() -> impl Iterator<Item = &'static str> {
        THEME_PROFILES.iter().map(|profile| profile.id)
    }

    pub fn format_canonical_ids(separator: &str) -> String {
        Self::canonical_ids().collect::<Vec<_>>().join(separator)
    }
}

impl ThemeProfile {
    fn matches(self, normalized: &str) -> bool {
        self.id == normalized || self.aliases.contains(&normalized)
    }
}

const THEME_PROFILES: &[ThemeProfile] = &[
    ThemeProfile {
        choice: ThemeChoice::GruvboxDark,
        id: "gruvbox-dark",
        label: "Gruvbox Dark",
        mode: ThemeMode::Dark,
        aliases: &["dark"],
    },
    ThemeProfile {
        choice: ThemeChoice::GruvboxLight,
        id: "gruvbox-light",
        label: "Gruvbox Light",
        mode: ThemeMode::Light,
        aliases: &["light"],
    },
    ThemeProfile {
        choice: ThemeChoice::WarmLedger,
        id: "warm-ledger",
        label: "Warm Ledger",
        mode: ThemeMode::Dark,
        aliases: &["warm"],
    },
];

fn normalize_theme_id(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('_', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_choice_parses_canonical_ids_and_aliases() {
        assert_eq!(
            ThemeChoice::parse("gruvbox-dark"),
            Some(ThemeChoice::GruvboxDark)
        );
        assert_eq!(
            ThemeChoice::parse("gruvbox_dark"),
            Some(ThemeChoice::GruvboxDark)
        );
        assert_eq!(ThemeChoice::parse(" dark "), Some(ThemeChoice::GruvboxDark));
        assert_eq!(
            ThemeChoice::parse("gruvbox-light"),
            Some(ThemeChoice::GruvboxLight)
        );
        assert_eq!(ThemeChoice::parse("LIGHT"), Some(ThemeChoice::GruvboxLight));
        assert_eq!(ThemeChoice::parse("gruvbox"), None);
    }

    #[test]
    fn theme_catalog_exposes_structured_profiles_and_usage_ids() {
        assert_eq!(
            ThemeChoice::canonical_ids().collect::<Vec<_>>(),
            vec!["gruvbox-dark", "gruvbox-light", "warm-ledger"]
        );
        assert_eq!(
            ThemeChoice::format_canonical_ids("|"),
            "gruvbox-dark|gruvbox-light|warm-ledger"
        );
        assert_eq!(
            ThemeChoice::parse("warm-ledger"),
            Some(ThemeChoice::WarmLedger)
        );
        assert_eq!(ThemeChoice::parse("warm"), Some(ThemeChoice::WarmLedger));
        assert_eq!(ThemeChoice::GruvboxLight.label(), "Gruvbox Light");
        assert_eq!(ThemeChoice::GruvboxDark.profile().mode, ThemeMode::Dark);
        for profile in ThemeChoice::all() {
            assert_eq!(profile.choice.profile().choice, profile.choice);
        }
    }
}
