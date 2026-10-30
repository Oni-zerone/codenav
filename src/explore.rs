//! `explore`: answers a natural-language question with the relevant subgraph in one call.
//!
//! 1. Seeds come from explicit ids, or from the question's terms scored against node
//!    names, ids and docs, weighted by term rarity (IDF).
//! 2. A BFS (broad context) or DFS (follow one chain) expands from the seeds.
//! 3. Nodes are rendered seeds first, then by distance and relevance, each with its
//!    edges to other rendered nodes, and the text is cut to a token budget.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Result, bail};

use crate::graph::{EdgeKind, Graph, SymKind};
use crate::query::{self, Direction};

const CHARS_PER_TOKEN: usize = 3;
const MAX_SEEDS: usize = 8;
const MAX_VISITED: usize = 3000;

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
    pub token_budget: usize,
}

pub fn explore(g: &Graph, question: &str, seeds: &[String], opts: &Options) -> Result<String> {
    let terms = query_terms(question);
    let scores = score_nodes(g, &terms);

    let (seeds, notes) = if seeds.is_empty() {
        (pick_seeds(g, question, &terms, &scores)?, Vec::new())
    } else {
        explicit_seeds(g, seeds)?
    };

    let visited = traverse(g, &seeds, opts);
    Ok(render(g, question, &seeds, &visited, &scores, &notes, opts))
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

/// How strongly a term matches a node: by name token, name substring, id/path or doc.
fn term_weight(term: &str, name: &str, tokens: &[String], id: &str, doc: &str) -> f64 {
    if name == term || tokens.iter().any(|t| t == term) {
        3.0
    } else if name.contains(term) {
        2.0
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
    /// (term, best node for that term, unless the term only matches very weakly)
    best_by_term: Vec<(String, Option<usize>)>,
}

fn score_nodes(g: &Graph, terms: &[String]) -> Scores {
    let n = g.nodes.len();
    let mut total = vec![0.0; n];
    let mut per_term: Vec<Vec<f64>> = vec![vec![0.0; n]; terms.len()];
    for (i, node) in g.nodes.iter().enumerate() {
        let name = node.name.to_lowercase();
        let tokens = name_tokens(&node.name);
        let id = node.id.to_lowercase();
        let doc = node.doc.as_deref().unwrap_or("").to_lowercase();
        for (t, term) in terms.iter().enumerate() {
            per_term[t][i] = term_weight(term, &name, &tokens, &id, &doc);
        }
    }
    let mut best_by_term = Vec::new();
    for (t, term) in terms.iter().enumerate() {
        let df = per_term[t].iter().filter(|&&w| w > 0.0).count();
        if df == 0 {
            best_by_term.push((term.clone(), None));
            continue;
        }
        let idf = ((n as f64 + 1.0) / (df as f64 + 1.0)).ln() + 1.0;
        let mut best: Option<(f64, usize)> = None;
        for i in 0..n {
            let w = per_term[t][i];
            if w == 0.0 {
                continue;
            }
            total[i] += w * idf;
            let key = w * kind_factor(g.nodes[i].kind) + g.degree(i).min(100) as f64 * 1e-4;
            if best.is_none_or(|(b, _)| key > b) {
                best = Some((key, i));
            }
        }
        // A doc-only hit is too weak to anchor a traversal on its own.
        let best = best.filter(|&(k, _)| k >= 1.0).map(|(_, i)| i);
        best_by_term.push((term.clone(), best));
    }
    for (i, s) in total.iter_mut().enumerate() {
        *s *= kind_factor(g.nodes[i].kind);
    }
    Scores {
        total,
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

fn pick_seeds(g: &Graph, question: &str, terms: &[String], scores: &Scores) -> Result<Vec<usize>> {
    // A question that is exactly the name of several nodes is a lookup, not a search.
    let q = question.trim();
    if !q.is_empty() && !q.contains(char::is_whitespace) {
        if let Ok(i) = query::resolve(g, q) {
            return Ok(vec![i]);
        }
        let exact = g.by_name.get(&q.to_lowercase()).map_or(0, Vec::len);
        if exact > 1 {
            query::resolve(g, q)?; // returns the ambiguity error with candidate ids
        }
    }

    let mut ranked: Vec<usize> = (0..g.nodes.len())
        .filter(|&i| scores.total[i] > 0.0)
        .collect();
    ranked.sort_by(|&a, &b| {
        scores.total[b]
            .total_cmp(&scores.total[a])
            .then(g.degree(b).cmp(&g.degree(a)))
            .then(a.cmp(&b))
    });
    let Some(&top) = ranked.first() else {
        bail!(
            "no node matches the question terms {terms:?}; try search_symbols or pass explicit seeds"
        );
    };

    // Everything close to the best score, then make sure every symbol-like term
    // is represented by its best match.
    let mut seeds: Vec<usize> = ranked
        .iter()
        .copied()
        .take_while(|&i| scores.total[i] >= scores.total[top] * 0.6)
        .take(MAX_SEEDS / 2)
        .collect();
    let only_relation_words = terms.iter().all(|t| RELATION_WORDS.contains(&t.as_str()));
    for (term, best) in &scores.best_by_term {
        if !only_relation_words && RELATION_WORDS.contains(&term.as_str()) {
            continue;
        }
        if let Some(b) = *best
            && !seeds.contains(&b)
            && seeds.len() < MAX_SEEDS
        {
            seeds.push(b);
        }
    }
    Ok(seeds)
}

fn explicit_seeds(g: &Graph, values: &[String]) -> Result<(Vec<usize>, Vec<String>)> {
    let mut seeds = Vec::new();
    let mut notes = Vec::new();
    for v in values {
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
}

fn traverse(g: &Graph, seeds: &[usize], opts: &Options) -> Visited {
    let mut v = Visited {
        order: Vec::new(),
        dist: HashMap::new(),
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
        query::steps(g, i, opts.direction, opts.kinds)
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
                    if v.order.len() >= MAX_VISITED {
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
            for &s in seeds {
                let mut stack = vec![(s, 0u32)];
                let mut on_path: HashSet<usize> = HashSet::new();
                while let Some((cur, d)) = stack.pop() {
                    on_path.insert(cur);
                    if d >= opts.depth || !expandable(cur, d) {
                        continue;
                    }
                    let mut nexts = steps(cur);
                    nexts.reverse(); // keep edge order when popping
                    for next in nexts {
                        if v.order.len() >= MAX_VISITED {
                            return v;
                        }
                        if on_path.contains(&next) {
                            continue;
                        }
                        let known = v.dist.get(&next).copied();
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
    let mut blocks = Vec::with_capacity(order.len());
    for &n in &order {
        let mut block = node_line(g, n, visited.dist[&n], seed_rank.contains_key(&n));
        let mut edges: Vec<(usize, String)> = Vec::new();
        for (other, e, forward) in query::steps(g, n, Direction::Both, opts.kinds) {
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
            if edge.ambiguous {
                line.push_str(" (ambiguous)");
            }
            edges.push((r, line));
        }
        edges.sort();
        for (_, line) in edges {
            block.push('\n');
            block.push_str(&line);
        }
        blocks.push(block);
    }

    let mode = match opts.mode {
        Mode::Bfs => "BFS",
        Mode::Dfs => "DFS",
    };
    let seed_ids: Vec<&str> = seeds.iter().map(|&s| g.nodes[s].id.as_str()).collect();
    let mut header = format!(
        "Question: {question}\nTraversal: {mode} depth={} | {} nodes | seeds: {}",
        opts.depth,
        order.len(),
        seed_ids.join(", ")
    );
    if !opts.kinds.is_empty() {
        let kinds: Vec<&str> = opts.kinds.iter().map(|k| k.as_str()).collect();
        header.push_str(&format!(" | edges: {}", kinds.join(",")));
    }
    for note in notes {
        header.push_str(&format!("\nNote: {note}"));
    }
    cut_to_budget(&header, &blocks, opts.token_budget)
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

/// Keeps whole node blocks within the budget and announces any cut at the top.
fn cut_to_budget(header: &str, blocks: &[String], token_budget: usize) -> String {
    let budget = token_budget.max(100) * CHARS_PER_TOKEN;
    let full: usize = header.len() + blocks.iter().map(|b| b.len() + 2).sum::<usize>();
    let mut used = header.len();
    let mut kept = 0;
    for b in blocks {
        if used + b.len() + 2 > budget && kept > 0 {
            break;
        }
        used += b.len() + 2;
        kept += 1;
    }
    let mut out = String::with_capacity(used + 200);
    out.push_str(header);
    if kept < blocks.len() {
        out.push_str(&format!(
            "\n[!] TRUNCATED: showing {kept} of {} nodes (~{token_budget}-token budget). \
             Use token_budget={} to see all, or narrow with edge_kinds, depth or seeds.",
            blocks.len(),
            full.div_ceil(CHARS_PER_TOKEN) + 50
        ));
    }
    for b in &blocks[..kept] {
        out.push_str("\n\n");
        out.push_str(b);
    }
    out
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
    fn budget_cut_keeps_whole_blocks() {
        let blocks: Vec<String> = (0..50)
            .map(|i| format!("node {i} {}", "x".repeat(40)))
            .collect();
        let out = cut_to_budget("H", &blocks, 100);
        assert!(out.contains("[!] TRUNCATED"));
        assert!(out.len() < 100 * CHARS_PER_TOKEN + 300);
        assert!(
            out.lines()
                .filter(|l| l.starts_with("node"))
                .all(|l| l.ends_with('x'))
        );
    }
}
