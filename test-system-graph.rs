// test-system-graph.rs
//
// A standalone, std-only demonstration of navigating complex graphs.
// Compile and run:
//   rustc -O test-system-graph.rs -o test-system-graph && ./test-system-graph
//
// Covers: construction, BFS, DFS (recursive + iterative), connected reachability,
// shortest path (unweighted BFS), Dijkstra (weighted), topological sort with
// cycle detection, strongly connected components (Tarjan), and a sample
// "system" graph modeling module dependencies with weighted build costs.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};

/// A weighted directed graph over arbitrary node ids of type Id.
#[derive(Debug, Clone, Default)]
struct Graph<Id: Clone + Eq + std::hash::Hash + Ord + std::fmt::Debug> {
    // adjacency: node -> list of (neighbor, edge weight)
    edges: BTreeMap<Id, Vec<(Id, u64)>>,
    // reverse adjacency for reachability-from and SCC bookkeeping
    redges: BTreeMap<Id, Vec<Id>>,
    // declared nodes (so isolated nodes survive)
    nodes: HashSet<Id>,
}

impl<Id> Graph<Id>
where
    Id: Clone + Eq + std::hash::Hash + Ord + std::fmt::Debug,
{
    fn new() -> Self {
        Self { edges: BTreeMap::new(), redges: BTreeMap::new(), nodes: HashSet::new() }
    }

    fn add_node(&mut self, id: Id) {
        self.nodes.insert(id.clone());
        self.edges.entry(id.clone()).or_default();
        self.redges.entry(id).or_default();
    }

    fn add_edge(&mut self, from: Id, to: Id, weight: u64) {
        self.add_node(from.clone());
        self.add_node(to.clone());
        self.edges.entry(from.clone()).or_default().push((to.clone(), weight));
        self.redges.entry(to).or_default().push(from);
    }

    fn neighbors(&self, id: &Id) -> Option<&Vec<(Id, u64)>> {
        self.edges.get(id)
    }

    fn node_count(&self) -> usize {
        self.nodes.len()
    }

    fn edge_count(&self) -> usize {
        self.edges.values().map(|v| v.len()).sum()
    }

    /// Breadth-first search from start. Returns visit order.
    fn bfs(&self, start: &Id) -> Vec<Id> {
        let mut order = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        seen.insert(start.clone());
        queue.push_back(start.clone());
        while let Some(node) = queue.pop_front() {
            order.push(node.clone());
            if let Some(nbrs) = self.neighbors(&node) {
                for (nb, _w) in nbrs {
                    if seen.insert(nb.clone()) {
                        queue.push_back(nb.clone());
                    }
                }
            }
        }
        order
    }

    /// Iterative depth-first search from start. Returns visit order.
    fn dfs(&self, start: &Id) -> Vec<Id> {
        let mut order = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![start.clone()];
        while let Some(node) = stack.pop() {
            if !seen.insert(node.clone()) {
                continue;
            }
            order.push(node.clone());
            if let Some(nbrs) = self.neighbors(&node) {
                // push in reverse so we visit in declared order
                for (nb, _w) in nbrs.iter().rev() {
                    if !seen.contains(nb) {
                        stack.push(nb.clone());
                    }
                }
            }
        }
        order
    }

    /// Recursive DFS. Returns visit order.
    fn dfs_recursive(&self, start: &Id) -> Vec<Id> {
        let mut order = Vec::new();
        let mut seen = HashSet::new();
        self.dfs_rec(start, &mut seen, &mut order);
        order
    }

    fn dfs_rec(&self, node: &Id, seen: &mut HashSet<Id>, order: &mut Vec<Id>) {
        if !seen.insert(node.clone()) {
            return;
        }
        order.push(node.clone());
        if let Some(nbrs) = self.neighbors(node) {
            for (nb, _w) in nbrs {
                self.dfs_rec(nb, seen, order);
            }
        }
    }

    /// Is target reachable from start?
    fn reachable(&self, start: &Id, target: &Id) -> bool {
        if start == target {
            return true;
        }
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([start.clone()]);
        seen.insert(start.clone());
        while let Some(node) = queue.pop_front() {
            if let Some(nbrs) = self.neighbors(&node) {
                for (nb, _w) in nbrs {
                    if nb == target {
                        return true;
                    }
                    if seen.insert(nb.clone()) {
                        queue.push_back(nb.clone());
                    }
                }
            }
        }
        false
    }

    /// Shortest path (fewest edges) from start to target via BFS.
    /// Returns the path as a Vec, or None if unreachable.
    fn shortest_path(&self, start: &Id, target: &Id) -> Option<Vec<Id>> {
        if start == target {
            return Some(vec![start.clone()]);
        }
        let mut parent: HashMap<Id, Id> = HashMap::new();
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        seen.insert(start.clone());
        queue.push_back(start.clone());
        while let Some(node) = queue.pop_front() {
            if let Some(nbrs) = self.neighbors(&node) {
                for (nb, _w) in nbrs {
                    if seen.insert(nb.clone()) {
                        parent.insert(nb.clone(), node.clone());
                        if nb == target {
                            // reconstruct
                            let mut path = vec![target.clone()];
                            let mut cur = target.clone();
                            while let Some(p) = parent.get(&cur) {
                                path.push(p.clone());
                                cur = p.clone();
                            }
                            path.reverse();
                            return Some(path);
                        }
                        queue.push_back(nb.clone());
                    }
                }
            }
        }
        None
    }

    /// Dijkstra: lowest total weight from start to every reachable node,
    /// plus a parent map to reconstruct paths.
    fn dijkstra(&self, start: &Id) -> (HashMap<Id, u64>, HashMap<Id, Id>) {
        let mut dist: HashMap<Id, u64> = HashMap::new();
        let mut parent: HashMap<Id, Id> = HashMap::new();
        let mut heap = BinaryHeap::new();
        dist.insert(start.clone(), 0);
        heap.push(State { cost: 0, node: start.clone() });
        while let Some(State { cost, node }) = heap.pop() {
            if cost > *dist.get(&node).unwrap_or(&u64::MAX) {
                continue;
            }
            if let Some(nbrs) = self.neighbors(&node) {
                for (nb, w) in nbrs {
                    let next = cost.saturating_add(*w);
                    if next < *dist.get(nb).unwrap_or(&u64::MAX) {
                        dist.insert(nb.clone(), next);
                        parent.insert(nb.clone(), node.clone());
                        heap.push(State { cost: next, node: nb.clone() });
                    }
                }
            }
        }
        (dist, parent)
    }

    /// Reconstruct a Dijkstra path from the parent map.
    fn dijkstra_path(&self, parent: &HashMap<Id, Id>, start: &Id, target: &Id) -> Option<Vec<Id>> {
        if !parent.contains_key(target) && start != target {
            return None;
        }
        let mut path = vec![target.clone()];
        let mut cur = target.clone();
        while cur != *start {
            let p = parent.get(&cur)?;
            path.push(p.clone());
            cur = p.clone();
        }
        path.reverse();
        Some(path)
    }

    /// Topological sort via Kahn's algorithm. Returns Err(cycle_sample) if
    /// the graph contains a cycle.
    fn topological_sort(&self) -> Result<Vec<Id>, Vec<Id>> {
        let mut indegree: HashMap<Id, usize> = self.nodes.iter().map(|n| (n.clone(), 0)).collect();
        for nbrs in self.edges.values() {
            for (nb, _w) in nbrs {
                *indegree.get_mut(nb).expect("node exists") += 1;
            }
        }
        let ready: Vec<Id> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(n, _)| n.clone())
            .collect();
        let mut ready: VecDeque<Id> = {
            let mut v = ready;
            v.sort();
            v.into_iter().collect()
        };

        let mut order = Vec::new();
        while let Some(node) = ready.pop_front() {
            order.push(node.clone());
            if let Some(nbrs) = self.neighbors(&node) {
                let mut newly_ready: Vec<Id> = Vec::new();
                for (nb, _w) in nbrs {
                    let d = indegree.get_mut(nb).expect("node exists");
                    *d -= 1;
                    if *d == 0 {
                        newly_ready.push(nb.clone());
                    }
                }
                newly_ready.sort();
                for n in newly_ready {
                    ready.push_back(n);
                }
            }
        }
        if order.len() == self.nodes.len() {
            Ok(order)
        } else {
            // remaining nodes with indegree > 0 are in cycles
            let cyclic: Vec<Id> = indegree
                .into_iter()
                .filter(|(_, d)| *d > 0)
                .map(|(n, _)| n)
                .collect();
            Err(cyclic)
        }
    }

    /// Tarjan's strongly connected components. Returns SCCs as Vecs of node ids,
    /// each non-trivial when it has >1 node or a self-loop.
    fn strongly_connected_components(&self) -> Vec<Vec<Id>> {
        let mut index: HashMap<Id, usize> = HashMap::new();
        let mut lowlink: HashMap<Id, usize> = HashMap::new();
        let mut on_stack: HashSet<Id> = HashSet::new();
        let mut stack: Vec<Id> = Vec::new();
        let mut sccs: Vec<Vec<Id>> = Vec::new();
        let mut counter: usize = 0;

        for node in self.nodes.iter() {
            if !index.contains_key(node) {
                strongconnect(
                    node,
                    self,
                    &mut index,
                    &mut lowlink,
                    &mut on_stack,
                    &mut stack,
                    &mut sccs,
                    &mut counter,
                );
            }
        }
        sccs
    }
}

fn strongconnect<Id>(
    v: &Id,
    g: &Graph<Id>,
    index: &mut HashMap<Id, usize>,
    lowlink: &mut HashMap<Id, usize>,
    on_stack: &mut HashSet<Id>,
    stack: &mut Vec<Id>,
    sccs: &mut Vec<Vec<Id>>,
    counter: &mut usize,
) where
    Id: Clone + Eq + std::hash::Hash + Ord + std::fmt::Debug,
{
    // Iterative formulation to avoid blowing the native stack on large graphs.
    let mut work: Vec<(Id, usize)> = vec![(v.clone(), 0)];
    index.insert(v.clone(), *counter);
    lowlink.insert(v.clone(), *counter);
    *counter += 1;
    stack.push(v.clone());
    on_stack.insert(v.clone());

    while let Some((node, i)) = work.last().cloned() {
        let nbrs = g.neighbors(&node).cloned().unwrap_or_default();
        if i < nbrs.len() {
            let (w, _weight) = &nbrs[i];
            // advance the cursor for this frame
            work.last_mut().expect("frame").1 = i + 1;
            if !index.contains_key(w) {
                index.insert(w.clone(), *counter);
                lowlink.insert(w.clone(), *counter);
                *counter += 1;
                stack.push(w.clone());
                on_stack.insert(w.clone());
                work.push((w.clone(), 0));
            } else if on_stack.contains(w) {
                let li = *index.get(w).unwrap();
                let cur = *lowlink.get(&node).unwrap();
                lowlink.insert(node.clone(), cur.min(li));
            }
        } else {
            // done with node; form an SCC if it is a root
            let root_index = *index.get(&node).unwrap();
            let node_low = *lowlink.get(&node).unwrap();
            if node_low == root_index {
                let mut comp = Vec::new();
                loop {
                    let w = stack.pop().expect("stack nonempty");
                    on_stack.remove(&w);
                    comp.push(w.clone());
                    if w == node {
                        break;
                    }
                }
                sccs.push(comp);
            }
            work.pop();
            // propagate lowlink to parent
            if let Some((parent, _)) = work.last().cloned() {
                let pl = *lowlink.get(&parent).unwrap();
                lowlink.insert(parent.clone(), pl.min(node_low));
            }
        }
    }
}

#[derive(Eq, PartialEq, Clone)]
struct State<Id: Eq> {
    cost: u64,
    node: Id,
}

impl<Id: Eq> Ord for State<Id> {
    fn cmp(&self, other: &Self) -> Ordering {
        // min-heap: invert
        other.cost.cmp(&self.cost)
    }
}
impl<Id: Eq> PartialOrd for State<Id> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// --- Demo graph: a system of modules with build dependencies and weights ---

fn build_system_graph() -> Graph<&'static str> {
    let mut g: Graph<&'static str> = Graph::new();
    // (from, to, weight) -- weight = relative build cost
    let edges: [(&'static str, &'static str, u64); 22] = [
        ("core", "event", 2),
        ("core", "sdk", 3),
        ("core", "tools", 4),
        ("event", "core", 2),       // back-edge: mutual dependency (cycle)
        ("sdk", "agents", 5),
        ("sdk", "tools", 1),
        ("tools", "cli", 8),
        ("agents", "cli", 6),
        ("agents", "swarm", 4),     // companion agent
        ("provider", "core", 2),
        ("provider", "cli", 7),
        ("cli", "user", 0),         // the human consumer
        ("swarm", "reviewer", 3),
        ("reviewer", "guardian", 5),
        ("guardian", "swarm", 1),   // back-edge: guardian feeds back into swarm (cycle)
        ("guardian", "cli", 9),
        ("causal-dag", "sdk", 4),
        ("causal-dag", "event", 2),
        ("session-export", "event", 1),
        ("session-export", "sdk", 2),
        ("diagnostics", "event", 3),
        ("autoresearch", "sdk", 6),
    ];
    for (f, t, w) in edges {
        g.add_edge(f, t, w);
    }
    // an isolated node for completeness
    g.add_node("scratch");
    g
}

fn main() {
    let g = build_system_graph();

    println!("== System Graph ==");
    println!("nodes: {}  edges: {}", g.node_count(), g.edge_count());
    println!("nodes: {:?}", g.nodes.iter().cloned().collect::<Vec<_>>());
    println!();

    println!("== BFS from `core` ==");
    println!("{:?}", g.bfs(&"core"));
    println!();

    println!("== DFS (iterative) from `core` ==");
    println!("{:?}", g.dfs(&"core"));
    println!();

    println!("== DFS (recursive) from `core` ==");
    println!("{:?}", g.dfs_recursive(&"core"));
    println!();

    println!("== Reachability ==");
    for target in ["cli", "guardian", "scratch", "does-not-exist"] {
        println!("  core -> {} : {}", target, g.reachable(&"core", &target));
    }
    println!();

    println!("== Shortest path (fewest edges) ==");
    for target in ["cli", "guardian", "scratch"] {
        match g.shortest_path(&"core", &target) {
            Some(p) => println!("  core -> {} : {:?}", target, p),
            None => println!("  core -> {} : unreachable", target),
        }
    }
    println!();

    println!("== Dijkstra (weighted) from `core` ==");
    let (dist, parent) = g.dijkstra(&"core");
    let mut ranked: Vec<_> = dist.iter().collect();
    ranked.sort_by_key(|(_, d)| **d);
    for (node, d) in &ranked {
        let path = g.dijkstra_path(&parent, &"core", node).unwrap_or_default();
        println!("  {:<14} cost={:<3} path={:?}", node, d, path);
    }
    println!();

    println!("== Topological sort ==");
    match g.topological_sort() {
        Ok(order) => println!("  acyclic; order: {:?}", order),
        Err(cycle_nodes) => {
            println!("  cycle detected among: {:?}", cycle_nodes);
        }
    }
    println!();

    println!("== Strongly connected components (Tarjan) ==");
    let sccs = g.strongly_connected_components();
    let mut nontrivial = 0;
    for comp in &sccs {
        let is_loop = comp.len() > 1
            || comp
                .first()
                .and_then(|n| g.neighbors(n))
                .map(|ns| ns.iter().any(|(nb, _)| *nb == *comp.first().unwrap()))
                .unwrap_or(false);
        if is_loop {
            nontrivial += 1;
            println!("  * cycle: {:?}", comp);
        } else {
            println!("    trivial: {:?}", comp);
        }
    }
    println!("  {} non-trivial cycle component(s) found", nontrivial);
    println!();

    println!("== Summary ==");
    println!("  The system graph contains feedback loops (core<->event, guardian->swarm),");
    println!("  so a strict build order does not exist; Tarjan isolates the cycles and");
    println!("  Dijkstra still yields the cheapest path to each reachable node.");
}
