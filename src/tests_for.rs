//! Which tests exercise a symbol or file: test files and test symbols that depend on it,
//! directly or through other code. Tests are recognised by path and name conventions
//! only, never by name similarity with the target.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::{Value, json};

use crate::graph::{Confidence, EdgeKind, Graph, SymKind};
use crate::query;

/// Dependency kinds followed from the target to its dependents.
const KINDS: [EdgeKind; 4] = [
    EdgeKind::Calls,
    EdgeKind::References,
    EdgeKind::Inherits,
    EdgeKind::Imports,
];

/// A test file by its path: `*.test.*`, `*.spec.*`, `__tests__/`, `tests/`, `test/`,
/// `test_*.py`, `*_test.py`, `*_test.go`, `*Tests.swift`, `Tests/`.
pub fn is_test_file(path: &str) -> bool {
    let mut parts = path.rsplit('/');
    let name = parts.next().unwrap_or("");
    let dirs: Vec<&str> = parts.collect();
    if dirs
        .iter()
        .any(|d| matches!(*d, "tests" | "test" | "__tests__" | "Tests"))
    {
        return true;
    }
    let stem = name.split('.').next().unwrap_or("");
    let ext = name.rsplit('.').next().unwrap_or("");
    name.contains(".test.")
        || name.contains(".spec.")
        || (ext == "py" && (stem.starts_with("test_") || stem.ends_with("_test")))
        || (ext == "go" && stem.ends_with("_test"))
        || (ext == "swift" && stem.ends_with("Tests"))
}

/// A test node: anything in a test file, or a symbol inside a module named `tests` /
/// `test` (Rust's `#[cfg(test)] mod tests`).
fn is_test_node(g: &Graph, i: usize) -> bool {
    let n = &g.nodes[i];
    if n.kind == SymKind::External {
        return false;
    }
    if is_test_file(&n.file) {
        return true;
    }
    let mut p = n.parent;
    while let Some(q) = p {
        let node = &g.nodes[q];
        if node.kind == SymKind::Module && matches!(node.name.as_str(), "tests" | "test") {
            return true;
        }
        p = node.parent;
    }
    false
}

/// One step towards the target: the next node and the edge used (None: from a symbol up
/// to its file, which costs nothing).
type Step = (usize, Option<usize>);

pub fn tests_for(g: &Graph, target: usize, depth: u32, limit: usize, min: Confidence) -> Value {
    // Seeds: the target and everything it contains.
    let mut dist: HashMap<usize, u32> = HashMap::new();
    let mut next: HashMap<usize, Step> = HashMap::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    let mut stack = vec![target];
    while let Some(s) = stack.pop() {
        if dist.insert(s, 0).is_none() {
            queue.push_back(s);
            for &e in &g.out_edges[s] {
                if g.edges[e].kind == EdgeKind::Contains {
                    stack.push(g.edges[e].to);
                }
            }
        }
    }

    // 0-1 BFS over dependents: a dependency edge costs 1; climbing from a symbol to the
    // file containing it costs 0 (code importing that file depends on the symbol).
    while let Some(cur) = queue.pop_front() {
        let d = dist[&cur];
        let file = match g.nodes[cur].kind {
            SymKind::File | SymKind::External => None,
            _ => g.file_of(cur),
        };
        if let Some(f) = file
            && dist.get(&f).is_none_or(|&old| old > d)
        {
            dist.insert(f, d);
            next.insert(f, (cur, None));
            queue.push_front(f);
        }
        if d >= depth {
            continue;
        }
        for &e in &g.in_edges[cur] {
            let edge = &g.edges[e];
            if !KINDS.contains(&edge.kind) || edge.confidence < min {
                continue;
            }
            let from = edge.from;
            if dist.get(&from).is_none_or(|&old| old > d + 1) {
                dist.insert(from, d + 1);
                next.insert(from, (cur, Some(e)));
                queue.push_back(from);
            }
        }
    }

    // Group the test nodes reached by file: closest distance, symbols, shortest path.
    struct Hit {
        distance: u32,
        node: usize,
        symbols: Vec<String>,
    }
    let mut by_file: BTreeMap<&str, Hit> = BTreeMap::new();
    let target_tests = is_test_node(g, target);
    for (&n, &d) in &dist {
        // The target's own tests (Rust `mod tests` in a target file) are seeds: direct.
        if n == target
            || !is_test_node(g, n)
            || (target_tests && d > 0 && g.nodes[n].file == g.nodes[target].file)
        {
            continue;
        }
        if d == 0 && (target_tests || g.nodes[n].kind == SymKind::File) {
            continue;
        }
        let file = g.nodes[n].file.as_str();
        let hit = by_file.entry(file).or_insert(Hit {
            distance: d,
            node: n,
            symbols: Vec::new(),
        });
        // The closest node starts the path; at equal distance a symbol over its file.
        let better = |a: usize, b: usize| {
            (g.nodes[a].kind == SymKind::File, a) < (g.nodes[b].kind == SymKind::File, b)
        };
        if d < hit.distance || (d == hit.distance && better(n, hit.node)) {
            hit.distance = d;
            hit.node = n;
        }
        if g.nodes[n].kind != SymKind::File {
            hit.symbols.push(g.qualified_name(n));
        }
    }
    let mut hits: Vec<(&str, Hit)> = by_file.into_iter().collect();
    hits.sort_by(|a, b| a.1.distance.cmp(&b.1.distance).then(a.0.cmp(b.0)));

    let total = hits.len();
    let mut direct = Vec::new();
    let mut indirect = Vec::new();
    for (file, mut hit) in hits.into_iter().take(limit) {
        hit.symbols.sort();
        let mut v = json!({ "file": file });
        if !hit.symbols.is_empty() {
            v["symbols"] = json!(hit.symbols);
        }
        v["path"] = json!(if hit.distance == 0 {
            format!("{} (inside the target)", g.nodes[hit.node].id)
        } else {
            path_text(g, hit.node, &next)
        });
        if hit.distance <= 1 {
            direct.push(v);
        } else {
            v["distance"] = json!(hit.distance);
            indirect.push(v);
        }
    }
    json!({
        "target": query::summary(g, target),
        "test_files": total,
        "truncated": total > limit,
        "direct": direct,
        "indirect": indirect,
    })
}

/// `test --calls--> helper --imports--> target`, following the recorded steps; a climb
/// from a symbol to its file is written ` ⊂ file`.
fn path_text(g: &Graph, start: usize, next: &HashMap<usize, Step>) -> String {
    let mut text = g.nodes[start].id.clone();
    let mut cur = start;
    let mut guard = 0;
    while let Some(&(to, edge)) = next.get(&cur) {
        match edge {
            Some(e) => {
                let edge = &g.edges[e];
                let kind = match edge.confidence.label() {
                    Some(l) => format!("{} ({l})", edge.kind.as_str()),
                    None => edge.kind.as_str().to_string(),
                };
                text.push_str(&format!(" --{kind}--> {}", g.nodes[to].id));
            }
            // Recorded on the file pointing at the symbol it was reached from.
            None => text.push_str(&format!(" ⊃ {}", g.nodes[to].id)),
        }
        cur = to;
        guard += 1;
        if guard > 64 {
            break;
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_files_by_convention() {
        for p in [
            "src/a.test.ts",
            "web/b.spec.tsx",
            "src/__tests__/c.ts",
            "tests/graph.rs",
            "pkg/test/util.js",
            "app/test_models.py",
            "app/models_test.py",
            "store/store_test.go",
            "Tests/AppTests/ViewModelTests.swift",
            "Sources/App/ViewModelTests.swift",
        ] {
            assert!(is_test_file(p), "{p}");
        }
        for p in [
            "src/testing.rs",
            "src/latest.ts",
            "src/contest/x.py",
            "app/attestation.py",
            "store/store.go",
            "src/a.ts",
        ] {
            assert!(!is_test_file(p), "{p}");
        }
    }
}
