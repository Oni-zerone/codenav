//! Dependency cycles between files or modules: strongly connected components (Tarjan),
//! then the shortest simple cycles inside each one.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde_json::{Value, json};

use crate::graph::{EdgeKind, Graph, SymKind};
use crate::query::{self, EdgeFilter};

/// Units cycles are reported between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    File,
    /// Directories truncated at this depth, as in module_graph.
    Module(usize),
}

/// At most this many cycles are listed per component.
const CYCLES_PER_COMPONENT: usize = 10;
/// Members listed per component.
const MEMBERS_SHOWN: usize = 20;
/// Cycles are searched from at most this many members of a component (the most
/// connected first), so a huge component stays cheap.
const MAX_STARTS: usize = 100;

pub fn find_cycles(
    g: &Graph,
    level: Level,
    filter: EdgeFilter,
    max_length: usize,
    limit: usize,
) -> Value {
    // Unit of every node, computed once: files, or directory components as module_graph
    // groups them (externals have none).
    let unit_name = |i: usize| -> Option<String> {
        let n = &g.nodes[i];
        match level {
            _ if n.kind == SymKind::External => None,
            Level::File if n.file.is_empty() => None,
            Level::File => Some(n.file.clone()),
            Level::Module(depth) => Some(query::component(&n.file, depth)),
        }
    };
    let node_units: Vec<Option<String>> = (0..g.nodes.len()).map(unit_name).collect();
    // Ids follow the sorted unit names, so everything below is deterministic.
    let names: Vec<&String> = node_units
        .iter()
        .flatten()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let ids: HashMap<&String, usize> = names.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let unit: Vec<Option<usize>> = node_units
        .iter()
        .map(|u| u.as_ref().map(|u| ids[u]))
        .collect();

    // Dependencies between units, with the edge kinds behind each.
    let mut deps: BTreeMap<(usize, usize), BTreeSet<&str>> = BTreeMap::new();
    for e in &g.edges {
        if e.kind == EdgeKind::Contains || !filter.allows(e) {
            continue;
        }
        if let (Some(a), Some(b)) = (unit[e.from], unit[e.to])
            && a != b
        {
            deps.entry((a, b)).or_default().insert(e.kind.as_str());
        }
    }
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); names.len()];
    for &(a, b) in deps.keys() {
        adj[a].push(b); // sorted: deps iterates in key order
    }

    let mut components: Vec<Vec<usize>> =
        tarjan(&adj).into_iter().filter(|c| c.len() > 1).collect();
    for c in &mut components {
        c.sort_unstable();
    }
    components.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));

    let total = components.len();
    let mut out = Vec::new();
    for comp in components.iter().take(limit) {
        let starts = start_nodes(&adj, comp);
        let cycles = shortest_cycles(&adj, comp, &starts, max_length);
        let rendered: Vec<Value> = cycles
            .iter()
            .take(CYCLES_PER_COMPONENT)
            .map(|cycle| {
                let mut text = names[cycle[0]].clone();
                for (i, &from) in cycle.iter().enumerate() {
                    let to = cycle[(i + 1) % cycle.len()];
                    let kinds: Vec<&str> = deps[&(from, to)].iter().copied().collect();
                    text.push_str(&format!(" --{}--> {}", kinds.join(","), names[to]));
                }
                json!(text)
            })
            .collect();
        let mut v = json!({
            "size": comp.len(),
            "members": comp.iter().take(MEMBERS_SHOWN).map(|&u| names[u]).collect::<Vec<_>>(),
            "cycles": rendered,
        });
        if cycles.is_empty() {
            v["note"] = json!(format!("no cycle within max_length {max_length}"));
        } else if cycles.len() > CYCLES_PER_COMPONENT {
            v["more_cycles_found"] = json!(cycles.len() - CYCLES_PER_COMPONENT);
        }
        if starts.len() < comp.len() {
            v["searched_from"] = json!(format!(
                "{} of {} members (the most connected)",
                starts.len(),
                comp.len()
            ));
        }
        out.push(v);
    }
    let level_name = match level {
        Level::File => "file".to_string(),
        Level::Module(d) => format!("module (depth {d})"),
    };
    json!({
        "level": level_name,
        "cyclic_components": total,
        "truncated": total > limit,
        "components": out,
    })
}

/// Members to search cycles from: all of a small component, else the `MAX_STARTS` with
/// the most links inside it (ties by id).
fn start_nodes(adj: &[Vec<usize>], comp: &[usize]) -> Vec<usize> {
    if comp.len() <= MAX_STARTS {
        return comp.to_vec();
    }
    let inside: BTreeSet<usize> = comp.iter().copied().collect();
    let mut degree: HashMap<usize, usize> = HashMap::new();
    for &u in comp {
        for &w in adj[u].iter().filter(|w| inside.contains(w)) {
            *degree.entry(u).or_default() += 1;
            *degree.entry(w).or_default() += 1;
        }
    }
    let mut ranked = comp.to_vec();
    ranked.sort_by(|a, b| degree.get(b).cmp(&degree.get(a)).then(a.cmp(b)));
    ranked.truncate(MAX_STARTS);
    ranked.sort_unstable();
    ranked
}

/// Strongly connected components (iterative Tarjan).
fn tarjan(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut next = 0;
    let mut out = Vec::new();
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        // (node, next neighbour to visit)
        let mut call: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (v, ref mut i)) = call.last_mut() {
            if *i < adj[v].len() {
                let w = adj[v][*i];
                *i += 1;
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    call.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                call.pop();
                if let Some(&(parent, _)) = call.last() {
                    low[parent] = low[parent].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut comp = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    out.push(comp);
                }
            }
        }
    }
    out
}

/// For each start, one shortest cycle through it inside the component (BFS), at most
/// `max_length` long; deduplicated, rotated to start at the smallest unit, shortest
/// first. Not every cycle: other cycles of the same length may exist.
fn shortest_cycles(
    adj: &[Vec<usize>],
    comp: &[usize],
    starts: &[usize],
    max_length: usize,
) -> Vec<Vec<usize>> {
    let inside: BTreeSet<usize> = comp.iter().copied().collect();
    let mut found: BTreeSet<(usize, Vec<usize>)> = BTreeSet::new();
    for &start in starts {
        let mut prev: HashMap<usize, usize> = HashMap::new();
        let mut queue = VecDeque::from([(start, 1usize)]);
        let mut cycle = None;
        'bfs: while let Some((v, len)) = queue.pop_front() {
            for &w in &adj[v] {
                if !inside.contains(&w) {
                    continue;
                }
                if w == start {
                    let mut path = vec![v];
                    let mut cur = v;
                    while cur != start {
                        cur = prev[&cur];
                        path.push(cur);
                    }
                    path.reverse();
                    cycle = Some(path);
                    break 'bfs;
                }
                if len < max_length && !prev.contains_key(&w) && w != start {
                    prev.insert(w, v);
                    queue.push_back((w, len + 1));
                }
            }
        }
        if let Some(mut c) = cycle {
            let min = c
                .iter()
                .enumerate()
                .min_by_key(|&(_, &u)| u)
                .map(|(i, _)| i)
                .unwrap();
            c.rotate_left(min);
            found.insert((c.len(), c));
        }
    }
    found.into_iter().map(|(_, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tarjan_finds_components() {
        // 0 -> 1 -> 2 -> 0, 2 -> 3, 3 -> 4 -> 3, 5 alone
        let adj = vec![vec![1], vec![2], vec![0, 3], vec![4], vec![3], vec![]];
        let mut comps: Vec<Vec<usize>> = tarjan(&adj)
            .into_iter()
            .map(|mut c| {
                c.sort();
                c
            })
            .collect();
        comps.sort();
        assert_eq!(comps, vec![vec![0, 1, 2], vec![3, 4], vec![5]]);
    }

    #[test]
    fn shortest_cycles_are_minimal_and_bounded() {
        // 0 -> 1 -> 0 (length 2) and 0 -> 2 -> 3 -> 0 (length 3)
        let adj = vec![vec![1, 2], vec![0], vec![3], vec![0]];
        let comp = vec![0, 1, 2, 3];
        assert_eq!(
            shortest_cycles(&adj, &comp, &comp, 6),
            vec![vec![0, 1], vec![0, 2, 3]]
        );
        assert_eq!(shortest_cycles(&adj, &comp, &comp, 2), vec![vec![0, 1]]);
    }

    #[test]
    fn large_components_search_from_the_most_connected() {
        // A ring of 500 plus a hub (0) linked both ways with every member.
        let n = 500;
        let mut adj: Vec<Vec<usize>> = (0..n).map(|i| vec![(i + 1) % n]).collect();
        for i in 1..n {
            adj[0].push(i);
            adj[i].push(0);
        }
        let comp: Vec<usize> = (0..n).collect();
        let starts = start_nodes(&adj, &comp);
        assert_eq!(starts.len(), MAX_STARTS);
        assert_eq!(starts[0], 0, "the hub is searched");
    }

    #[test]
    fn deep_chains_do_not_overflow() {
        // A 100k-long ring: recursion would overflow the stack.
        let n = 100_000;
        let adj: Vec<Vec<usize>> = (0..n).map(|i| vec![(i + 1) % n]).collect();
        let comps = tarjan(&adj);
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].len(), n);
    }
}
