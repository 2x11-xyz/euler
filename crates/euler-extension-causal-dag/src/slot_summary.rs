use crate::{input_error, projection::Projection};
use euler_sdk::{ExtensionError, HostApi};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub(crate) const GRAPH_SLOT_NAME: &str = "graph";
const MAX_SLOT_BYTES: usize = 4096;
const ACTIVE_PATH_LIMIT: usize = 8;
const TITLE_BYTES: usize = 120;
const ROOT_TITLE_BYTES: usize = 160;
const REASON_BYTES: usize = 360;

#[derive(Clone, Debug)]
struct NodeSummary {
    id: String,
    status: String,
    title: String,
    reason: String,
    order: usize,
}

#[derive(Clone, Debug)]
struct DeadEndLine {
    title: String,
    reason: String,
}

#[derive(Clone, Debug)]
struct SummaryParts {
    header: String,
    active_path: Vec<String>,
    active_hidden: usize,
    open: Vec<String>,
    open_hidden: usize,
    dead_ends: Vec<DeadEndLine>,
    dead_hidden: usize,
}

pub(crate) fn render_slot_summary(projection: &Projection) -> String {
    let graph = SummaryGraph::from_projection(projection);
    render_graph_summary(&graph)
}

pub(crate) fn render_artifact_summary(artifact: &Value) -> Result<String, ExtensionError> {
    let graph = SummaryGraph::from_artifact(artifact)?;
    Ok(render_graph_summary(&graph))
}

fn render_graph_summary(graph: &SummaryGraph) -> String {
    let mut parts = SummaryParts::from_graph(graph);
    parts.fit_to_budget();
    parts.render()
}

struct SummaryGraph {
    active_root: Option<String>,
    roots: Vec<String>,
    nodes: Vec<Value>,
    edges: Vec<Value>,
}

impl SummaryGraph {
    fn from_projection(projection: &Projection) -> Self {
        Self {
            active_root: projection.active_root_id().map(str::to_owned),
            roots: projection.root_ids().map(str::to_owned).collect(),
            nodes: projection.nodes().to_vec(),
            edges: projection.edges().to_vec(),
        }
    }

    fn from_artifact(artifact: &Value) -> Result<Self, ExtensionError> {
        let forest = artifact
            .get("forest")
            .and_then(Value::as_object)
            .ok_or_else(|| input_error("causal-dag artifact is missing `forest`"))?;
        let roots = forest
            .get("roots")
            .and_then(Value::as_array)
            .ok_or_else(|| input_error("causal-dag artifact is missing `forest.roots`"))?
            .iter()
            .map(|root| {
                root.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| input_error("causal-dag forest.roots must contain strings"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let active_root = match forest.get("active_root") {
            None | Some(Value::Null) => None,
            Some(Value::String(root)) => Some(root.clone()),
            _ => return Err(input_error("causal-dag forest.active_root is invalid")),
        };
        let nodes = forest
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| input_error("causal-dag artifact is missing `forest.nodes`"))?;
        let edges = forest
            .get("edges")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| input_error("causal-dag artifact is missing `forest.edges`"))?;
        Ok(Self {
            active_root,
            roots,
            nodes,
            edges,
        })
    }

    fn node_count(&self) -> usize {
        self.nodes.len()
    }

    fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SlotPublication {
    Published,
    Failed(String),
    NotAttempted,
}

impl SlotPublication {
    pub(crate) fn merge(self, next: Self) -> Self {
        match (self, next) {
            (Self::Failed(error), _) | (_, Self::Failed(error)) => Self::Failed(error),
            (Self::Published, _) | (_, Self::Published) => Self::Published,
            (Self::NotAttempted, Self::NotAttempted) => Self::NotAttempted,
        }
    }
}

pub(crate) fn publish_graph_slot(host: &dyn HostApi, projection: &Projection) -> SlotPublication {
    let summary = render_slot_summary(projection);
    match host.update_context_slot(GRAPH_SLOT_NAME, &summary) {
        Ok(()) => SlotPublication::Published,
        Err(error) => SlotPublication::Failed(error.to_string()),
    }
}

pub(crate) fn with_slot_publication(mut output: Value, publication: SlotPublication) -> Value {
    let object = output
        .as_object_mut()
        .expect("command output is constructed as an object");
    match publication {
        SlotPublication::Published => {
            object.insert("slot_published".to_owned(), Value::Bool(true));
        }
        SlotPublication::NotAttempted => {
            object.insert("slot_published".to_owned(), Value::Bool(false));
            object.insert(
                "slot_error".to_owned(),
                Value::String("not attempted: no graph artifact persisted".to_owned()),
            );
        }
        SlotPublication::Failed(error) => {
            object.insert("slot_published".to_owned(), Value::Bool(false));
            object.insert("slot_error".to_owned(), Value::String(error));
        }
    }
    output
}

impl SummaryParts {
    fn from_graph(graph: &SummaryGraph) -> Self {
        let ordering = BackboneOrdering::from_graph(graph);
        let nodes = projection_nodes(graph, &ordering.order);
        let root_title = root_title(graph, &nodes);
        let mut active_path = ordering
            .active_path
            .iter()
            .filter_map(|id| nodes.get(id))
            .map(|node| node.title.clone())
            .collect::<Vec<_>>();
        let active_hidden = active_path.len().saturating_sub(ACTIVE_PATH_LIMIT);
        if active_hidden > 0 {
            active_path = active_path.split_off(active_hidden);
        }

        let mut ordered = nodes.values().collect::<Vec<_>>();
        ordered.sort_by_key(|node| (node.order, node.id.as_str()));

        Self {
            header: format!(
                "GRAPH: {} ({} nodes, {} edges)",
                bounded_text(&root_title, ROOT_TITLE_BYTES),
                graph.node_count(),
                graph.edge_count()
            ),
            active_path,
            active_hidden,
            open: ordered
                .iter()
                .filter(|node| is_open_status(&node.status))
                .map(|node| node.title.clone())
                .collect(),
            open_hidden: 0,
            dead_ends: ordered
                .iter()
                .filter(|node| is_dead_end_status(&node.status))
                .map(|node| DeadEndLine {
                    title: node.title.clone(),
                    reason: node.reason.clone(),
                })
                .collect(),
            dead_hidden: 0,
        }
    }

    fn fit_to_budget(&mut self) {
        while self.byte_len() > MAX_SLOT_BYTES && !self.open.is_empty() {
            self.open.pop();
            self.open_hidden += 1;
        }
        while self.byte_len() > MAX_SLOT_BYTES && !self.active_path.is_empty() {
            self.active_path.remove(0);
            self.active_hidden += 1;
        }
        while self.byte_len() > MAX_SLOT_BYTES && self.shorten_longest_dead_reason() {}
        while self.byte_len() > MAX_SLOT_BYTES && !self.dead_ends.is_empty() {
            self.dead_ends.pop();
            self.dead_hidden += 1;
        }
    }

    fn shorten_longest_dead_reason(&mut self) -> bool {
        let Some((index, current_len)) = self
            .dead_ends
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                let len = line.reason.len();
                (len > 0).then_some((index, len))
            })
            .max_by_key(|(_, len)| *len)
        else {
            return false;
        };
        let next_len = current_len.saturating_sub(current_len.max(24) / 2);
        self.dead_ends[index].reason = bounded_text(&self.dead_ends[index].reason, next_len);
        true
    }

    fn byte_len(&self) -> usize {
        self.render().len()
    }

    fn render(&self) -> String {
        let mut lines = vec![self.header.clone()];
        push_section(
            &mut lines,
            "DEAD ENDS",
            self.dead_ends.iter().map(render_dead_end_line),
            self.dead_hidden,
        );
        push_section(
            &mut lines,
            "ACTIVE PATH",
            self.active_path.iter().map(|title| format!("- {title}")),
            self.active_hidden,
        );
        push_section(
            &mut lines,
            "OPEN",
            self.open.iter().map(|title| format!("- {title}")),
            self.open_hidden,
        );
        lines.join("\n")
    }
}

#[derive(Default)]
struct BackboneOrdering {
    order: BTreeMap<String, usize>,
    active_path: Vec<String>,
}

impl BackboneOrdering {
    fn from_graph(graph: &SummaryGraph) -> Self {
        let children = backbone_children(&graph.edges);
        let roots = ordered_roots(graph);
        let mut order = BTreeMap::new();
        let mut next = 0usize;
        for root in &roots {
            assign_order(root, &children, &mut order, &mut next);
        }
        let active_root = graph
            .active_root
            .as_deref()
            .or_else(|| roots.first().map(String::as_str));
        let active_path = active_root.map_or_else(Vec::new, |root| best_path(root, &children));
        Self { order, active_path }
    }
}

fn projection_nodes(
    graph: &SummaryGraph,
    backbone_order: &BTreeMap<String, usize>,
) -> BTreeMap<String, NodeSummary> {
    graph
        .nodes
        .iter()
        .filter_map(|node| {
            let id = string_field(node, "id")?;
            Some((
                id.clone(),
                NodeSummary {
                    order: backbone_order.get(&id).copied().unwrap_or(usize::MAX),
                    id,
                    status: string_field(node, "status").unwrap_or_default(),
                    title: bounded_text(&line_text(&string_field(node, "title")?), TITLE_BYTES),
                    reason: bounded_text(
                        &reason_text(&string_field(node, "summary").unwrap_or_default()),
                        REASON_BYTES,
                    ),
                },
            ))
        })
        .collect()
}

fn root_title(graph: &SummaryGraph, nodes: &BTreeMap<String, NodeSummary>) -> String {
    graph
        .active_root
        .as_deref()
        .and_then(|id| nodes.get(id))
        .or_else(|| graph.roots.iter().find_map(|id| nodes.get(id)))
        .map(|node| node.title.clone())
        .unwrap_or_else(|| "empty graph".to_owned())
}

fn backbone_children(edges: &[Value]) -> BTreeMap<String, Vec<String>> {
    let mut children = BTreeMap::<String, BTreeSet<String>>::new();
    for edge in edges {
        if edge.get("canonical_backbone").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(from) = string_field(edge, "from") else {
            continue;
        };
        let Some(to) = string_field(edge, "to") else {
            continue;
        };
        children.entry(from).or_default().insert(to);
    }
    children
        .into_iter()
        .map(|(from, children)| (from, children.into_iter().collect()))
        .collect()
}

fn ordered_roots(graph: &SummaryGraph) -> Vec<String> {
    graph.roots.clone()
}

fn assign_order(
    root: &str,
    children: &BTreeMap<String, Vec<String>>,
    order: &mut BTreeMap<String, usize>,
    next: &mut usize,
) {
    let mut queue = VecDeque::from([root.to_owned()]);
    while let Some(node) = queue.pop_front() {
        if order.insert(node.clone(), *next).is_some() {
            continue;
        }
        *next += 1;
        if let Some(node_children) = children.get(&node) {
            queue.extend(node_children.iter().cloned());
        }
    }
}

fn best_path(root: &str, children: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let Some(node_children) = children.get(root) else {
        return vec![root.to_owned()];
    };
    let mut best = Vec::new();
    for child in node_children {
        let candidate = best_path(child, children);
        if candidate.len() > best.len() || (candidate.len() == best.len() && candidate < best) {
            best = candidate;
        }
    }
    let mut path = vec![root.to_owned()];
    path.extend(best);
    path
}

fn push_section(
    lines: &mut Vec<String>,
    heading: &str,
    body: impl Iterator<Item = String>,
    hidden: usize,
) {
    lines.push(String::new());
    lines.push(format!("{heading}:"));
    if hidden > 0 {
        lines.push(format!("… {hidden} more"));
    }
    lines.extend(body);
}

fn render_dead_end_line(line: &DeadEndLine) -> String {
    if line.reason.is_empty() {
        format!("- {}", line.title)
    } else {
        format!("- {} — {}", line.title, line.reason)
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

fn is_open_status(status: &str) -> bool {
    matches!(status, "open")
}

fn is_dead_end_status(status: &str) -> bool {
    // `superseded` normalizes an abandoned approach; keep all dead-end-class
    // statuses visible after compaction.
    matches!(status, "dead_end" | "abandoned" | "superseded")
}

fn reason_text(summary: &str) -> String {
    let line = line_text(summary);
    let first_sentence = line
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '.' | '!' | '?').then_some(index + ch.len_utf8()))
        .map_or(line.as_str(), |end| &line[..end]);
    first_sentence.trim().to_owned()
}

fn line_text(value: &str) -> String {
    value
        .chars()
        .filter_map(|ch| match ch {
            '\n' => Some(' '),
            ch if ch.is_control() => None,
            ch => Some(ch),
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    if max_bytes == 0 {
        return String::new();
    }
    if max_bytes <= '…'.len_utf8() {
        return "…".to_owned();
    }
    let limit = max_bytes - '…'.len_utf8();
    let mut end = 0;
    for (index, ch) in value.char_indices() {
        let next = index + ch.len_utf8();
        if next > limit {
            break;
        }
        end = next;
    }
    format!("{}…", value[..end].trim_end())
}

#[cfg(test)]
pub(crate) fn rendered_len(summary: &str) -> usize {
    summary.len()
}
