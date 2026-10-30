//! Structural communities: clusters of files and symbols that depend on each other more
//! than on the rest of the repository, found with Louvain modularity optimisation.
//!
//! The clustering graph is undirected and weighted: `calls`, `references`, `inherits` and
//! `imports` edges, plus `contains` so a file's symbols stay with their file. Dependency
//! edges of hubs (nodes whose dependency degree is above a percentile) are left out, so
//! widely used utilities don't glue unrelated parts together. External packages get no
//! community. Nodes are visited in index order and ties go to the lowest community, so
//! the result is deterministic: the same graph always gives the same ids.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::graph::{Edge, EdgeKind, Graph, SymKind};

/// Default percentile of dependency degree above which a node counts as a hub.
pub const DEFAULT_HUB_PERCENTILE: f64 = 99.0;
/// Below this degree a node is never a hub, so small repositories lose no edges.
const MIN_HUB_DEGREE: usize = 20;
/// Weight multiplier for edges whose target was guessed by name.
const AMBIGUOUS_WEIGHT: f64 = 0.5;
const MAX_LEVELS: usize = 20;
const MAX_PASSES: usize = 50;
const EPSILON: f64 = 1e-12;

/// One community, stored in the graph; its id is its index in `Graph::communities`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Community {
    /// Dominant directory, plus the most depended-on symbol when the directory is
    /// shared with another community.
    pub label: String,
    pub size: usize,
}

/// Assigns `node.community` to every non-external node and fills `g.communities`,
/// largest community first.
pub fn detect(g: &mut Graph, hub_percentile: f64) {
    let n = g.nodes.len();
    let active: Vec<bool> = g
        .nodes
        .iter()
        .map(|n| n.kind != SymKind::External)
        .collect();
    let dep_degree = dependency_degree(g, &active);
    let hub_limit = hub_threshold(&dep_degree, &active, hub_percentile);

    // Compact ids for active nodes.
    let mut local = vec![usize::MAX; n];
    let mut global = Vec::new();
    for i in (0..n).filter(|&i| active[i]) {
        local[i] = global.len();
        global.push(i);
    }
    let mut edges = Vec::with_capacity(g.edges.len());
    for e in &g.edges {
        if e.from == e.to || !active[e.from] || !active[e.to] {
            continue;
        }
        let hub = dep_degree[e.from] > hub_limit || dep_degree[e.to] > hub_limit;
        if e.kind != EdgeKind::Contains && hub {
            continue;
        }
        edges.push((local[e.from], local[e.to], weight(e)));
    }
    let membership = louvain(global.len(), &edges);

    // Renumber: largest first, ties by the smallest member id.
    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for (l, &c) in membership.iter().enumerate() {
        members.entry(c).or_default().push(global[l]);
    }
    let mut groups: Vec<Vec<usize>> = members.into_values().collect();
    for m in &mut groups {
        m.sort_by(|&a, &b| g.nodes[a].id.cmp(&g.nodes[b].id));
    }
    groups.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| g.nodes[a[0]].id.cmp(&g.nodes[b[0]].id))
    });

    for node in &mut g.nodes {
        node.community = None;
    }
    for (c, m) in groups.iter().enumerate() {
        for &i in m {
            g.nodes[i].community = Some(c as u32);
        }
    }
    g.communities = label_communities(g, &groups, &dep_degree);
}

/// Distinct dependency (non-`contains`) edges per node, between active nodes.
fn dependency_degree(g: &Graph, active: &[bool]) -> Vec<usize> {
    let mut deg = vec![0; g.nodes.len()];
    for e in &g.edges {
        if e.kind != EdgeKind::Contains && e.from != e.to && active[e.from] && active[e.to] {
            deg[e.from] += 1;
            deg[e.to] += 1;
        }
    }
    deg
}

/// Degree above which a node is a hub.
fn hub_threshold(deg: &[usize], active: &[bool], percentile: f64) -> usize {
    let mut sorted: Vec<usize> = (0..deg.len())
        .filter(|&i| active[i])
        .map(|i| deg[i])
        .collect();
    if sorted.is_empty() || percentile >= 100.0 {
        return usize::MAX;
    }
    sorted.sort_unstable();
    let rank = ((percentile.max(0.0) / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank].max(MIN_HUB_DEGREE)
}

fn weight(e: &Edge) -> f64 {
    let w = 1.0 + f64::from(e.count.max(1)).ln();
    if e.ambiguous { w * AMBIGUOUS_WEIGHT } else { w }
}

/// Undirected weighted graph; `adj[i]` holds merged (neighbour, weight) pairs and a
/// self-loop entry for the internal weight of aggregated nodes.
struct WGraph {
    adj: Vec<Vec<(usize, f64)>>,
}

impl WGraph {
    fn new(n: usize, edges: &[(usize, usize, f64)]) -> WGraph {
        let mut raw: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
        for &(a, b, w) in edges {
            raw[a].push((b, w));
            if a != b {
                raw[b].push((a, w));
            }
        }
        let adj = raw
            .into_iter()
            .map(|mut list| {
                list.sort_unstable_by_key(|&(j, _)| j);
                let mut merged: Vec<(usize, f64)> = Vec::with_capacity(list.len());
                for (j, w) in list {
                    match merged.last_mut() {
                        Some(last) if last.0 == j => last.1 += w,
                        _ => merged.push((j, w)),
                    }
                }
                merged
            })
            .collect();
        WGraph { adj }
    }

    fn len(&self) -> usize {
        self.adj.len()
    }
}

/// Community of each of the `n` nodes (arbitrary ids, not compacted).
fn louvain(n: usize, edges: &[(usize, usize, f64)]) -> Vec<usize> {
    let mut graph = WGraph::new(n, edges);
    let mut membership: Vec<usize> = (0..n).collect();
    for _ in 0..MAX_LEVELS {
        let (comm, moved) = local_moving(&graph);
        if !moved {
            break;
        }
        // Compact the community ids and aggregate.
        let mut index = vec![usize::MAX; graph.len()];
        let mut count = 0;
        for &c in &comm {
            if index[c] == usize::MAX {
                index[c] = count;
                count += 1;
            }
        }
        for m in &mut membership {
            *m = index[comm[*m]];
        }
        let mut agg = Vec::new();
        for (i, list) in graph.adj.iter().enumerate() {
            for &(j, w) in list {
                if i <= j {
                    agg.push((index[comm[i]], index[comm[j]], w));
                }
            }
        }
        graph = WGraph::new(count, &agg);
    }
    membership
}

/// One Louvain level: moves nodes between communities while modularity improves.
/// Returns each node's community and whether anything moved.
fn local_moving(g: &WGraph) -> (Vec<usize>, bool) {
    let n = g.len();
    // k[i]: weighted degree, self-loops counted twice (they hold internal weight once).
    let k: Vec<f64> = g
        .adj
        .iter()
        .enumerate()
        .map(|(i, l)| {
            l.iter()
                .map(|&(j, w)| if j == i { 2.0 * w } else { w })
                .sum()
        })
        .collect();
    let m2: f64 = k.iter().sum();
    let mut comm: Vec<usize> = (0..n).collect();
    if m2 <= 0.0 {
        return (comm, false);
    }
    let mut tot = k.clone();
    let mut neigh_w = vec![0.0; n];
    let mut touched: Vec<usize> = Vec::new();
    let mut moved_any = false;

    for _ in 0..MAX_PASSES {
        let mut moved = false;
        for i in 0..n {
            let own = comm[i];
            for &(j, w) in &g.adj[i] {
                if j == i {
                    continue;
                }
                let c = comm[j];
                if neigh_w[c] == 0.0 {
                    touched.push(c);
                }
                neigh_w[c] += w;
            }
            tot[own] -= k[i];
            let gain = |c: usize, kin: f64| kin - tot[c] * k[i] / m2;
            let mut best = own;
            let mut best_gain = gain(own, neigh_w[own]);
            touched.sort_unstable();
            for &c in &touched {
                let gc = gain(c, neigh_w[c]);
                if gc > best_gain + EPSILON {
                    best = c;
                    best_gain = gc;
                }
            }
            tot[best] += k[i];
            if best != own {
                comm[i] = best;
                moved = true;
            }
            for &c in &touched {
                neigh_w[c] = 0.0;
            }
            touched.clear();
        }
        if !moved {
            break;
        }
        moved_any = true;
    }
    (comm, moved_any)
}

/// Labels a community by its dominant directory. When several communities share it,
/// the dominant file (if it holds at least half the members) or the most depended-on
/// symbol tells them apart; leftover duplicates are numbered.
fn label_communities(g: &Graph, groups: &[Vec<usize>], dep_degree: &[usize]) -> Vec<Community> {
    let dirs: Vec<String> = groups
        .iter()
        .map(|m| {
            let (dir, _) = dominant(m, |i| {
                g.nodes[i].file.rsplit_once('/').map_or("", |(d, _)| d)
            });
            if dir.is_empty() {
                "(root)".to_string()
            } else {
                dir.to_string()
            }
        })
        .collect();
    let top_symbol = |members: &[usize]| -> Option<String> {
        members
            .iter()
            .copied()
            .filter(|&i| g.nodes[i].kind != SymKind::File)
            .max_by(|&a, &b| {
                dep_degree[a]
                    .cmp(&dep_degree[b])
                    .then_with(|| g.nodes[b].id.cmp(&g.nodes[a].id))
            })
            .map(|i| g.qualified_name(i))
    };
    let with_symbol = |base: &str, members: &[usize]| match top_symbol(members) {
        Some(sym) => format!("{base} · {sym}"),
        None => base.to_string(),
    };

    let dir_uses = uses(&dirs);
    let mut labels: Vec<String> = groups
        .iter()
        .zip(&dirs)
        .map(|(members, dir)| {
            if dir_uses[dir.as_str()] == 1 {
                return dir.clone();
            }
            match dominant(members, |i| g.nodes[i].file.as_str()) {
                (file, n) if 2 * n >= members.len() && !file.is_empty() => file.to_string(),
                _ => with_symbol(dir, members),
            }
        })
        .collect();
    // Two communities dominated by the same file.
    let file_uses = uses(&labels);
    for (label, members) in labels.iter_mut().zip(groups) {
        if file_uses[label.as_str()] > 1 && !label.contains(" · ") {
            *label = with_symbol(label, members);
        }
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    labels
        .into_iter()
        .zip(groups)
        .map(|(mut label, members)| {
            let n = seen.entry(label.clone()).or_default();
            *n += 1;
            if *n > 1 {
                label = format!("{label} #{n}");
            }
            Community {
                label,
                size: members.len(),
            }
        })
        .collect()
}

/// The most frequent key among `members` and its count; ties go to the smallest key.
fn dominant<'a>(members: &[usize], key: impl Fn(usize) -> &'a str) -> (&'a str, usize) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for &i in members {
        *counts.entry(key(i)).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .unwrap_or(("", 0))
}

fn uses(labels: &[String]) -> HashMap<String, usize> {
    let mut m: HashMap<String, usize> = HashMap::new();
    for l in labels {
        *m.entry(l.clone()).or_default() += 1;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn clique(start: usize, size: usize, edges: &mut Vec<(usize, usize, f64)>) {
        for a in start..start + size {
            for b in a + 1..start + size {
                edges.push((a, b, 1.0));
            }
        }
    }

    #[test]
    fn two_cliques_joined_by_one_edge_split() {
        let mut edges = Vec::new();
        clique(0, 5, &mut edges);
        clique(5, 5, &mut edges);
        edges.push((4, 5, 1.0));
        let m = louvain(10, &edges);
        assert!(m[..5].iter().all(|&c| c == m[0]), "{m:?}");
        assert!(m[5..].iter().all(|&c| c == m[5]), "{m:?}");
        assert_ne!(m[0], m[5]);
    }

    #[test]
    fn isolated_nodes_stay_alone_and_empty_graphs_work() {
        assert_eq!(louvain(3, &[]), vec![0, 1, 2]);
        assert!(louvain(0, &[]).is_empty());
    }

    #[test]
    fn hub_threshold_ignores_small_degrees() {
        let deg = vec![1, 2, 3, 30];
        let active = vec![true; 4];
        assert_eq!(hub_threshold(&deg, &active, 50.0), MIN_HUB_DEGREE);
        assert_eq!(hub_threshold(&deg, &active, 100.0), usize::MAX);
        let deg: Vec<usize> = (0..1000).collect();
        let active = vec![true; 1000];
        assert_eq!(hub_threshold(&deg, &active, 99.0), 989);
    }

    /// 300 clusters of 100 nodes (30k nodes), sparse inside, a few links between.
    fn synthetic(clusters: usize, size: usize) -> Vec<(usize, usize, f64)> {
        let mut edges = Vec::new();
        let mut x: u64 = 42;
        let mut rand = |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        for c in 0..clusters {
            let base = c * size;
            for i in 0..size {
                for _ in 0..4 {
                    edges.push((base + i, base + rand(size), 1.0));
                }
            }
            for _ in 0..3 {
                edges.push((base + rand(size), rand(clusters) * size + rand(size), 1.0));
            }
        }
        edges
    }

    #[test]
    fn finds_planted_clusters_deterministically() {
        let edges = synthetic(50, 40);
        let a = louvain(2000, &edges);
        assert_eq!(a, louvain(2000, &edges));
        // Most clusters are recovered whole.
        let whole = (0..50)
            .filter(|c| a[c * 40..(c + 1) * 40].iter().all(|&x| x == a[c * 40]))
            .count();
        assert!(whole >= 45, "only {whole}/50 clusters recovered");
    }

    #[test]
    fn thirty_thousand_nodes_in_under_a_second() {
        let edges = synthetic(300, 100);
        let started = Instant::now();
        let m = louvain(30_000, &edges);
        let elapsed = started.elapsed();
        assert_eq!(m.len(), 30_000);
        // Debug builds are several times slower; the target is for release builds.
        let limit = if cfg!(debug_assertions) { 5.0 } else { 1.0 };
        assert!(elapsed.as_secs_f64() < limit, "{elapsed:?}");
    }
}
