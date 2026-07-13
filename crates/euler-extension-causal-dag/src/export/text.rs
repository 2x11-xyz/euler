use super::graph::{ViewerDag, ViewerNode};
use super::palette::Palette;
use euler_sdk::ExtensionError;
use std::collections::BTreeSet;

pub(super) fn render_dot(dag: &ViewerDag, palette: &Palette) -> Result<Vec<u8>, ExtensionError> {
    let mut dot = String::from("digraph causal_dag {\n");
    dot.push_str(&format!(
        "  graph [rankdir=TB, bgcolor=\"{}\", pad=0.25, nodesep=0.35, ranksep=0.6];\n",
        palette.backgrounds.day
    ));
    dot.push_str(&format!(
        "  node [fontname=\"monospace\", style=\"filled\", fillcolor=\"{}\"];\n",
        palette.backgrounds.day
    ));
    dot.push_str("  edge [fontname=\"monospace\", fontsize=9];\n");
    for node in &dag.nodes {
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
        dot.push_str(&format!(
            "  \"{}\" [label=\"{} {}\", shape={}, color=\"{}\", fontcolor=\"{}\", penwidth={}];\n",
            dot_escape(&node.id),
            dot_escape(glyph),
            dot_escape(&node.title),
            graphviz_shape(&node.kind),
            color,
            color,
            if node.kind == "root" || node.kind == "synthesis" {
                "2"
            } else {
                "1"
            }
        ));
    }
    for node in &dag.nodes {
        if let Some(parent) = node.parent.as_deref() {
            dot.push_str(&format!(
                "  \"{}\" -> \"{}\" [color=\"{}\"];\n",
                dot_escape(parent),
                dot_escape(&node.id),
                palette.structural_edges.day
            ));
        }
    }
    for arc in &dag.arcs {
        let kind_color = palette
            .cross_arcs
            .kinds
            .get(&arc.kind)
            .unwrap_or(&palette.cross_arcs.rest);
        dot.push_str(&format!(
            "  \"{}\" -> \"{}\" [label=\"{}\", style=dashed, color=\"{}\", fontcolor=\"{}\", constraint=false];\n",
            dot_escape(&arc.from),
            dot_escape(&arc.to),
            dot_escape(&arc.kind),
            palette.cross_arcs.rest,
            kind_color
        ));
    }
    dot.push_str("}\n");
    Ok(dot.into_bytes())
}

pub(super) fn render_markdown(dag: &ViewerDag) -> Vec<u8> {
    let mut markdown = format!(
        "# {}\n\n- Session: `{}`\n- Construction: `{}`\n- Nodes: {}\n- Edges: {}\n- Cross-arcs: {}\n\n## Backbone\n\n",
        markdown_text(&dag.title),
        markdown_text(&dag.session_id),
        markdown_text(&dag.operation),
        dag.node_count(),
        dag.edge_count(),
        dag.cross_arc_count()
    );
    let children = dag.children();
    let mut seen = BTreeSet::new();
    if dag.roots.is_empty() {
        markdown.push_str("_Empty graph._\n");
    } else {
        for root in &dag.roots {
            render_outline(dag, root, 0, &children, &mut seen, &mut markdown);
        }
    }
    markdown.push_str("\n## Dead Ends\n\n");
    push_status_nodes(
        &mut markdown,
        dag.nodes.iter().filter(|node| {
            matches!(
                node.status.as_str(),
                "dead_end" | "abandoned" | "superseded"
            )
        }),
    );
    markdown.push_str("\n## Open Frontier\n\n");
    push_status_nodes(
        &mut markdown,
        dag.nodes
            .iter()
            .filter(|node| matches!(node.status.as_str(), "open" | "blocked" | "inconclusive")),
    );
    markdown.push_str("\n## Cross-Arcs\n\n");
    if dag.arcs.is_empty() {
        markdown.push_str("_None._\n");
    } else {
        for arc in &dag.arcs {
            let from = dag
                .node_by_id(&arc.from)
                .map_or(arc.from.as_str(), |node| node.title.as_str());
            let to = dag
                .node_by_id(&arc.to)
                .map_or(arc.to.as_str(), |node| node.title.as_str());
            markdown.push_str(&format!(
                "- **{}:** {} → {}{}\n",
                markdown_text(&arc.kind),
                markdown_text(from),
                markdown_text(to),
                if arc.note.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", markdown_text(&arc.note))
                }
            ));
        }
    }
    markdown.into_bytes()
}

fn render_outline(
    dag: &ViewerDag,
    id: &str,
    depth: usize,
    children: &std::collections::BTreeMap<&str, Vec<&ViewerNode>>,
    seen: &mut BTreeSet<String>,
    output: &mut String,
) {
    if !seen.insert(id.to_owned()) {
        return;
    }
    let Some(node) = dag.node_by_id(id) else {
        return;
    };
    output.push_str(&format!(
        "{}- **{}** _{}_ — {}\n",
        "  ".repeat(depth),
        markdown_text(&node.title),
        markdown_text(&node.status),
        markdown_text(&node.summary)
    ));
    if let Some(node_children) = children.get(id) {
        for child in node_children {
            render_outline(dag, &child.id, depth + 1, children, seen, output);
        }
    }
}

fn push_status_nodes<'a>(output: &mut String, nodes: impl Iterator<Item = &'a ViewerNode>) {
    let mut count = 0usize;
    for node in nodes {
        count += 1;
        output.push_str(&format!(
            "- **{}** (`{}`) — {}\n",
            markdown_text(&node.title),
            markdown_text(&node.status),
            markdown_text(&node.summary)
        ));
    }
    if count == 0 {
        output.push_str("_None._\n");
    }
}

fn graphviz_shape(kind: &str) -> &'static str {
    match kind {
        "root" => "circle",
        "attempt" => "circle",
        "claim" => "diamond",
        "checkpoint" => "box",
        "synthesis" => "doublecircle",
        _ => "ellipse",
    }
}

fn dot_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "")
        .replace('\n', "\\n")
}

fn markdown_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('\r', "")
        .replace('\n', " ")
}
