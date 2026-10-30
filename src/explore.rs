//! `explore`: answers a natural-language question with the relevant subgraph in one call.
//!
//! 1. Seeds come from explicit ids, or from the question's terms scored against node
//!    names, ids and docs, weighted by term rarity (IDF).
//! 2. A BFS (broad context) or DFS (follow one chain) expands from the seeds.
//! 3. Nodes are rendered seeds first, then by distance and relevance, each with its
//!    edges to other rendered nodes, and the text is cut to a token budget.

use std::collections::{HashMap, VecDeque};

use anyhow::{Result, bail};

use crate::graph::{Confidence, EdgeKind, Graph, SymKind};
use crate::output;
use crate::query::{self, Direction};

const MAX_SEEDS: usize = 8;
const MAX_VISITED: usize = 3000;
const MAX_TERMS: usize = 32;
/// Edge lines listed under one node; the rest are summarized.
const MAX_EDGES_PER_NODE: usize = 30;
/// Name substrings only count for terms at least this long (`get` ⊄ `budget`).
const MIN_SUBSTRING_TERM: usize = 4;

/// Words that carry no symbol information (English and Italian).
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "is", "are", "be", "how", "what", "which", "who", "where", "why", "when",
    "does", "do", "of", "to", "in", "on", "for", "with", "and", "or", "from", "by", "it", "this",
    "that", "there", "between", "il", "lo", "la", "i", "gli", "le", "un", "uno", "una", "di", "da",
    "del", "della", "dei", "delle", "e", "ed", "o", "che", "chi", "come", "cosa", "dove", "perche",
    "perché", "quale", "quali", "con", "per", "su", "tra", "fra", "nel", "nella",
];

/// Words describing the relation being asked about rather than a symbol to start from.
const RELATION_WORDS: &[&str] = &[
    "call",
    "calls",
    "called",
    "caller",
    "callers",
    "use",
    "uses",
    "used",
    "import",
    "imports",
    "inherit",
    "inherits",
    "extends",
    "implement",
    "implements",
    "reference",
    "references",
    "depend",
    "depends",
    "dependency",
    "dependencies",
    "relation",
    "related",
    "connected",
    "chiama",
    "chiamato",
    "usa",
    "usato",
    "importa",
    "eredita",
    "estende",
    "implementa",
    "dipende",
    "dipendenze",
    "relazione",
    "collegato",
    "collegati",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Bfs,
    Dfs,
}

pub struct Options<'a> {
    pub mode: Mode,
    pub depth: u32,
    pub direction: Direction,
    pub kinds: &'a [EdgeKind],
    /// Only edges at least this sure are followed and drawn.
    pub min_confidence: Confidence,
    pub token_budget: usize,
}

impl Options<'_> {
    fn filter(&self) -> query::EdgeFilter<'_> {
        query::EdgeFilter {
            kinds: self.kinds,
            min: self.min_confidence,
        }
    }
}

pub fn explore(g: &Graph, question: &str, seeds: &[String], opts: &Options) -> Result<String> {
    let terms = query_terms(question);
    let scores = score_nodes(g, &symbol_terms(&terms));

    let seeds: Vec<&str> = seeds
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let (seeds, mut notes) = if seeds.is_empty() {
        pick_seeds(g, question, &terms, &scores)?
    } else {
        explicit_seeds(g, &seeds)?
    };

    let visited = traverse(g, &seeds, opts);
    if visited.capped {
        notes.push(format!(
            "traversal stopped at {MAX_VISITED} nodes; reduce depth or restrict edge_kinds/direction"
        ));
    }
    Ok(render(g, question, &seeds, &visited, &scores, &notes, opts))
}

/// Terms that can name a symbol: relation words describe the question, not a target.
fn symbol_terms(terms: &[String]) -> Vec<String> {
    terms
        .iter()
        .filter(|t| !RELATION_WORDS.contains(&t.as_str()))
        .cloned()
        .collect()
}

/// Lowercase terms of the question, without stop words.
fn query_terms(question: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in question.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let w = word.to_lowercase();
        if w.chars().count() < 2 || STOP_WORDS.contains(&w.as_str()) || out.contains(&w) {
            continue;
        }
        out.push(w);
        if out.len() == MAX_TERMS {
            break;
        }
    }
    out
}

/// `UserService` / `user_service` -> `user`, `service`.
fn name_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let boundary = c.is_uppercase()
            && !cur.is_empty()
            && (chars[i - 1].is_lowercase() || chars.get(i + 1).is_some_and(|n| n.is_lowercase()));
        if boundary {
            tokens.push(std::mem::take(&mut cur));
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Weight from which a match is on the symbol name itself.
const NAME_MATCH: f64 = 2.0;

/// How strongly a term matches a node: exact name, name token, name substring,
/// id/path or doc.
fn term_weight(term: &str, name: &str, tokens: &[String], id: &str, doc: &str) -> f64 {
    if name == term {
        4.0
    } else if tokens.iter().any(|t| t == term) {
        3.0
    } else if term.chars().count() >= MIN_SUBSTRING_TERM && name.contains(term) {
        NAME_MATCH
    } else if id.contains(term) {
        1.0
    } else if doc.contains(term) {
        0.5
    } else {
        0.0
    }
}

/// Relevance of every node to the question; per-term weights are kept for seed picking.
struct Scores {
    total: Vec<f64>,
    /// Some term matches the node's name (not just its path or doc).
    name_match: Vec<bool>,
    /// Best name match for each term, if any.
    best_by_term: Vec<Option<usize>>,
}

fn score_nodes(g: &Graph, terms: &[String]) -> Scores {
    let n = g.nodes.len();
    let mut total = vec![0.0; n];
    let mut name_match = vec![false; n];
    let mut per_term: Vec<Vec<f64>> = vec![vec![0.0; n]; terms.len()];
    for (i, node) in g.nodes.iter().enumerate() {
        // A file is named by its stem: `api.ts` must not match the term `ts`.
        let display = if node.kind == SymKind::File {
            node.name
                .rsplit_once('.')
                .map_or(node.name.as_str(), |(stem, _)| stem)
        } else {
            node.name.as_str()
        };
        let name = display.to_lowercase();
        let tokens = name_tokens(display);
        let id = node.id.to_lowercase();
        let doc = node.doc.as_deref().unwrap_or("").to_lowercase();
        for (t, term) in terms.iter().enumerate() {
            per_term[t][i] = term_weight(term, &name, &tokens, &id, &doc);
        }
    }
    let mut best_by_term = Vec::new();
    for weights in &per_term {
        let df = weights.iter().filter(|&&w| w > 0.0).count();
        if df == 0 {
            best_by_term.push(None);
            continue;
        }
        let idf = ((n as f64 + 1.0) / (df as f64 + 1.0)).ln() + 1.0;
        let mut best: Option<(f64, usize)> = None;
        for i in 0..n {
            let w = weights[i];
            if w == 0.0 {
                continue;
            }
            total[i] += w * idf;
            if w < NAME_MATCH {
                continue;
            }
            name_match[i] = true;
            let key = w * kind_factor(g.nodes[i].kind) + g.degree(i).min(100) as f64 * 1e-4;
            if best.is_none_or(|(b, _)| key > b) {
                best = Some((key, i));
            }
        }
        best_by_term.push(best.map(|(_, i)| i));
    }
    for (i, s) in total.iter_mut().enumerate() {
        *s *= kind_factor(g.nodes[i].kind);
    }
    Scores {
        total,
        name_match,
        best_by_term,
    }
}

/// Symbols are better starting points than files and external packages.
fn kind_factor(kind: SymKind) -> f64 {
    match kind {
        SymKind::File => 0.6,
        SymKind::External => 0.4,
        _ => 1.0,
    }
}

fn pick_seeds(
    g: &Graph,
    question: &str,
    terms: &[String],
    scores: &Scores,
) -> Result<(Vec<usize>, Vec<String>)> {
    // A one-word question naming a node exactly is a lookup, not a search.
    let q = question
        .trim()
        .trim_matches(|c: char| !(c.is_alphanumeric() || "_:./".contains(c)));
    if !q.is_empty() && !q.contains(char::is_whitespace) {
        if let Some(&i) = g.by_id.get(q).or(g.by_file.get(q)) {
            return Ok((vec![i], Vec::new()));
        }
        let exact = g.by_name.get(&q.to_lowercase()).map_or(0, Vec::len);
        if (exact == 1 || (exact == 0 && q.contains("::")))
            && let Ok(i) = query::resolve(g, q)
        {
            return Ok((vec![i], Vec::new()));
        }
        if exact > 1 {
            query::resolve(g, q).map_err(|e| {
                anyhow::anyhow!(
                    "{e}\nPass the right one in `seeds`, or ask a more specific question."
                )
            })?;
        }
    }

    if symbol_terms(terms).is_empty() {
        bail!(
            "the question has no term naming code ({terms:?}); mention a symbol, file or concept, \
             or pass explicit seeds"
        );
    }
    let mut ranked: Vec<usize> = (0..g.nodes.len())
        .filter(|&i| scores.total[i] > 0.0)
        .collect();
    if ranked.is_empty() {
        bail!(
            "no node matches the question terms {terms:?}; try search_symbols or pass explicit seeds"
        );
    }
    ranked.sort_by(|&a, &b| {
        scores.total[b]
            .total_cmp(&scores.total[a])
            .then(g.degree(b).cmp(&g.degree(a)))
            .then(a.cmp(&b))
    });

    // Seeds must match on their name; path or doc text alone is only a fallback.
    let mut notes = Vec::new();
    let anchored: Vec<usize> = ranked
        .iter()
        .copied()
        .filter(|&i| scores.name_match[i])
        .collect();
    let pool = if anchored.is_empty() {
        notes.push("no symbol name matches the question; seeds come from paths/docs".to_string());
        &ranked
    } else {
        &anchored
    };
    let top = scores.total[pool[0]];
    let mut seeds: Vec<usize> = pool
        .iter()
        .copied()
        .take_while(|&i| scores.total[i] >= top * 0.6)
        .take(MAX_SEEDS / 2)
        .collect();
    // Make sure every term naming something is represented by its best match.
    for &best in scores.best_by_term.iter().flatten() {
        if !seeds.contains(&best) && seeds.len() < MAX_SEEDS {
            seeds.push(best);
        }
    }
    Ok((seeds, notes))
}

fn explicit_seeds(g: &Graph, values: &[&str]) -> Result<(Vec<usize>, Vec<String>)> {
    let mut seeds = Vec::new();
    let mut notes = Vec::new();
    for &v in values {
        match query::resolve(g, v) {
            Ok(i) if !seeds.contains(&i) => seeds.push(i),
            Ok(_) => {}
            Err(e) => notes.push(format!(
                "seed `{v}` skipped: {}",
                first_line(&e.to_string())
            )),
        }
    }
    if seeds.is_empty() {
        bail!("no seed could be resolved:\n{}", notes.join("\n"));
    }
    Ok((seeds, notes))
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

/// node -> distance from the closest seed, in discovery order.
struct Visited {
    order: Vec<usize>,
    dist: HashMap<usize, u32>,
    /// Stopped at `MAX_VISITED`.
    capped: bool,
}

fn traverse(g: &Graph, seeds: &[usize], opts: &Options) -> Visited {
    let mut v = Visited {
        order: Vec::new(),
        dist: HashMap::new(),
        capped: false,
    };
    for &s in seeds {
        if v.dist.insert(s, 0).is_none() {
            v.order.push(s);
        }
    }
    // Files and externals are shown but not expanded (unless they are seeds):
    // a file would pull in every symbol it contains.
    let expandable =
        |i: usize, d: u32| d == 0 || !matches!(g.nodes[i].kind, SymKind::File | SymKind::External);
    let steps = |i: usize| -> Vec<usize> {
        query::steps(g, i, opts.direction, opts.filter())
            .map(|(n, _, _)| n)
            .collect()
    };

    match opts.mode {
        Mode::Bfs => {
            let mut queue: VecDeque<usize> = seeds.iter().copied().collect();
            while let Some(cur) = queue.pop_front() {
                let d = v.dist[&cur];
                if d >= opts.depth || !expandable(cur, d) {
                    continue;
                }
                for next in steps(cur) {
                    if v.order.len() >= MAX_VISITED && !v.dist.contains_key(&next) {
                        v.capped = true;
                        return v;
                    }
                    if let std::collections::hash_map::Entry::Vacant(e) = v.dist.entry(next) {
                        e.insert(d + 1);
                        v.order.push(next);
                        queue.push_back(next);
                    }
                }
            }
        }
        Mode::Dfs => {
            // Depth-first order, but a node reached later through a shorter path gets
            // its distance lowered and is expanded again, so every node within
            // `depth` is found with its true distance.
            for &s in seeds {
                let mut stack = vec![(s, 0u32)];
                while let Some((cur, d)) = stack.pop() {
                    if d > v.dist[&cur] {
                        continue; // stale entry, a shorter path was found meanwhile
                    }
                    if d >= opts.depth || !expandable(cur, d) {
                        continue;
                    }
                    let mut nexts = steps(cur);
                    nexts.reverse(); // keep edge order when popping
                    for next in nexts {
                        let known = v.dist.get(&next).copied();
                        if known.is_none() && v.order.len() >= MAX_VISITED {
                            v.capped = true;
                            return v;
                        }
                        if known.is_none_or(|k| k > d + 1) {
                            if known.is_none() {
                                v.order.push(next);
                            }
                            v.dist.insert(next, d + 1);
                            stack.push((next, d + 1));
                        }
                    }
                }
            }
        }
    }
    v
}

fn render(
    g: &Graph,
    question: &str,
    seeds: &[usize],
    visited: &Visited,
    scores: &Scores,
    notes: &[String],
    opts: &Options,
) -> String {
    // Seeds first, then by distance, relevance to the question and connectivity.
    let mut order = visited.order.clone();
    let seed_rank: HashMap<usize, usize> = seeds.iter().enumerate().map(|(r, &s)| (s, r)).collect();
    order.sort_by(|&a, &b| {
        let ka = seed_rank.get(&a).copied().unwrap_or(usize::MAX);
        let kb = seed_rank.get(&b).copied().unwrap_or(usize::MAX);
        ka.cmp(&kb)
            .then(visited.dist[&a].cmp(&visited.dist[&b]))
            .then(scores.total[b].total_cmp(&scores.total[a]))
            .then(g.degree(b).cmp(&g.degree(a)))
            .then(a.cmp(&b))
    });
    let rank: HashMap<usize, usize> = order.iter().enumerate().map(|(r, &n)| (n, r)).collect();

    // Each edge between rendered nodes is listed once, under its higher-ranked endpoint.
    let mut blocks: Vec<Vec<String>> = Vec::with_capacity(order.len());
    for &n in &order {
        let mut block = vec![node_line(
            g,
            n,
            visited.dist[&n],
            seed_rank.contains_key(&n),
        )];
        let mut edges: Vec<(usize, String)> = Vec::new();
        for (other, e, forward) in query::steps(g, n, Direction::Both, opts.filter()) {
            let Some(&r) = rank.get(&other) else { continue };
            if r < rank[&n] || (r == rank[&n] && !forward) {
                continue;
            }
            let edge = &g.edges[e];
            let mut line = if forward {
                format!("    {} → {}", edge.kind.as_str(), g.nodes[other].id)
            } else {
                format!("    {} ← {}", edge.kind.inverse_str(), g.nodes[other].id)
            };
            if edge.count > 1 {
                line.push_str(&format!(" (×{})", edge.count));
            }
            if let Some(l) = edge.confidence.label() {
                line.push_str(&format!(" ({l})"));
            }
            // Sites are lines of the edge's source: say which file when that is not `n`'s.
            if let Some(sites) = edge.sites_text() {
                if forward {
                    line.push_str(&format!(" {sites}"));
                } else {
                    line.push_str(&format!(" {sites} in {}", g.nodes[edge.from].file));
                }
            }
            edges.push((r, line));
        }
        edges.sort();
        let total = edges.len();
        block.extend(edges.into_iter().take(MAX_EDGES_PER_NODE).map(|(_, l)| l));
        if total > MAX_EDGES_PER_NODE {
            block.push(format!(
                "    … and {} more edges (use get_neighbors on this node)",
                total - MAX_EDGES_PER_NODE
            ));
        }
        blocks.push(block);
    }

    let mode = match opts.mode {
        Mode::Bfs => "BFS",
        Mode::Dfs => "DFS",
    };
    let seed_ids: Vec<&str> = seeds.iter().map(|&s| g.nodes[s].id.as_str()).collect();
    let shown_question = output::clip(question, 120);
    let mut header = format!(
        "Question: {shown_question}\nTraversal: {mode} depth={} | {} nodes | seeds: {}",
        opts.depth,
        order.len(),
        seed_ids.join(", ")
    );
    if !opts.kinds.is_empty() {
        let kinds: Vec<&str> = opts.kinds.iter().map(|k| k.as_str()).collect();
        header.push_str(&format!(" | edges: {}", kinds.join(",")));
    }
    if opts.min_confidence != Confidence::Ambiguous {
        header.push_str(&format!(
            " | min confidence: {}",
            opts.min_confidence.as_str()
        ));
    }
    for note in notes.iter().take(5) {
        header.push_str(&format!("\nNote: {}", output::clip(note, 160)));
    }
    if notes.len() > 5 {
        header.push_str(&format!("\nNote: … and {} more notes", notes.len() - 5));
    }
    output::fit_blocks(
        &header,
        &blocks,
        opts.token_budget,
        "nodes",
        "narrow with edge_kinds, depth or seeds",
    )
}

fn node_line(g: &Graph, n: usize, dist: u32, seed: bool) -> String {
    let node = &g.nodes[n];
    let marker = if seed {
        "★".to_string()
    } else {
        format!("[{dist}]")
    };
    let mut line = format!("{marker} {} ({})", node.id, node.kind.as_str());
    if node.kind != SymKind::File && !node.file.is_empty() {
        line.push_str(&format!(" L{}-{}", node.line, node.end_line));
    }
    if !node.signature.is_empty() {
        line.push_str(&format!(" `{}`", node.signature));
    }
    if seed && let Some(doc) = &node.doc {
        line.push_str(&format!("\n    doc: {doc}"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_names() {
        assert_eq!(name_tokens("UserService"), vec!["user", "service"]);
        assert_eq!(name_tokens("HTTPClient"), vec!["http", "client"]);
        assert_eq!(name_tokens("parse_args"), vec!["parse", "args"]);
    }

    #[test]
    fn drops_stop_words() {
        assert_eq!(
            query_terms("Come viene usato lo UserService con il Repository?"),
            vec!["viene", "usato", "userservice", "repository"]
        );
    }

    #[test]
    fn exact_name_beats_token_and_short_substrings_do_not_match() {
        let toks = name_tokens("resolve_imports");
        assert!(
            term_weight("resolve", "resolve", &["resolve".into()], "", "")
                > term_weight("resolve", "resolve_imports", &toks, "", "")
        );
        assert_eq!(
            term_weight("get", "budget", &["budget".into()], "", ""),
            0.0
        );
    }
}
