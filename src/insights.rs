//! Things worth a closer look: surprising cross-file connections, bridge nodes between
//! communities (approximate betweenness), and questions an LLM could ask about them.
//! Deterministic: every ranking breaks ties by node index, sampling is fixed.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde_json::{Value, json};

use crate::communities;
use crate::graph::{Confidence, Edge, EdgeKind, Graph, SymKind, dir_of};

/// Betweenness is computed from at most this many source nodes (evenly spaced).
const MAX_SOURCES: usize = 256;

fn is_dependency(e: &Edge) -> bool {
    e.kind != EdgeKind::Contains
}

fn active(g: &Graph, i: usize) -> bool {
    g.nodes[i].kind != SymKind::External
}

fn label(g: &Graph, c: Option<u32>) -> String {
    c.and_then(|c| g.communities.get(c as usize))
        .map_or_else(|| "?".into(), |info| info.label.clone())
}

type CommunityPair = (u32, u32);
type FilePair<'g> = (&'g str, &'g str);

struct Surprise {
    score: u32,
    edge: usize,
    reasons: Vec<String>,
}

/// A cross-file dependency worth scoring: between repository files, and not a guess across
/// languages (that is noise, not a finding).
fn candidate(g: &Graph, e: &Edge) -> bool {
    if !is_dependency(e) || !active(g, e.from) || !active(g, e.to) {
        return false;
    }
    let (a, b) = (&g.nodes[e.from], &g.nodes[e.to]);
    let languages_differ = match (a.lang, b.lang) {
        (Some(x), Some(y)) => x.family() != y.family(),
        _ => false,
    };
    a.file != b.file && !(languages_differ && !e.confidence.is_exact())
}

/// The two files of an edge, in a fixed order (a pair, whichever the direction).
fn file_pair<'g>(g: &'g Graph, e: &Edge) -> (&'g str, &'g str) {
    let (a, b) = (g.nodes[e.from].file.as_str(), g.nodes[e.to].file.as_str());
    if a <= b { (a, b) } else { (b, a) }
}

/// Cross-file dependency edges ranked by how unexpected they are; the best per file pair
/// (either direction).
fn surprising_connections(g: &Graph, degree: &[usize], hub: usize) -> Vec<Surprise> {
    // Distinct file pairs linking each pair of communities: a lone link is surprising,
    // however many edges (an import and a call) make it up.
    let mut links: BTreeSet<(CommunityPair, FilePair)> = BTreeSet::new();
    for e in g.edges.iter().filter(|e| candidate(g, e)) {
        if let (Some(a), Some(b)) = (g.nodes[e.from].community, g.nodes[e.to].community)
            && a != b
        {
            links.insert(((a.min(b), a.max(b)), file_pair(g, e)));
        }
    }
    let mut between: HashMap<(u32, u32), usize> = HashMap::new();
    for (pair, _) in &links {
        *between.entry(*pair).or_default() += 1;
    }

    let mut best: BTreeMap<(&str, &str), Surprise> = BTreeMap::new();
    for (i, e) in g.edges.iter().enumerate() {
        if !candidate(g, e) {
            continue;
        }
        let (a, b) = (&g.nodes[e.from], &g.nodes[e.to]);
        let mut score = 0;
        let mut reasons = Vec::new();
        match e.confidence {
            Confidence::Ambiguous => {
                score += 2;
                reasons.push("guessed among same-named candidates".to_string());
            }
            Confidence::Inferred => {
                score += 1;
                reasons.push("inferred, not certain".to_string());
            }
            Confidence::Exact => {}
        }
        if let (Some(ca), Some(cb)) = (a.community, b.community)
            && ca != cb
        {
            // Crossing alone is routine; crossing where almost nothing else does is not.
            score += 1;
            let rare = match between[&(ca.min(cb), ca.max(cb))] {
                1 => {
                    score += 3;
                    ", the only link between them"
                }
                2..=3 => {
                    score += 2;
                    ", one of few links between them"
                }
                _ => "",
            };
            reasons.push(format!(
                "crosses communities {} → {}{rare}",
                label(g, Some(ca)),
                label(g, Some(cb))
            ));
        } else if dir_of(&a.file) != dir_of(&b.file) {
            score += 1;
            reasons.push(format!(
                "crosses directories {} → {}",
                dir_of(&a.file),
                dir_of(&b.file)
            ));
        }
        if degree[e.from] <= 2 && degree[e.to] > hub {
            score += 1;
            reasons.push("from a peripheral node to a hub".to_string());
        }
        if score < 2 {
            continue;
        }
        let key = file_pair(g, e);
        match best.get(&key) {
            Some(old) if old.score >= score => {}
            _ => {
                best.insert(
                    key,
                    Surprise {
                        score,
                        edge: i,
                        reasons,
                    },
                );
            }
        }
    }
    let mut out: Vec<Surprise> = best.into_values().collect();
    out.sort_by(|x, y| y.score.cmp(&x.score).then(x.edge.cmp(&y.edge)));
    out
}

/// A dependency edge sure enough to shape the structure (guesses would invent bridges).
fn is_structural(e: &Edge) -> bool {
    is_dependency(e) && !e.is_ambiguous()
}

/// Approximate betweenness (Brandes) on the undirected graph of structural dependencies
/// between non-external nodes, from evenly spaced sources, scaled to the full graph.
fn betweenness(g: &Graph) -> Vec<f64> {
    let n = g.nodes.len();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in g.edges.iter().filter(|e| is_structural(e)) {
        if e.from != e.to && active(g, e.from) && active(g, e.to) {
            adj[e.from].push(e.to);
            adj[e.to].push(e.from);
        }
    }
    for list in &mut adj {
        list.sort_unstable();
        list.dedup();
    }
    let nodes: Vec<usize> = (0..n).filter(|&i| !adj[i].is_empty()).collect();
    let mut bc = vec![0.0; n];
    if nodes.is_empty() {
        return bc;
    }
    let k = nodes.len().min(MAX_SOURCES);
    let sources: Vec<usize> = (0..k).map(|j| nodes[j * nodes.len() / k]).collect();
    let mut sigma = vec![0.0f64; n];
    let mut dist = vec![-1i64; n];
    let mut delta = vec![0.0f64; n];
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &s in &sources {
        let mut order = Vec::new();
        for &v in &nodes {
            sigma[v] = 0.0;
            dist[v] = -1;
            delta[v] = 0.0;
            preds[v].clear();
        }
        sigma[s] = 1.0;
        dist[s] = 0;
        let mut queue = VecDeque::from([s]);
        while let Some(v) = queue.pop_front() {
            order.push(v);
            for &w in &adj[v] {
                if dist[w] < 0 {
                    dist[w] = dist[v] + 1;
                    queue.push_back(w);
                }
                if dist[w] == dist[v] + 1 {
                    sigma[w] += sigma[v];
                    preds[w].push(v);
                }
            }
        }
        for &w in order.iter().rev() {
            for &v in &preds[w] {
                delta[v] += sigma[v] / sigma[w] * (1.0 + delta[w]);
            }
            if w != s {
                bc[w] += delta[w];
            }
        }
    }
    let scale = nodes.len() as f64 / k as f64 / 2.0; // undirected: each pair counted twice
    for v in &mut bc {
        *v *= scale;
    }
    bc
}

pub fn insights(g: &Graph, top: usize) -> Value {
    let degree: Vec<usize> = (0..g.nodes.len())
        .map(|i| {
            if active(g, i) {
                communities::dependency_degree(g, i)
            } else {
                0
            }
        })
        .collect();
    // Hubs as community detection sees them (same percentile, same minimum degree).
    let is_active: Vec<bool> = (0..g.nodes.len()).map(|i| active(g, i)).collect();
    let hub = communities::hub_threshold(&degree, &is_active, g.hub_percentile);

    let surprises = surprising_connections(g, &degree, hub);
    let surprising: Vec<Value> = surprises
        .iter()
        .take(top)
        .map(|s| {
            let e = &g.edges[s.edge];
            let kind = match e.confidence.label() {
                Some(l) => format!("{} ({l})", e.kind.as_str()),
                None => e.kind.as_str().to_string(),
            };
            json!({
                "edge": format!("{} --{kind}--> {}", g.nodes[e.from].id, g.nodes[e.to].id),
                "line": format!("{}:{}", g.nodes[e.from].file, e.line),
                "score": s.score,
                "why": s.reasons.join("; "),
            })
        })
        .collect();

    // Bridges: highest betweenness among nodes whose neighbours lie in several
    // communities; those communities, most linked first.
    let bc = betweenness(g);
    let mut bridges: Vec<(usize, Vec<u32>)> = (0..g.nodes.len())
        .filter(|&i| bc[i] > 0.0)
        .filter_map(|i| {
            let mut links: BTreeMap<u32, usize> = BTreeMap::new();
            for &e in g.out_edges[i].iter().chain(&g.in_edges[i]) {
                let edge = &g.edges[e];
                if !is_structural(edge) {
                    continue;
                }
                let other = if edge.from == i { edge.to } else { edge.from };
                if let Some(c) = g.nodes[other].community {
                    *links.entry(c).or_default() += 1;
                }
            }
            let mut comms: Vec<(u32, usize)> = links.into_iter().collect();
            comms.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            (comms.len() >= 2).then(|| (i, comms.into_iter().map(|(c, _)| c).collect()))
        })
        .collect();
    bridges.sort_by(|a, b| bc[b.0].total_cmp(&bc[a.0]).then(a.0.cmp(&b.0)));
    bridges.truncate(top);
    let bridge_values: Vec<Value> = bridges
        .iter()
        .map(|(i, comms)| {
            json!({
                "id": g.nodes[*i].id,
                "kind": g.nodes[*i].kind.as_str(),
                "betweenness": (bc[*i] * 10.0).round() / 10.0,
                "connects": comms.iter().map(|&c| label(g, Some(c))).collect::<Vec<_>>(),
            })
        })
        .collect();

    // Questions: guesses to verify first, then why bridges join communities.
    let mut questions: Vec<String> = Vec::new();
    for s in surprises
        .iter()
        .filter(|s| g.edges[s.edge].is_ambiguous())
        .take(top.div_ceil(2))
    {
        let e = &g.edges[s.edge];
        questions.push(format!(
            "What exactly is the relation between {} and {}? (`{}` was resolved by name; check {}:{})",
            g.nodes[e.from].id,
            g.nodes[e.to].id,
            e.kind.as_str(),
            g.nodes[e.from].file,
            e.line
        ));
    }
    for (i, comms) in bridges.iter().take(top.div_ceil(2)) {
        let more = match comms.len() - 2 {
            0 => String::new(),
            n => format!(" (and {n} more)"),
        };
        questions.push(format!(
            "Why does {} connect the communities {} and {}{more}?",
            g.nodes[*i].id,
            label(g, Some(comms[0])),
            label(g, Some(comms[1])),
        ));
    }
    questions.truncate(top);

    json!({
        "surprising_connections": surprising,
        "bridges": bridge_values,
        "suggested_questions": questions,
        "hint": "Verify guessed edges with read_code; get_community and find_paths explain how communities connect.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Node;
    use std::path::Path;

    #[test]
    fn betweenness_peaks_at_the_bridge() {
        // Two triangles joined through node 3: 0-1-2-0, 2-3, 3-4, 4-5-6-4.
        let mut g = Graph::new(Path::new("/r"));
        for i in 0..7 {
            g.nodes.push(Node {
                id: format!("n{i}"),
                kind: SymKind::Function,
                name: format!("n{i}"),
                file: "m.rs".into(),
                lang: None,
                line: 1,
                end_line: 1,
                signature: String::new(),
                doc: None,
                parent: None,
                community: None,
            });
        }
        for (a, b) in [
            (0, 1),
            (1, 2),
            (2, 0),
            (2, 3),
            (3, 4),
            (4, 5),
            (5, 6),
            (6, 4),
        ] {
            g.edges.push(Edge {
                from: a,
                to: b,
                kind: EdgeKind::Calls,
                count: 1,
                confidence: Confidence::Exact,
                line: 1,
                lines: Vec::new(),
            });
        }
        g.build_indexes();
        let bc = betweenness(&g);
        let max = (0..7).max_by(|&a, &b| bc[a].total_cmp(&bc[b])).unwrap();
        assert_eq!(max, 3, "{bc:?}");
        // Exact on a small graph: node 3 lies on all 3×3 paths between the triangles.
        assert!((bc[3] - 9.0).abs() < 1e-9, "{bc:?}");
    }
}
