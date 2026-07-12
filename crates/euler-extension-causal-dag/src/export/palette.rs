use crate::input_error;
use euler_sdk::ExtensionError;
use serde::Deserialize;
use std::collections::BTreeMap;

pub(super) const PALETTE_JSON: &str = include_str!("../../assets/palette.json");

const STATUSES: [&str; 8] = [
    "verified",
    "success",
    "open",
    "inconclusive",
    "blocked",
    "dead_end",
    "superseded",
    "abandoned",
];
const ARC_KINDS: [&str; 6] = [
    "pivot",
    "evidence",
    "supersedes",
    "refutation",
    "related",
    "artifact_use",
];

#[derive(Clone, Debug, Deserialize)]
pub(super) struct Palette {
    schema: String,
    pub(super) backgrounds: Backgrounds,
    pub(super) structural_edges: ThemeColors,
    pub(super) root: ThemeColors,
    status_order: Vec<String>,
    pub(super) statuses: BTreeMap<String, StatusToken>,
    pub(super) cross_arcs: CrossArcTokens,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct Backgrounds {
    pub(super) day: String,
    pub(super) night: String,
    pub(super) constellation: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ThemeColors {
    pub(super) day: String,
    pub(super) night: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct StatusToken {
    pub(super) day: String,
    pub(super) night: String,
    pub(super) glyph: String,
    pub(super) label: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct CrossArcTokens {
    pub(super) rest: String,
    pub(super) opacity: f64,
    pub(super) kinds: BTreeMap<String, String>,
}

impl Palette {
    pub(super) fn load() -> Result<Self, ExtensionError> {
        let palette: Self = serde_json::from_str(PALETTE_JSON)
            .map_err(|error| input_error(format!("invalid causal-dag palette: {error}")))?;
        palette.validate()?;
        Ok(palette)
    }

    pub(super) fn status(&self, status: &str) -> Result<&StatusToken, ExtensionError> {
        self.statuses.get(status).ok_or_else(|| {
            input_error(format!("causal-dag palette has no status token `{status}`"))
        })
    }

    fn validate(&self) -> Result<(), ExtensionError> {
        if self.schema != "euler.causal_dag.palette.v1" {
            return Err(input_error("unsupported causal-dag palette schema"));
        }
        if self.status_order != STATUSES {
            return Err(input_error(
                "causal-dag palette status order does not match the v2 status set",
            ));
        }
        require_exact_keys(
            "status",
            self.statuses.keys().map(String::as_str),
            &STATUSES,
        )?;
        for status in STATUSES {
            let token = self.status(status)?;
            validate_color(&token.day)?;
            validate_color(&token.night)?;
            if token.glyph.is_empty() || token.label.is_empty() {
                return Err(input_error(format!(
                    "causal-dag palette status `{status}` needs a glyph and label"
                )));
            }
        }
        for color in [
            &self.backgrounds.day,
            &self.backgrounds.night,
            &self.backgrounds.constellation,
            &self.structural_edges.day,
            &self.structural_edges.night,
            &self.root.day,
            &self.root.night,
            &self.cross_arcs.rest,
        ] {
            validate_color(color)?;
        }
        if !(0.0..=1.0).contains(&self.cross_arcs.opacity) {
            return Err(input_error(
                "causal-dag palette cross-arc opacity must be between zero and one",
            ));
        }
        require_exact_keys(
            "cross-arc kind",
            self.cross_arcs.kinds.keys().map(String::as_str),
            &ARC_KINDS,
        )?;
        for kind in ARC_KINDS {
            validate_color(&self.cross_arcs.kinds[kind])?;
        }
        Ok(())
    }
}

fn require_exact_keys<'a, const N: usize>(
    owner: &str,
    actual: impl Iterator<Item = &'a str>,
    expected: &[&str; N],
) -> Result<(), ExtensionError> {
    let actual = actual.collect::<std::collections::BTreeSet<_>>();
    let expected = expected
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    if actual == expected {
        Ok(())
    } else {
        Err(input_error(format!(
            "causal-dag palette {owner} keys do not match the canonical set"
        )))
    }
}

fn validate_color(color: &str) -> Result<(), ExtensionError> {
    if color.len() == 7
        && color.starts_with('#')
        && color[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Ok(());
    }
    Err(input_error(format!(
        "causal-dag palette color `{color}` is not #RRGGBB"
    )))
}

#[cfg(test)]
mod tests {
    use super::Palette;

    #[test]
    fn canonical_palette_covers_the_schema_without_overloading_kind() {
        let palette = Palette::load().expect("palette");
        assert_eq!(palette.statuses.len(), 8);
        assert_eq!(palette.status("verified").unwrap().day, "#0072B2");
        assert_eq!(palette.status("dead_end").unwrap().night, "#ff8a52");
        assert_eq!(palette.status("abandoned").unwrap().glyph, "⊗");
        assert_eq!(palette.root.day, "#E8B931");
        assert_eq!(palette.root.night, "#f0cb52");
        assert_eq!(palette.cross_arcs.rest, "#7f97a8");
        assert_eq!(palette.cross_arcs.opacity, 0.45);
        assert_eq!(palette.cross_arcs.kinds["refutation"], "#D55E00");
    }
}
