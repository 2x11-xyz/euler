use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionKind {
    #[default]
    Interactive,
    NonInteractive,
}

impl SessionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::NonInteractive => "non-interactive",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "interactive" => Some(Self::Interactive),
            "non-interactive" => Some(Self::NonInteractive),
            _ => None,
        }
    }
}
