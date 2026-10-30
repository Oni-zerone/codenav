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

pub fn find_cycles(
    g: &Graph,
    level: Level,
    filter: EdgeFilter,
    max_length: usize,
    limit: usize,
) -> Value {
    // Units and the dependencies between them (aggregated from symbol-level edges).
    let unit_of = |i: usize| -> Option<String> {
        let n = &g.nodes[i];
        if n.kind == SymKind::External || n.file.is_empty() {
            return None;
        }
        Some(match level {
            Level::File => n.file.clone(),
            Level::Module(depth) => query::component(&n.file, depth),
        })
    };
    let mut deps: BTreeMap<(String, String), BTreeMap<&str, u32>> = BTreeMap::new();
    for e in &g.edges {
        if e.kind == EdgeKind::Contains || !filter.allows(e) {
            continue;
        }
        let (Some(a), Some(b)) = (unit_of(e.from), unit_of(e.to)) else {
            continue;
        };
        if a == b {
            continue;
        }
        *deps
            .entry((a, b))
            .or_default()
            .entry(e.kind.as_str())
            .or_default() += e.count;
    }
    // Ids follow the sorted unit names, so everything below is deterministic.
    let names: Vec<&String> = deps
        .keys()
        .flat_map(|(a, b)| [a, b])
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let ids: HashMap<&String, usize> = names.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); names.len()];
    for (a, b) in deps.keys() {
        adj[ids[a]].push(ids[b]);
    }
    for list in &mut adj {
        list.sort_unstable();
    }

    let mut components: Vec<Vec<usize>> =
        tarjan(&adj).into_iter().filter(|c| c.len() > 1).collect();
    for c in &mut components {
        c.sort_unstable_by_key(|&u| names[u]);
    }
    components.sort_by(|a, b| b.len().cmp(&a.len()).then(names[a[0]].cmp(names[b[0]])));

    let total = components.len();
    let mut out = Vec::new();
    for comp in components.iter().take(limit) {
        let cycles = shortest_cycles(&adj, comp, max_length);
        let rendered: Vec<Value> = cycles
            .iter()
            .take(CYCLES_PER_COMPONENT)
            .map(|cycle| {
                let mut text = names[cycle[0]].clone();
                for w in cycle
                    .windows(2)
                    .chain(std::iter::once(&[cycle[cycle.len() - 1], cycle[0]][..]))
                {
                    let kinds = &deps[&(names[w[0]].clone(), names[w[1]].clone())];
                    let kinds: Vec<&str> = kinds.keys().copied().collect();
                    text.push_str(&format!(" --{}--> {}", kinds.join(","), names[w[1]]));
                }
                json!(text)
            })
            .collect();
        let mut v = json!({
            "size": comp.len(),
            "members": comp.iter().take(MEMBERS_SHOWN).map(|&u| names[u]).collect::<Vec<_>>(),
            "shortest_cycles": rendered,
        });
        if cycles.is_empty() {
            v["note"] = json!(format!("no cycle within max_length {max_length}"));
        } else if cycles.len() > CYCLES_PER_COMPONENT {
            v["more_cycles"] = json!(cycles.len() - CYCLES_PER_COMPONENT);
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

/// For each member, the shortest cycle through it inside the component (BFS), at most
/// `max_length` long; deduplicated, rotated to start at the smallest unit, shortest first.
fn shortest_cycles(adj: &[Vec<usize>], comp: &[usize], max_length: usize) -> Vec<Vec<usize>> {
    let inside: BTreeSet<usize> = comp.iter().copied().collect();
    let mut found: BTreeSet<(usize, Vec<usize>)> = BTreeSet::new();
    for &start in comp {
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
            shortest_cycles(&adj, &comp, 6),
            vec![vec![0, 1], vec![0, 2, 3]]
        );
        assert_eq!(shortest_cycles(&adj, &comp, 2), vec![vec![0, 1]]);
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
