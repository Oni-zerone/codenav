//! Which tests exercise a symbol or file: test files and test symbols that depend on it,
//! directly or through other code. Tests are recognised by path and name conventions
//! only, never by name similarity with the target.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::{Value, json};

use crate::graph::{Confidence, Graph, SymKind};
use crate::query;

/// Conventions recognised, as described to MCP clients.
pub const CONVENTIONS: &str = "*.test.*, *.spec.*, files under tests/, test/, __tests__/ or \
Tests/, tests.* / test.* (e.g. Rust `mod tests;` in tests.rs, Django tests.py), test_*.py, \
*_test.py, *_test.go, *Tests.swift, and symbols inside a Rust `mod tests`; fixtures, testdata, \
__mocks__ and conftest.py are support, not tests";

/// A test file by its path (see `CONVENTIONS`).
pub fn is_test_file(path: &str) -> bool {
    let mut parts = path.rsplit('/');
    let name = parts.next().unwrap_or("");
    let dirs: Vec<&str> = parts.collect();
    // Test data and helpers live next to tests but are not tests to run.
    if name == "conftest.py"
        || dirs.iter().any(|d| {
            matches!(
                *d,
                "fixtures" | "fixture" | "__fixtures__" | "testdata" | "__mocks__"
            )
        })
    {
        return false;
    }
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
        || matches!(stem, "tests" | "test")
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

pub fn tests_for(g: &Graph, target: usize, depth: u32, limit: usize, min: Confidence) -> Value {
    // BFS over dependents from what a change to the target touches (itself, its members,
    // the methods they override). Symbols are reached through calls and references, files
    // through imports: a test importing the target's file is not taken as using every
    // symbol in it.
    let (mut seeds, overridden) = query::change_seeds(g, target, min);
    seeds.extend(overridden);
    let mut dist: HashMap<usize, u32> = HashMap::new();
    // dependent -> (node one step closer to the target, edge)
    let mut next: HashMap<usize, (usize, usize)> = HashMap::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    for &(s, _) in &seeds {
        if dist.insert(s, 0).is_none() {
            queue.push_back(s);
        }
    }
    while let Some(cur) = queue.pop_front() {
        let d = dist[&cur];
        if d >= depth {
            continue;
        }
        for &e in &g.in_edges[cur] {
            let edge = &g.edges[e];
            if !query::DEPENDENCY_KINDS.contains(&edge.kind) || edge.confidence < min {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(v) = dist.entry(edge.from) {
                v.insert(d + 1);
                next.insert(edge.from, (cur, e));
                queue.push_back(edge.from);
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
    let target_is_test = is_test_node(g, target);
    for (&n, &d) in &dist {
        if n == target || !is_test_node(g, n) {
            continue;
        }
        // Seeds are the target itself: count them only as a code target's own tests
        // (Rust `mod tests` inside a target file), never a test target's own parts.
        if d == 0 && (target_is_test || g.nodes[n].kind == SymKind::File) {
            continue;
        }
        let file = g.nodes[n].file.as_str();
        let hit = by_file.entry(file).or_insert(Hit {
            distance: d,
            node: n,
            symbols: Vec::new(),
        });
        // The closest node starts the path; at equal distance a symbol over its file.
        let rank = |i: usize| (g.nodes[i].kind == SymKind::File, i);
        if d < hit.distance || (d == hit.distance && rank(n) < rank(hit.node)) {
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

/// `test --calls--> helper --imports--> target`, following the recorded steps (labelled
/// with the confidence when not exact).
fn path_text(g: &Graph, start: usize, next: &HashMap<usize, (usize, usize)>) -> String {
    let mut text = g.nodes[start].id.clone();
    let mut cur = start;
    while let Some(&(to, e)) = next.get(&cur) {
        let edge = &g.edges[e];
        let kind = match edge.confidence.label() {
            Some(l) => format!("{} ({l})", edge.kind.as_str()),
            None => edge.kind.as_str().to_string(),
        };
        text.push_str(&format!(" --{kind}--> {}", g.nodes[to].id));
        cur = to;
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
            "src/parser/tests.rs",
            "app/tests.py",
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
            "tests/fixtures/poly/py/app/models.py",
            "pkg/testdata/input.go",
            "src/__mocks__/api.ts",
            "tests/conftest.py",
        ] {
            assert!(!is_test_file(p), "{p}");
        }
    }
}
