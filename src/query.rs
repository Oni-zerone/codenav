//! Read-only graph analyses used by the MCP tools and the CLI. Every function returns a
//! JSON value meant to be read by an LLM: compact, with stable ids it can pass back.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::communities;
use crate::graph::{Confidence, EdgeKind, Graph, SymKind};

const MAX_CODE_LINES: usize = 150;
const MAX_LINE_BYTES: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

impl Direction {
    pub fn parse(s: &str) -> Option<Direction> {
        Some(match s {
            "out" | "outgoing" => Direction::Out,
            "in" | "incoming" => Direction::In,
            "both" | "any" => Direction::Both,
            _ => return None,
        })
    }
}

pub fn summary(g: &Graph, i: usize) -> Value {
    // The id already carries the file and the name.
    let n = &g.nodes[i];
    let mut v = json!({ "id": n.id, "kind": n.kind.as_str() });
    if !n.file.is_empty() {
        v["lines"] = json!(format!("{}-{}", n.line, n.end_line));
    }
    if !n.signature.is_empty() {
        v["signature"] = json!(n.signature);
    }
    v
}

/// Accepts an exact id, a file path, a qualified suffix (`Graph::load`) or a bare name.
pub fn resolve(g: &Graph, query: &str) -> Result<usize> {
    let query = query.trim();
    if query.is_empty() {
        bail!("empty node reference");
    }
    if let Some(&i) = g.by_id.get(query).or(g.by_file.get(query)) {
        return Ok(i);
    }
    let mut cands: Vec<usize> = g
        .by_name
        .get(&query.to_lowercase())
        .cloned()
        .unwrap_or_default();
    if cands.is_empty() {
        let suffix = format!("::{query}");
        cands = (0..g.nodes.len())
            .filter(|&i| g.nodes[i].id.ends_with(&suffix) || g.nodes[i].file.ends_with(query))
            .collect();
        cands.retain(|&i| g.nodes[i].kind == SymKind::File || g.nodes[i].id.ends_with(&suffix));
    }
    match cands.len() {
        0 => bail!("no node matches `{query}`; use search_symbols to find valid ids"),
        1 => Ok(cands[0]),
        _ => {
            cands.sort_by_key(|&i| std::cmp::Reverse(g.degree(i)));
            let list: Vec<String> = cands
                .iter()
                .take(15)
                .map(|&i| g.nodes[i].id.clone())
                .collect();
            bail!(
                "`{query}` is ambiguous ({} matches), pass one of these ids:\n{}",
                cands.len(),
                list.join("\n")
            )
        }
    }
}

pub fn search(
    g: &Graph,
    query: &str,
    kinds: &[SymKind],
    path_filter: Option<&str>,
    limit: usize,
) -> Value {
    let q = query.trim().to_lowercase();
    let terms: Vec<&str> = q.split_whitespace().collect();
    let mut scored: Vec<(i64, usize)> = Vec::new();
    for (i, n) in g.nodes.iter().enumerate() {
        if !kinds.is_empty() && !kinds.contains(&n.kind) {
            continue;
        }
        if let Some(p) = path_filter
            && !n.file.contains(p)
        {
            continue;
        }
        let name = n.name.to_lowercase();
        let id = n.id.to_lowercase();
        let mut score = if terms.is_empty() {
            1
        } else if name == q {
            100
        } else if name.starts_with(&q) {
            60
        } else if name.contains(&q) {
            40
        } else if id.contains(&q) {
            25
        } else if terms.iter().all(|t| id.contains(t)) {
            15
        } else if n.doc.as_ref().is_some_and(|d| {
            let d = d.to_lowercase();
            terms.iter().all(|t| d.contains(t))
        }) {
            10
        } else {
            0
        };
        if score == 0 {
            continue;
        }
        // Prefer well-connected symbols over files and externals on ties.
        score = score * 1000 + (g.degree(i).min(999) as i64);
        if matches!(n.kind, SymKind::File | SymKind::External) {
            score -= 500;
        }
        scored.push((score, i));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let total = scored.len();
    let results: Vec<Value> = scored
        .iter()
        .take(limit)
        .map(|&(_, i)| summary(g, i))
        .collect();
    json!({ "total_matches": total, "results": results })
}

pub fn symbol_details(g: &Graph, root: &Path, i: usize, include_code: bool) -> Value {
    let n = &g.nodes[i];
    let mut v = summary(g, i);
    if let Some(doc) = &n.doc {
        v["doc"] = json!(doc);
    }
    if let Some(p) = n.parent {
        v["parent"] = json!(g.nodes[p].id);
    }
    if let Some(c) = n.community {
        v["community"] = community_ref(g, c);
    }

    let mut outgoing: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    let mut incoming: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for &e in &g.out_edges[i] {
        let e = &g.edges[e];
        outgoing
            .entry(e.kind.as_str())
            .or_default()
            .push(edge_ref(g, e.to, e));
    }
    for &e in &g.in_edges[i] {
        let e = &g.edges[e];
        if e.kind == EdgeKind::Contains {
            continue; // already reported as `parent`
        }
        incoming
            .entry(e.kind.inverse_str())
            .or_default()
            .push(edge_ref(g, e.from, e));
    }
    v["outgoing"] = cap_lists(outgoing, 40);
    v["incoming"] = cap_lists(incoming, 40);

    if include_code
        && !n.file.is_empty()
        && let Some(code) = read_lines(root, &n.file, n.line, n.end_line)
    {
        v["code"] = json!(code);
    }
    v
}

fn edge_ref(g: &Graph, other: usize, e: &crate::graph::Edge) -> Value {
    let mut v = json!(g.nodes[other].id);
    if e.count > 1 || !e.confidence.is_exact() {
        let mut o = json!({ "id": g.nodes[other].id });
        if e.count > 1 {
            o["count"] = json!(e.count);
        }
        if let Some(l) = e.confidence.label() {
            o["confidence"] = json!(l);
        }
        v = o;
    }
    v
}

fn cap_lists(map: BTreeMap<&str, Vec<Value>>, max: usize) -> Value {
    let mut out = serde_json::Map::new();
    for (k, mut list) in map {
        let total = list.len();
        if total > max {
            list.truncate(max);
            list.push(json!(format!("… and {} more", total - max)));
        }
        out.insert(k.to_string(), json!(list));
    }
    Value::Object(out)
}

/// Minified or generated code can have huge lines: keep the start and say how much is cut.
fn clip_line(line: &str) -> std::borrow::Cow<'_, str> {
    if line.len() <= MAX_LINE_BYTES {
        return line.into();
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} …[+{} bytes]", &line[..end], line.len() - end).into()
}

pub fn read_lines(root: &Path, file: &str, start: u32, end: u32) -> Option<String> {
    let text = fs::read_to_string(root.join(file)).ok()?;
    let start = start.max(1) as usize;
    let end = (end as usize).max(start);
    let mut out = String::new();
    for (shown, (n, line)) in text
        .lines()
        .enumerate()
        .skip(start - 1)
        .take(end - start + 1)
        .enumerate()
    {
        if shown == MAX_CODE_LINES {
            out.push_str(&format!("… ({} more lines)\n", end - n));
            break;
        }
        out.push_str(&format!("{:>5} | {}\n", n + 1, clip_line(line)));
    }
    Some(out)
}

/// One traversal step: (neighbor, edge index, edge followed forward?).
pub type Step = (usize, usize, bool);

fn edge_allowed(kinds: &[EdgeKind], k: EdgeKind) -> bool {
    kinds.is_empty() || kinds.contains(&k)
}

/// Which edges a traversal may follow.
#[derive(Debug, Clone, Copy)]
pub struct EdgeFilter<'a> {
    /// Allowed kinds (empty: all).
    pub kinds: &'a [EdgeKind],
    /// Least confidence followed.
    pub min: Confidence,
}

impl EdgeFilter<'_> {
    pub fn allows(&self, e: &crate::graph::Edge) -> bool {
        edge_allowed(self.kinds, e.kind) && e.confidence >= self.min
    }
}

/// Steps from `i` along edges allowed by `filter` in the given direction:
/// (neighbor, edge, outgoing?).
pub fn steps<'a>(
    g: &'a Graph,
    i: usize,
    dir: Direction,
    filter: EdgeFilter<'a>,
) -> impl Iterator<Item = Step> + 'a {
    let out = matches!(dir, Direction::Out | Direction::Both)
        .then(|| {
            g.out_edges[i]
                .iter()
                .map(move |&e| (g.edges[e].to, e, true))
        })
        .into_iter()
        .flatten();
    let inc = matches!(dir, Direction::In | Direction::Both)
        .then(|| {
            g.in_edges[i]
                .iter()
                .map(move |&e| (g.edges[e].from, e, false))
        })
        .into_iter()
        .flatten();
    out.chain(inc)
        .filter(move |&(_, e, _)| filter.allows(&g.edges[e]))
}

pub fn neighbors(
    g: &Graph,
    start: usize,
    dir: Direction,
    filter: EdgeFilter,
    depth: u32,
    limit: usize,
) -> Value {
    let mut seen = HashSet::from([start]);
    let mut queue = VecDeque::from([(start, 0u32)]);
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut truncated = false;
    while let Some((cur, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        for (next, e, _) in steps(g, cur, dir, filter) {
            let edge = &g.edges[e];
            if edges.len() >= limit * 2 {
                truncated = true;
                break;
            }
            let mut ev = json!({
                "from": g.nodes[edge.from].id,
                "kind": edge.kind.as_str(),
                "to": g.nodes[edge.to].id,
            });
            if edge.count > 1 {
                ev["count"] = json!(edge.count);
            }
            if let Some(l) = edge.confidence.label() {
                ev["confidence"] = json!(l);
            }
            edges.push(ev);
            if seen.insert(next) {
                if nodes.len() >= limit {
                    truncated = true;
                    continue;
                }
                let mut s = summary(g, next);
                s["depth"] = json!(d + 1);
                nodes.push(s);
                queue.push_back((next, d + 1));
            }
        }
    }
    json!({
        "start": summary(g, start),
        "nodes": nodes,
        "edges": edges,
        "truncated": truncated,
    })
}

/// Up to `max_paths` shortest paths between two nodes.
pub fn find_paths(
    g: &Graph,
    from: usize,
    to: usize,
    filter: EdgeFilter,
    directed: bool,
    max_depth: u32,
    max_paths: usize,
) -> Value {
    let dir = if directed {
        Direction::Out
    } else {
        Direction::Both
    };
    let mut dist: HashMap<usize, u32> = HashMap::from([(from, 0)]);
    let mut preds: HashMap<usize, Vec<Step>> = HashMap::new();
    let mut queue = VecDeque::from([from]);
    while let Some(cur) = queue.pop_front() {
        let d = dist[&cur];
        if cur == to || d >= max_depth {
            continue;
        }
        for (next, e, fwd) in steps(g, cur, dir, filter) {
            match dist.get(&next) {
                None => {
                    dist.insert(next, d + 1);
                    preds.entry(next).or_default().push((cur, e, fwd));
                    queue.push_back(next);
                }
                Some(&nd) if nd == d + 1 => preds.entry(next).or_default().push((cur, e, fwd)),
                _ => {}
            }
        }
    }
    if !dist.contains_key(&to) {
        return json!({
            "found": false,
            "message": format!("no path within {max_depth} hops; try directed=false, more edge kinds or a larger max_depth"),
        });
    }

    // Enumerate shortest paths backwards from `to`.
    let mut paths: Vec<Vec<Step>> = Vec::new();
    let mut stack: Vec<(usize, Vec<Step>)> = vec![(to, Vec::new())];
    while let Some((cur, acc)) = stack.pop() {
        if paths.len() >= max_paths {
            break;
        }
        if cur == from {
            let mut p = acc.clone();
            p.reverse();
            paths.push(p);
            continue;
        }
        for &(prev, e, fwd) in preds.get(&cur).into_iter().flatten() {
            let mut next = acc.clone();
            next.push((prev, e, fwd));
            stack.push((prev, next));
        }
    }

    let rendered: Vec<Value> = paths
        .iter()
        .map(|p| {
            let mut text = g.nodes[from].id.clone();
            for &(_, e, fwd) in p {
                let edge = &g.edges[e];
                let kind = match edge.confidence.label() {
                    Some(l) => format!("{} ({l})", edge.kind.as_str()),
                    None => edge.kind.as_str().to_string(),
                };
                let (arrow, other) = if fwd {
                    (format!(" --{kind}--> "), edge.to)
                } else {
                    (format!(" <--{kind}-- "), edge.from)
                };
                text.push_str(&arrow);
                text.push_str(&g.nodes[other].id);
            }
            json!(text)
        })
        .collect();
    json!({ "found": true, "length": dist[&to], "paths": rendered })
}

pub fn file_outline(g: &Graph, file: usize) -> Value {
    fn tree(g: &Graph, i: usize, depth: u32) -> Vec<Value> {
        let mut children: Vec<usize> = g.out_edges[i]
            .iter()
            .map(|&e| &g.edges[e])
            .filter(|e| e.kind == EdgeKind::Contains && g.nodes[e.to].file == g.nodes[i].file)
            .map(|e| e.to)
            .collect();
        children.sort_by_key(|&c| g.nodes[c].line);
        children
            .into_iter()
            .map(|c| {
                let mut v = summary(g, c);
                if let Some(doc) = &g.nodes[c].doc {
                    v["doc"] = json!(doc);
                }
                if depth < 4 {
                    let sub = tree(g, c, depth + 1);
                    if !sub.is_empty() {
                        v["members"] = json!(sub);
                    }
                }
                v
            })
            .collect()
    }
    let imports: Vec<&str> = g.out_edges[file]
        .iter()
        .map(|&e| &g.edges[e])
        .filter(|e| e.kind == EdgeKind::Imports)
        .map(|e| g.nodes[e.to].id.as_str())
        .collect();
    let mut imported_by: Vec<&str> = g.in_edges[file]
        .iter()
        .map(|&e| &g.edges[e])
        .filter(|e| e.kind == EdgeKind::Imports)
        .map(|e| g.nodes[e.from].id.as_str())
        .collect();
    imported_by.sort();

    // Files whose symbols interact with this file's symbols (calls/inheritance/types).
    let mut related: HashMap<&str, (u32, u32)> = HashMap::new();
    for (i, n) in g.nodes.iter().enumerate() {
        if n.file != g.nodes[file].file || n.kind == SymKind::File {
            continue;
        }
        for &e in &g.out_edges[i] {
            let e = &g.edges[e];
            let other = &g.nodes[e.to].file;
            if e.kind != EdgeKind::Contains && !other.is_empty() && other != &n.file {
                related.entry(other).or_default().0 += e.count;
            }
        }
        for &e in &g.in_edges[i] {
            let e = &g.edges[e];
            let other = &g.nodes[e.from].file;
            if e.kind != EdgeKind::Contains && !other.is_empty() && other != &n.file {
                related.entry(other).or_default().1 += e.count;
            }
        }
    }
    let mut related: Vec<(&str, (u32, u32))> = related.into_iter().collect();
    related.sort_by_key(|(f, (o, i))| (std::cmp::Reverse(o + i), *f));
    let related: Vec<Value> = related
        .iter()
        .take(25)
        .map(|(f, (o, i))| json!({ "file": f, "uses": o, "used_by": i }))
        .collect();

    json!({
        "file": g.nodes[file].id,
        "lines": g.nodes[file].end_line,
        "language": g.nodes[file].lang,
        "symbols": tree(g, file, 0),
        "imports": imports,
        "imported_by": imported_by,
        "related_files": related,
    })
}

/// What could break if `start` changes: everything that transitively depends on it
/// (or on its members) through calls, inheritance, type references and imports.
///
/// A method also stands in for the members it overrides: a call through the interface
/// or base class (`repo.load()` resolved to `Repository::load`) reaches it at runtime.
///
/// Only edges at least as sure as `min` are followed; each hit reports the confidence of
/// the weakest edge on the path that reached it.
/// Edge kinds through which code depends on other code (everything but containment).
pub const DEPENDENCY_KINDS: [EdgeKind; 4] = [
    EdgeKind::Calls,
    EdgeKind::Inherits,
    EdgeKind::References,
    EdgeKind::Imports,
];

/// Nodes with the confidence of how they were reached.
pub type Seeds = Vec<(usize, Confidence)>;

/// What a change to `start` touches: `start`, its members (through containment at least
/// `min` sure), and the members they override, each with the confidence of how it was
/// reached. Shared by impact_analysis and tests_for.
pub fn change_seeds(g: &Graph, start: usize, min: Confidence) -> (Seeds, Seeds) {
    let mut seeds: Seeds = vec![(start, Confidence::Exact)];
    let mut i = 0;
    while i < seeds.len() {
        let (s, conf) = seeds[i];
        for &e in &g.out_edges[s] {
            let edge = &g.edges[e];
            if edge.kind == EdgeKind::Contains && edge.confidence >= min {
                seeds.push((edge.to, conf.min(edge.confidence)));
            }
        }
        i += 1;
    }
    let mut overridden: Vec<(usize, Confidence)> = Vec::new();
    for &(s, conf) in &seeds {
        for (m, via) in overridden_members(g, s, min) {
            let known = seeds.iter().chain(&overridden).any(|&(x, _)| x == m);
            if !known {
                overridden.push((m, conf.min(via)));
            }
        }
    }
    (seeds, overridden)
}

pub fn impact(g: &Graph, start: usize, depth: u32, limit: usize, min: Confidence) -> Value {
    let (mut seeds, overridden) = change_seeds(g, start, min);
    seeds.extend(&overridden);
    let seed_set: HashSet<usize> = seeds.iter().map(|&(s, _)| s).collect();
    let kinds = DEPENDENCY_KINDS;

    // One BFS per confidence level, surest first, following only edges (and seeds) at least
    // that sure: a node gets the best confidence any path within `depth` gives it, and the
    // shortest depth at that level. So the label does not depend on edge order.
    let mut best: HashMap<usize, (u32, EdgeKind, Confidence)> = HashMap::new();
    for level in Confidence::ALL.into_iter().filter(|&l| l >= min) {
        let mut seen = seed_set.clone();
        let mut queue: VecDeque<(usize, u32)> = seeds
            .iter()
            .filter(|&&(_, c)| c >= level)
            .map(|&(s, _)| (s, 0))
            .collect();
        while let Some((cur, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            for &e in &g.in_edges[cur] {
                let edge = &g.edges[e];
                if !kinds.contains(&edge.kind) || edge.confidence < level || !seen.insert(edge.from)
                {
                    continue;
                }
                best.entry(edge.from).or_insert((d + 1, edge.kind, level));
                queue.push_back((edge.from, d + 1));
            }
        }
    }
    let mut hits: Vec<(usize, u32, EdgeKind, Confidence)> = best
        .into_iter()
        .map(|(n, (d, k, c))| (n, d, k, c))
        .collect();
    hits.sort_by(|a, b| a.1.cmp(&b.1).then(b.3.cmp(&a.3)).then(a.0.cmp(&b.0)));

    let mut by_file: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for &(n, d, k, conf) in hits.iter().take(limit) {
        let node = &g.nodes[n];
        let guess = conf.label().map(|l| format!(", {l}")).unwrap_or_default();
        let label = if node.kind == SymKind::File {
            format!("(file) via {} [depth {d}{guess}]", k.inverse_str())
        } else {
            format!(
                "{} ({}) via {} [depth {d}{guess}]",
                g.qualified_name(n),
                node.kind.as_str(),
                k.inverse_str()
            )
        };
        by_file
            .entry(node.file.clone())
            .or_default()
            .push(json!(label));
    }
    let mut out = json!({
        "target": summary(g, start),
        "members_considered": seed_set.len(),
    });
    // Listed before `by_file` so a text output cut to the budget keeps it.
    if !overridden.is_empty() {
        out["overridden_members"] = json!(
            overridden
                .iter()
                .map(|&(m, conf)| match conf.label() {
                    Some(l) => format!("{} ({l})", g.nodes[m].id),
                    None => g.nodes[m].id.clone(),
                })
                .collect::<Vec<_>>()
        );
    }
    out["affected_symbols"] = json!(
        hits.iter()
            .filter(|h| g.nodes[h.0].kind != SymKind::File)
            .count()
    );
    out["affected_files"] = json!(by_file.len());
    out["truncated"] = json!(hits.len() > limit);
    out["by_file"] = json!(by_file);
    out
}

/// Methods with the same name as method `m` in the supertypes of its type, nearest
/// first, following `inherits` edges at least `min` sure transitively. Each comes with
/// the confidence of the weakest link that reached it.
fn overridden_members(g: &Graph, m: usize, min: Confidence) -> Vec<(usize, Confidence)> {
    let node = &g.nodes[m];
    if node.kind != SymKind::Method {
        return vec![];
    }
    let Some(owner) = node.parent.filter(|&p| g.nodes[p].kind.is_type()) else {
        return vec![];
    };
    let mut found = Vec::new();
    let mut seen = HashSet::from([owner]);
    let mut queue = VecDeque::from([(owner, Confidence::Exact)]);
    while let Some((t, conf)) = queue.pop_front() {
        for &e in &g.out_edges[t] {
            let edge = &g.edges[e];
            if edge.kind != EdgeKind::Inherits || edge.confidence < min || !seen.insert(edge.to) {
                continue;
            }
            let base = edge.to;
            let conf = conf.min(edge.confidence);
            found.extend(
                g.out_edges[base]
                    .iter()
                    .filter(|&&c| g.edges[c].kind == EdgeKind::Contains)
                    .map(|&c| g.edges[c].to)
                    .filter(|&c| g.nodes[c].kind == SymKind::Method && g.nodes[c].name == node.name)
                    .map(|c| (c, conf)),
            );
            queue.push_back((base, conf));
        }
    }
    found
}

/// The component of `file` at directory `depth` (`.` for top-level files), as used by
/// module_graph and find_cycles.
pub fn component(file: &str, depth: usize) -> String {
    let parts: Vec<&str> = file.split('/').collect();
    if parts.len() <= 1 {
        return ".".to_string();
    }
    parts[..(parts.len() - 1).min(depth)].join("/")
}

/// Component-level view: edges aggregated between directories truncated at `depth`.
pub fn module_graph(g: &Graph, depth: usize, min_weight: u32) -> Value {
    let component = |file: &str| component(file, depth);
    let mut sizes: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for n in &g.nodes {
        if n.file.is_empty() {
            continue;
        }
        let c = sizes.entry(component(&n.file)).or_default();
        if n.kind == SymKind::File {
            c.0 += 1;
        } else {
            c.1 += 1;
        }
    }
    let mut deps: HashMap<(String, String), BTreeMap<&str, u32>> = HashMap::new();
    let mut ext: HashMap<String, HashSet<String>> = HashMap::new();
    for e in &g.edges {
        if e.kind == EdgeKind::Contains {
            continue;
        }
        let (a, b) = (&g.nodes[e.from], &g.nodes[e.to]);
        if b.kind == SymKind::External {
            ext.entry(component(&a.file))
                .or_default()
                .insert(b.name.clone());
            continue;
        }
        let (ca, cb) = (component(&a.file), component(&b.file));
        if ca != cb {
            *deps
                .entry((ca, cb))
                .or_default()
                .entry(e.kind.as_str())
                .or_default() += e.count;
        }
    }
    #[allow(clippy::type_complexity)]
    let mut deps: Vec<((String, String), BTreeMap<&str, u32>)> = deps
        .into_iter()
        .filter(|(_, k)| k.values().sum::<u32>() >= min_weight)
        .collect();
    deps.sort_by_key(|(k, v)| (std::cmp::Reverse(v.values().sum::<u32>()), k.clone()));

    let components: Vec<Value> = sizes
        .iter()
        .map(|(c, (files, symbols))| {
            let mut v = json!({ "component": c, "files": files, "symbols": symbols });
            if let Some(e) = ext.get(c) {
                let mut e: Vec<&String> = e.iter().collect();
                e.sort();
                v["external_deps"] = json!(e);
            }
            v
        })
        .collect();
    let dependencies: Vec<Value> = deps
        .iter()
        .map(|((a, b), k)| json!({ "from": a, "to": b, "weight": k.values().sum::<u32>(), "by_kind": k }))
        .collect();
    json!({ "depth": depth, "components": components, "dependencies": dependencies })
}

pub fn overview(g: &Graph, top: usize) -> Value {
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    let mut langs: BTreeMap<String, usize> = BTreeMap::new();
    for n in &g.nodes {
        *kinds.entry(n.kind.as_str()).or_default() += 1;
        if n.kind == SymKind::File
            && let Some(l) = n.lang
        {
            *langs.entry(format!("{l:?}").to_lowercase()).or_default() += 1;
        }
    }
    let mut edge_kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for e in &g.edges {
        *edge_kinds.entry(e.kind.as_str()).or_default() += 1;
    }

    let fan_in = |i: usize, kinds: &[EdgeKind]| -> u32 {
        g.in_edges[i]
            .iter()
            .map(|&e| &g.edges[e])
            .filter(|e| kinds.contains(&e.kind))
            .map(|e| e.count)
            .sum()
    };
    let ranked = |filter: &dyn Fn(usize) -> bool, kinds: &[EdgeKind]| -> Vec<Value> {
        let mut v: Vec<(u32, usize)> = (0..g.nodes.len())
            .filter(|&i| filter(i))
            .map(|i| (fan_in(i, kinds), i))
            .filter(|(s, _)| *s > 0)
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        v.into_iter()
            .take(top)
            .map(|(s, i)| json!({ "id": g.nodes[i].id, "kind": g.nodes[i].kind.as_str(), "score": s }))
            .collect()
    };
    let sym_kinds = [EdgeKind::Calls, EdgeKind::Inherits, EdgeKind::References];
    let central = ranked(
        &|i| !matches!(g.nodes[i].kind, SymKind::File | SymKind::External),
        &sym_kinds,
    );
    let central_files = ranked(&|i| g.nodes[i].kind == SymKind::File, &[EdgeKind::Imports]);
    let externals = ranked(
        &|i| g.nodes[i].kind == SymKind::External,
        &[EdgeKind::Imports],
    );

    let members = community_members(g);
    let communities: Vec<Value> = members
        .iter()
        .enumerate()
        .take(top)
        .map(|(c, m)| {
            let mut v = community_ref(g, c as u32);
            v["size"] = json!(m.len());
            v["top_members"] = json!(
                top_members(g, m, 3)
                    .iter()
                    .map(|&i| &g.nodes[i].id)
                    .collect::<Vec<_>>()
            );
            v
        })
        .collect();

    json!({
        "root": g.root,
        // Filled by vcs::index_state (it needs the repository); keeps its place here.
        "index": Value::Null,
        "languages": langs,
        "node_kinds": kinds,
        "edge_kinds": edge_kinds,
        "edge_confidence": confidence_shares(g),
        "most_depended_on_symbols": central,
        "most_imported_files": central_files,
        "top_external_dependencies": externals,
        "community_count": g.communities.len(),
        "largest_communities": communities,
        "hint": "Use module_graph for component-level dependencies, get_community for structural clusters, search_symbols to find ids, then get_symbol / get_neighbors / find_paths / impact_analysis.",
    })
}

/// Share of each confidence level among dependency edges (`contains` is always exact).
fn confidence_shares(g: &Graph) -> Value {
    let mut counts = [0usize; 3];
    for e in g.edges.iter().filter(|e| e.kind != EdgeKind::Contains) {
        counts[e.confidence as usize] += 1;
    }
    let total: usize = counts.iter().sum();
    let mut out = serde_json::Map::new();
    for c in Confidence::ALL {
        let n = counts[c as usize];
        let pct = (n * 1000)
            .checked_div(total)
            .map_or(0.0, |x| x as f64 / 10.0);
        out.insert(c.as_str().into(), json!({ "edges": n, "percent": pct }));
    }
    Value::Object(out)
}

/// Edges at one confidence level, to check what the name-based resolver guessed: totals
/// per edge kind and level, then the matching edges grouped by source file, with lines.
pub fn audit_edges(
    g: &Graph,
    level: Confidence,
    path: Option<&str>,
    kinds: &[EdgeKind],
    limit: usize,
) -> Value {
    let in_scope = |e: &crate::graph::Edge| {
        e.kind != EdgeKind::Contains
            && edge_allowed(kinds, e.kind)
            && path.is_none_or(|p| in_path(&g.nodes[e.from].file, p))
    };
    let mut totals: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut matching: Vec<&crate::graph::Edge> = Vec::new();
    for e in g.edges.iter().filter(|e| in_scope(e)) {
        *totals
            .entry(e.kind.as_str())
            .or_default()
            .entry(e.confidence.as_str())
            .or_default() += 1;
        if e.confidence == level {
            matching.push(e);
        }
    }
    matching.sort_by(|a, b| {
        let (fa, fb) = (&g.nodes[a.from].file, &g.nodes[b.from].file);
        fa.cmp(fb).then(a.line.cmp(&b.line)).then(a.to.cmp(&b.to))
    });
    let mut by_file: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for e in matching.iter().take(limit) {
        let count = if e.count > 1 {
            format!(" (×{})", e.count)
        } else {
            String::new()
        };
        by_file
            .entry(g.nodes[e.from].file.as_str())
            .or_default()
            .push(json!(format!(
                "L{} {} {} → {}{count}",
                e.line,
                g.qualified_name(e.from),
                e.kind.as_str(),
                g.nodes[e.to].id
            )));
    }
    json!({
        "confidence": level.as_str(),
        "totals_by_kind": totals,
        "matching_edges": matching.len(),
        "truncated": matching.len() > limit,
        "by_file": by_file,
    })
}

/// `file` is `path` or inside directory `path` (component-wise, so `src` excludes `src2/`).
fn in_path(file: &str, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    path.is_empty() || file == path || file.strip_prefix(path).is_some_and(|r| r.starts_with('/'))
}

fn community_ref(g: &Graph, c: u32) -> Value {
    let label = g
        .communities
        .get(c as usize)
        .map_or("", |info| info.label.as_str());
    json!({ "id": c, "label": label })
}

/// Members of each community, indexed by community id.
fn community_members(g: &Graph) -> Vec<Vec<usize>> {
    let mut members = vec![Vec::new(); g.communities.len()];
    for (i, n) in g.nodes.iter().enumerate() {
        if let Some(list) = n.community.and_then(|c| members.get_mut(c as usize)) {
            list.push(i);
        }
    }
    members
}

/// The `n` most connected members, ties by id.
fn top_members(g: &Graph, members: &[usize], n: usize) -> Vec<usize> {
    let mut ranked: Vec<(usize, usize)> = members
        .iter()
        .map(|&i| (communities::dependency_degree(g, i), i))
        .collect();
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| g.nodes[a.1].id.cmp(&g.nodes[b.1].id))
    });
    ranked.into_iter().take(n).map(|(_, i)| i).collect()
}

/// A structural community: the files it spans, its most connected members and the
/// communities it depends on or is used by.
pub fn community(g: &Graph, c: u32, from: Option<usize>, limit: usize) -> Value {
    let members: Vec<usize> = (0..g.nodes.len())
        .filter(|&i| g.nodes[i].community == Some(c))
        .collect();

    let mut files: BTreeMap<&str, usize> = BTreeMap::new();
    for &i in &members {
        *files.entry(g.nodes[i].file.as_str()).or_default() += 1;
    }
    let mut files: Vec<(&str, usize)> = files.into_iter().collect();
    files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let file_count = files.len();

    // Edge occurrences to and from other communities. Name-guessed (`ambiguous`) edges
    // are counted apart: clustering uses them at half weight, but they may be wrong.
    #[derive(Default)]
    struct Count {
        edges: u32,
        ambiguous: u32,
    }
    impl Count {
        fn add(&mut self, e: &crate::graph::Edge) {
            if e.is_ambiguous() {
                self.ambiguous += e.count;
            } else {
                self.edges += e.count;
            }
        }
    }
    let mut depends_on: BTreeMap<u32, Count> = BTreeMap::new();
    let mut used_by: BTreeMap<u32, Count> = BTreeMap::new();
    let mut internal = Count::default();
    for e in &g.edges {
        if e.kind == EdgeKind::Contains {
            continue;
        }
        let (a, b) = (g.nodes[e.from].community, g.nodes[e.to].community);
        match (a == Some(c), b == Some(c)) {
            (true, true) => internal.add(e),
            (true, false) => {
                if let Some(b) = b {
                    depends_on.entry(b).or_default().add(e);
                }
            }
            (false, true) => {
                if let Some(a) = a {
                    used_by.entry(a).or_default().add(e);
                }
            }
            _ => {}
        }
    }
    let links = |map: BTreeMap<u32, Count>| -> Vec<Value> {
        let mut v: Vec<(u32, Count)> = map.into_iter().collect();
        v.sort_by(|a, b| {
            (b.1.edges, b.1.ambiguous)
                .cmp(&(a.1.edges, a.1.ambiguous))
                .then(a.0.cmp(&b.0))
        });
        v.into_iter()
            .take(10)
            .map(|(other, n)| {
                let mut r = community_ref(g, other);
                r["edges"] = json!(n.edges);
                if n.ambiguous > 0 {
                    r["ambiguous_edges"] = json!(n.ambiguous);
                }
                r
            })
            .collect()
    };

    let mut v = community_ref(g, c);
    if let Some(i) = from {
        v["node"] = json!(g.nodes[i].id);
    }
    v["size"] = json!(members.len());
    v["internal_edges"] = json!(internal.edges);
    if internal.ambiguous > 0 {
        v["internal_ambiguous_edges"] = json!(internal.ambiguous);
    }
    v["file_count"] = json!(file_count);
    v["files"] = json!(
        files
            .iter()
            .take(20)
            .map(|(f, n)| format!("{f} ({n})"))
            .collect::<Vec<_>>()
    );
    v["members"] = json!(
        top_members(g, &members, limit)
            .into_iter()
            .map(|i| json!({ "id": g.nodes[i].id, "kind": g.nodes[i].kind.as_str() }))
            .collect::<Vec<_>>()
    );
    v["truncated"] = json!(members.len() > limit || file_count > 20);
    v["depends_on"] = json!(links(depends_on));
    v["used_by"] = json!(links(used_by));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Node};

    /// A graph from (name, kind, parent) nodes and (from, kind, to, confidence) edges;
    /// every node lives in `m.rs` and its id is its name.
    fn graph(
        nodes: &[(&str, SymKind, Option<usize>)],
        edges: &[(usize, EdgeKind, usize, Confidence)],
    ) -> Graph {
        let mut g = Graph::new(Path::new("/repo"));
        for &(name, kind, parent) in nodes {
            g.nodes.push(Node {
                id: name.into(),
                kind,
                name: name.rsplit("::").next().unwrap().into(),
                file: "m.rs".into(),
                lang: None,
                line: 1,
                end_line: 1,
                signature: String::new(),
                doc: None,
                parent,
                community: None,
            });
        }
        for &(from, kind, to, confidence) in edges {
            g.edges.push(Edge {
                from,
                to,
                kind,
                count: 1,
                confidence,
                line: 1,
            });
        }
        g.build_indexes();
        g
    }

    fn labels(v: &Value) -> String {
        v["by_file"]["m.rs"].to_string()
    }

    #[test]
    fn impact_reports_the_surest_path_whatever_the_edge_order() {
        use Confidence::*;
        use EdgeKind::Calls;
        // caller -> target is a guess, but caller -> helper -> target is sure. The guessed
        // edge comes first in target's incoming edges.
        let g = graph(
            &[
                ("target", SymKind::Function, None),
                ("caller", SymKind::Function, None),
                ("helper", SymKind::Function, None),
            ],
            &[
                (1, Calls, 0, Ambiguous),
                (2, Calls, 0, Exact),
                (1, Calls, 2, Exact),
            ],
        );
        let v = impact(&g, 0, 3, 100, Ambiguous);
        let l = labels(&v);
        assert!(
            l.contains("caller (function) via called_by [depth 2]"),
            "{v:#}"
        );
        assert!(
            l.contains("helper (function) via called_by [depth 1]"),
            "{v:#}"
        );
        // Without the sure path, the guess is reported as such.
        let g = graph(
            &[
                ("target", SymKind::Function, None),
                ("caller", SymKind::Function, None),
            ],
            &[(1, Calls, 0, Ambiguous)],
        );
        let l = labels(&impact(&g, 0, 3, 100, Ambiguous));
        assert!(
            l.contains("caller (function) via called_by [depth 1, ambiguous]"),
            "{l}"
        );
    }

    #[test]
    fn min_confidence_also_limits_override_seeds() {
        use Confidence::*;
        use EdgeKind::*;
        // Impl::run overrides Base::run through an inferred `inherits`; user calls Base::run.
        let g = graph(
            &[
                ("Base", SymKind::Interface, None),
                ("Base::run", SymKind::Method, Some(0)),
                ("Impl", SymKind::Class, None),
                ("Impl::run", SymKind::Method, Some(2)),
                ("user", SymKind::Function, None),
            ],
            &[
                (0, Contains, 1, Exact),
                (2, Contains, 3, Exact),
                (2, Inherits, 0, Inferred),
                (4, Calls, 1, Exact),
            ],
        );
        let v = impact(&g, 3, 3, 100, Ambiguous);
        assert_eq!(
            v["overridden_members"],
            json!(["Base::run (inferred)"]),
            "{v:#}"
        );
        assert!(
            labels(&v).contains("user (function) via called_by [depth 1, inferred]"),
            "{v:#}"
        );

        let v = impact(&g, 3, 3, 100, Exact);
        assert!(v.get("overridden_members").is_none(), "{v:#}");
        assert_eq!(v["affected_symbols"], 0, "{v:#}");
    }

    #[test]
    fn find_paths_labels_guessed_edges() {
        use Confidence::*;
        let g = graph(
            &[
                ("a", SymKind::Function, None),
                ("b", SymKind::Function, None),
            ],
            &[(0, EdgeKind::Calls, 1, Inferred)],
        );
        let filter = EdgeFilter {
            kinds: &[],
            min: Ambiguous,
        };
        let v = find_paths(&g, 0, 1, filter, true, 3, 1);
        assert_eq!(v["paths"], json!(["a --calls (inferred)--> b"]));
    }

    #[test]
    fn in_path_matches_whole_components() {
        assert!(in_path("src/a.rs", "src"));
        assert!(in_path("src/a.rs", "src/"));
        assert!(in_path("src/a.rs", "src/a.rs"));
        assert!(in_path("src/a.rs", ""));
        assert!(!in_path("src2/a.rs", "src"));
        assert!(!in_path("src/abc.rs", "src/a"));
    }
}
