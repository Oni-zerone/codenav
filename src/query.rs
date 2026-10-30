//! Read-only graph analyses used by the MCP tools and the CLI. Every function returns a
//! JSON value meant to be read by an LLM: compact, with stable ids it can pass back.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::graph::{EdgeKind, Graph, SymKind};

const MAX_CODE_LINES: usize = 150;

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
    if e.count > 1 || e.ambiguous {
        let mut o = json!({ "id": g.nodes[other].id });
        if e.count > 1 {
            o["count"] = json!(e.count);
        }
        if e.ambiguous {
            o["ambiguous"] = json!(true);
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
        out.push_str(&format!("{:>5} | {line}\n", n + 1));
    }
    Some(out)
}

/// One traversal step: (neighbor, edge index, edge followed forward?).
pub type Step = (usize, usize, bool);

fn edge_allowed(kinds: &[EdgeKind], k: EdgeKind) -> bool {
    kinds.is_empty() || kinds.contains(&k)
}

/// Steps from `i` along allowed edges in the given direction: (neighbor, edge, outgoing?).
pub fn steps<'a>(
    g: &'a Graph,
    i: usize,
    dir: Direction,
    kinds: &'a [EdgeKind],
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
        .filter(move |&(_, e, _)| edge_allowed(kinds, g.edges[e].kind))
}

pub fn neighbors(
    g: &Graph,
    start: usize,
    dir: Direction,
    kinds: &[EdgeKind],
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
        for (next, e, _) in steps(g, cur, dir, kinds) {
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
            if edge.ambiguous {
                ev["ambiguous"] = json!(true);
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
    kinds: &[EdgeKind],
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
        for (next, e, fwd) in steps(g, cur, dir, kinds) {
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
                let (arrow, other) = if fwd {
                    (format!(" --{}--> ", edge.kind.as_str()), edge.to)
                } else {
                    (format!(" <--{}-- ", edge.kind.as_str()), edge.from)
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
pub fn impact(g: &Graph, start: usize, depth: u32, limit: usize) -> Value {
    let mut seeds = vec![start];
    let mut i = 0;
    while i < seeds.len() {
        for &e in &g.out_edges[seeds[i]] {
            if g.edges[e].kind == EdgeKind::Contains {
                seeds.push(g.edges[e].to);
            }
        }
        i += 1;
    }
    let seed_set: HashSet<usize> = seeds.iter().copied().collect();
    let kinds = [
        EdgeKind::Calls,
        EdgeKind::Inherits,
        EdgeKind::References,
        EdgeKind::Imports,
    ];
    let mut seen = seed_set.clone();
    let mut queue: VecDeque<(usize, u32)> = seeds.iter().map(|&s| (s, 0)).collect();
    let mut hits: Vec<(usize, u32, EdgeKind, bool)> = Vec::new();
    while let Some((cur, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        for &e in &g.in_edges[cur] {
            let edge = &g.edges[e];
            if !kinds.contains(&edge.kind) || !seen.insert(edge.from) {
                continue;
            }
            hits.push((edge.from, d + 1, edge.kind, edge.ambiguous));
            queue.push_back((edge.from, d + 1));
        }
    }

    let mut by_file: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for &(n, d, k, ambiguous) in hits.iter().take(limit) {
        let node = &g.nodes[n];
        let guess = if ambiguous { ", ambiguous" } else { "" };
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
    json!({
        "target": summary(g, start),
        "members_considered": seed_set.len(),
        "affected_symbols": hits.iter().filter(|h| g.nodes[h.0].kind != SymKind::File).count(),
        "affected_files": by_file.len(),
        "truncated": hits.len() > limit,
        "by_file": by_file,
    })
}

/// Component-level view: edges aggregated between directories truncated at `depth`.
pub fn module_graph(g: &Graph, depth: usize, min_weight: u32) -> Value {
    let component = |file: &str| -> String {
        let parts: Vec<&str> = file.split('/').collect();
        if parts.len() <= 1 {
            return ".".to_string();
        }
        parts[..(parts.len() - 1).min(depth)].join("/")
    };
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

    json!({
        "root": g.root,
        "indexed_at_unix": g.indexed_at,
        "languages": langs,
        "node_kinds": kinds,
        "edge_kinds": edge_kinds,
        "most_depended_on_symbols": central,
        "most_imported_files": central_files,
        "top_external_dependencies": externals,
        "hint": "Use module_graph for component-level dependencies, search_symbols to find ids, then get_symbol / get_neighbors / find_paths / impact_analysis.",
    })
}
