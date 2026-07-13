use super::graph::{ViewerDag, ViewerNode};
use super::palette::Palette;
use crate::input_error;
use euler_sdk::ExtensionError;
use std::collections::{BTreeMap, BTreeSet};

const X_GAP: f64 = 190.0;
const Y_GAP: f64 = 116.0;
const MARGIN_X: f64 = 90.0;
const MARGIN_TOP: f64 = 112.0;
const MARGIN_BOTTOM: f64 = 96.0;

#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

pub(super) fn render_svg(dag: &ViewerDag, palette: &Palette) -> Result<Vec<u8>, ExtensionError> {
    let positions = layout(dag)?;
    let max_x = positions.values().map(|point| point.x).fold(0.0, f64::max);
    let max_y = positions.values().map(|point| point.y).fold(0.0, f64::max);
    let width = (max_x + MARGIN_X).max(720.0);
    let height = (max_y + MARGIN_BOTTOM).max(360.0);
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.0}\" height=\"{height:.0}\" viewBox=\"0 0 {width:.0} {height:.0}\" role=\"img\" aria-labelledby=\"title desc\">\n"
    );
    svg.push_str(&format!(
        "<title id=\"title\">{}</title><desc id=\"desc\">Euler causal DAG with {} nodes and {} annotation cross-arcs.</desc>\n",
        escape_xml(&dag.title),
        dag.node_count(),
        dag.cross_arc_count()
    ));
    svg.push_str(&format!(
        "<rect width=\"100%\" height=\"100%\" fill=\"{}\"/>\n",
        palette.backgrounds.day
    ));
    svg.push_str(&format!(
        "<defs><marker id=\"arrow\" viewBox=\"0 0 10 10\" refX=\"9\" refY=\"5\" markerWidth=\"6\" markerHeight=\"6\" orient=\"auto-start-reverse\"><path d=\"M 0 0 L 10 5 L 0 10 z\" fill=\"{}\" fill-opacity=\"{:.2}\"/></marker></defs>\n",
        palette.cross_arcs.rest, palette.cross_arcs.opacity
    ));
    svg.push_str(&format!(
        "<text x=\"40\" y=\"42\" font-family=\"ui-monospace,monospace\" font-size=\"18\" font-weight=\"600\" fill=\"#26251f\">{}</text>\n",
        escape_xml(&dag.title)
    ));
    svg.push_str(&format!(
        "<text x=\"40\" y=\"66\" font-family=\"ui-monospace,monospace\" font-size=\"12\" fill=\"#8a887f\">causal DAG · {} nodes · {} cross-arcs · {}</text>\n",
        dag.node_count(),
        dag.cross_arc_count(),
        escape_xml(&dag.operation)
    ));
    render_backbone(&mut svg, dag, &positions, palette);
    render_arcs(&mut svg, dag, &positions, palette);
    render_nodes(&mut svg, dag, &positions, palette)?;
    svg.push_str("</svg>\n");
    Ok(svg.into_bytes())
}

fn layout(dag: &ViewerDag) -> Result<BTreeMap<String, Point>, ExtensionError> {
    if dag.nodes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let children = dag.children();
    let mut state = LayoutState::new(&children);
    for root in &dag.roots {
        state.assign(root, 0)?;
        state.next_leaf += 0.5;
    }
    if state.visited.len() != dag.nodes.len() {
        return Err(input_error(
            "causal-dag backbone contains nodes unreachable from forest roots",
        ));
    }
    Ok(state
        .x_units
        .into_iter()
        .map(|(id, x)| {
            let y = state.depth.get(&id).copied().unwrap_or_default() as f64;
            (
                id,
                Point {
                    x: MARGIN_X + x * X_GAP,
                    y: MARGIN_TOP + y * Y_GAP,
                },
            )
        })
        .collect())
}

struct LayoutState<'a> {
    children: &'a BTreeMap<&'a str, Vec<&'a ViewerNode>>,
    x_units: BTreeMap<String, f64>,
    depth: BTreeMap<String, usize>,
    next_leaf: f64,
    visiting: BTreeSet<String>,
    visited: BTreeSet<String>,
}

impl<'a> LayoutState<'a> {
    fn new(children: &'a BTreeMap<&'a str, Vec<&'a ViewerNode>>) -> Self {
        Self {
            children,
            x_units: BTreeMap::new(),
            depth: BTreeMap::new(),
            next_leaf: 0.0,
            visiting: BTreeSet::new(),
            visited: BTreeSet::new(),
        }
    }

    fn assign(&mut self, id: &str, level: usize) -> Result<f64, ExtensionError> {
        if !self.visiting.insert(id.to_owned()) {
            return Err(input_error("causal-dag backbone contains a cycle"));
        }
        if !self.visited.insert(id.to_owned()) {
            return Err(input_error(format!(
                "causal-dag node `{id}` appears in multiple backbone branches"
            )));
        }
        self.depth.insert(id.to_owned(), level);
        let node_children = self.children.get(id).cloned().unwrap_or_default();
        let x = if node_children.is_empty() {
            let x = self.next_leaf;
            self.next_leaf += 1.0;
            x
        } else {
            let mut child_x = Vec::with_capacity(node_children.len());
            for child in node_children {
                child_x.push(self.assign(&child.id, level + 1)?);
            }
            (child_x[0] + child_x[child_x.len() - 1]) / 2.0
        };
        self.visiting.remove(id);
        self.x_units.insert(id.to_owned(), x);
        Ok(x)
    }
}

fn render_backbone(
    svg: &mut String,
    dag: &ViewerDag,
    positions: &BTreeMap<String, Point>,
    palette: &Palette,
) {
    for node in &dag.nodes {
        let Some(parent) = node.parent.as_deref() else {
            continue;
        };
        let (Some(from), Some(to)) = (positions.get(parent), positions.get(&node.id)) else {
            continue;
        };
        svg.push_str(&format!(
            "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"{}\" stroke-width=\"1.25\"/>\n",
            from.x, from.y, to.x, to.y, palette.structural_edges.day
        ));
    }
}

fn render_arcs(
    svg: &mut String,
    dag: &ViewerDag,
    positions: &BTreeMap<String, Point>,
    palette: &Palette,
) {
    for arc in &dag.arcs {
        let (Some(from), Some(to)) = (positions.get(&arc.from), positions.get(&arc.to)) else {
            continue;
        };
        let mid_x = (from.x + to.x) / 2.0;
        let mid_y = (from.y + to.y) / 2.0 - 34.0;
        svg.push_str(&format!(
            "<path d=\"M {:.1} {:.1} Q {:.1} {:.1} {:.1} {:.1}\" fill=\"none\" stroke=\"{}\" stroke-opacity=\"{:.2}\" stroke-width=\"1.25\" stroke-dasharray=\"4 6\" marker-end=\"url(#arrow)\"><title>{}: {}</title></path>\n",
            from.x,
            from.y,
            mid_x,
            mid_y,
            to.x,
            to.y,
            palette.cross_arcs.rest,
            palette.cross_arcs.opacity,
            escape_xml(&arc.kind),
            escape_xml(&arc.note)
        ));
    }
}

fn render_nodes(
    svg: &mut String,
    dag: &ViewerDag,
    positions: &BTreeMap<String, Point>,
    palette: &Palette,
) -> Result<(), ExtensionError> {
    for node in &dag.nodes {
        let Some(point) = positions.get(&node.id) else {
            continue;
        };
        let status = palette.status(&node.status)?;
        let is_root = node.kind == "root";
        let color = if is_root {
            palette.root.day.as_str()
        } else {
            status.day.as_str()
        };
        let glyph = if is_root {
            "○"
        } else {
            status.glyph.as_str()
        };
        svg.push_str("<g>");
        svg.push_str(&format!(
            "<title>{} · {}\n{}\nEvidence: {}</title>",
            escape_xml(&node.title),
            escape_xml(&status.label),
            escape_xml(&node.summary),
            escape_xml(&node.ev)
        ));
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" dominant-baseline=\"central\" font-family=\"ui-monospace,monospace\" font-size=\"{}\" font-weight=\"{}\" fill=\"{}\">{}</text>",
            point.x,
            point.y,
            if is_root { 20 } else { 17 },
            if is_root { 600 } else { 400 },
            color,
            escape_xml(glyph)
        ));
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-family=\"ui-monospace,monospace\" font-size=\"11\" fill=\"#4a4840\">{}</text></g>\n",
            point.x,
            point.y + 34.0,
            escape_xml(&truncate_chars(&node.title, 28))
        ));
    }
    Ok(())
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_owned()
    } else {
        format!(
            "{}…",
            value
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
        )
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::render_svg;
    use crate::export::{graph::ViewerDag, palette::Palette};
    use serde_json::Value;

    #[test]
    fn static_svg_uses_bare_status_marks_and_a_gold_root() {
        let mut artifact: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/causal_dag/knuth_style_search/expected.causal-dag.json"
        ))
        .expect("fixture artifact");
        let root = artifact["forest"]["nodes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|node| node["kind"] == "root")
            .expect("root");
        root["status"] = Value::String("verified".to_owned());

        let dag = ViewerDag::from_artifact(&artifact).expect("viewer DAG");
        let svg = String::from_utf8(
            render_svg(&dag, &Palette::load().expect("palette")).expect("SVG render"),
        )
        .expect("UTF-8 SVG");

        assert!(svg.contains("fill=\"#E8B931\">○</text>"));
        assert!(!svg.contains("<circle"));
    }
}
