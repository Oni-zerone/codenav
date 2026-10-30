//! End-to-end checks on the multi-language fixture in `tests/fixtures/poly`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn codenav() -> Command {
    Command::new(env!("CARGO_BIN_EXE_codenav"))
}

fn fixture() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/poly"))
}

fn tool(name: &str, args: Value) -> Value {
    let out = codenav()
        .args(["tool", name, &args.to_string(), "--path"])
        .arg(fixture())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn tool_text(name: &str, args: Value) -> (bool, String) {
    tool_text_in(fixture(), name, args)
}

fn tool_text_in(root: &Path, name: &str, args: Value) -> (bool, String) {
    let out = codenav()
        .args(["tool", name, &args.to_string(), "--path"])
        .arg(root)
        .output()
        .unwrap();
    let text = if out.status.success() {
        out.stdout
    } else {
        out.stderr
    };
    (
        out.status.success(),
        String::from_utf8_lossy(&text).into_owned(),
    )
}

/// All non-containment edges of a node as `kind target-id` strings.
fn outgoing(id: &str) -> Vec<String> {
    let v = tool("get_symbol", json!({ "id": id, "include_code": false }));
    let mut out = Vec::new();
    for (kind, list) in v["outgoing"].as_object().unwrap() {
        for item in list.as_array().unwrap() {
            let target = item.as_str().or(item["id"].as_str()).unwrap();
            out.push(format!("{kind} {target}"));
        }
    }
    out
}

fn assert_has(edges: &[String], expected: &str) {
    assert!(
        edges.iter().any(|e| e == expected),
        "missing `{expected}` in {edges:#?}"
    );
}

#[test]
fn python_imports_calls_and_inheritance() {
    let users = outgoing("py/app/services/users.py");
    assert_has(&users, "imports py/app/models.py");
    assert_has(&users, "imports ext:requests");
    let create = outgoing("py/app/services/users.py::UserService::create");
    assert_has(&create, "calls py/app/models.py::User");
    assert_has(
        &create,
        "calls py/app/services/users.py::UserService::notify",
    );
    assert_has(
        &outgoing("py/app/models.py::User"),
        "inherits py/app/models.py::Base",
    );
}

#[test]
fn typescript_and_jsx() {
    let profile = outgoing("web/src/components/Profile.tsx::Profile");
    assert_has(&profile, "calls web/src/lib/api.ts::HttpClient");
    assert_has(&profile, "calls web/src/components/Avatar.tsx::Avatar");
    assert_has(&profile, "references web/src/lib/api.ts::Client");
    assert_has(
        &outgoing("web/src/lib/api.ts::HttpClient"),
        "inherits web/src/lib/api.ts::Client",
    );
}

#[test]
fn go_packages_and_receivers() {
    let main = outgoing("goapp/main.go::main");
    assert_has(&main, "calls goapp/internal/store/store.go::New");
    assert_has(&main, "calls goapp/internal/store/store.go::Store::Get");
}

#[test]
fn swift_protocols_and_extensions() {
    assert_has(
        &outgoing("ios/Sources/App/Model.swift::ItemStore"),
        "inherits ios/Sources/App/Model.swift::Repository",
    );
    assert_has(
        &outgoing("ios/Sources/App/ViewModel.swift::ViewModel::reset"),
        "calls ios/Sources/App/ViewModel.swift::ViewModel::refresh",
    );
}

#[test]
fn paths_and_impact() {
    let p = tool(
        "find_paths",
        json!({ "from": "py/app/services/users.py::UserService::notify", "to": "py/app/models.py::Base" }),
    );
    assert_eq!(p["found"], true);
    let impact = tool(
        "impact_analysis",
        json!({ "id": "py/app/models.py::format_name" }),
    );
    assert!(
        impact["by_file"]["py/app/services/users.py"].is_array(),
        "{impact:#}"
    );
}

#[test]
fn explore_question_returns_relevant_subgraph() {
    let (ok, text) = tool_text(
        "explore",
        json!({ "question": "how does UserService create users" }),
    );
    assert!(ok, "{text}");
    let seeds = text.lines().find(|l| l.starts_with("Traversal:")).unwrap();
    assert!(
        seeds.contains("py/app/services/users.py::UserService::create"),
        "{text}"
    );
    for expected in [
        "calls → py/app/models.py::User",
        "calls → py/app/services/users.py::UserService::notify",
        "py/app/models.py::Base",
    ] {
        assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
    }
    assert!(!text.contains("TRUNCATED"), "{text}");
}

#[test]
fn explore_refuses_ambiguous_names_and_honours_seeds() {
    let (ok, text) = tool_text("explore", json!({ "question": "load" }));
    assert!(!ok);
    assert!(
        text.contains("ItemStore::load") && text.contains("Repository::load"),
        "{text}"
    );

    let (ok, text) = tool_text(
        "explore",
        json!({ "question": "store", "seeds": ["Store", "missing"], "depth": 1 }),
    );
    assert!(ok, "{text}");
    assert!(
        text.contains("★ goapp/internal/store/store.go::Store (struct)"),
        "{text}"
    );
    assert!(text.contains("seed `missing` skipped"), "{text}");
}

#[test]
fn explore_dfs_follows_callers_and_respects_budget() {
    let (ok, text) = tool_text(
        "explore",
        json!({ "question": "chi chiama refresh?", "mode": "dfs", "direction": "in", "edge_kinds": ["calls"] }),
    );
    assert!(ok, "{text}");
    assert!(
        text.contains("★ ios/Sources/App/ViewModel.swift::ViewModel::refresh"),
        "{text}"
    );
    assert!(
        text.contains("called_by ← ios/Sources/App/ViewModel.swift::ViewModel::reset"),
        "{text}"
    );

    let (ok, text) = tool_text(
        "explore",
        json!({ "question": "Profile component", "token_budget": 100 }),
    );
    assert!(ok, "{text}");
    assert!(text.contains("[!] TRUNCATED"), "{text}");
    assert!(
        text.len() < 100 * 3 + 400,
        "output too long: {}",
        text.len()
    );
}

/// A throwaway repository with the given files, removed on drop.
struct TempRepo(std::path::PathBuf);

impl TempRepo {
    fn new(name: &str, files: &[(&str, String)]) -> TempRepo {
        let dir = std::env::temp_dir().join(format!("codenav-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, content) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        TempRepo(dir)
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn explore_budget_holds_for_hub_nodes() {
    let mut src = String::from("def hub():\n    pass\n");
    for i in 0..1500 {
        src.push_str(&format!("def caller_{i}():\n    hub()\n"));
    }
    let repo = TempRepo::new("hub", &[("m.py", src)]);
    let (ok, text) = tool_text_in(
        &repo.0,
        "explore",
        json!({ "question": "hub", "token_budget": 100 }),
    );
    assert!(ok, "{text}");
    assert!(
        text.len() <= 100 * 3,
        "budget exceeded: {} bytes\n{text}",
        text.len()
    );
    assert!(text.contains("[!] TRUNCATED"), "{text}");

    // Absurd budgets are clamped instead of overflowing.
    let (ok, text) = tool_text_in(
        &repo.0,
        "explore",
        json!({ "question": "hub", "token_budget": u64::MAX, "depth": 1 }),
    );
    assert!(ok, "{text}");
    assert!(
        text.contains("more edges"),
        "edge list per node is capped: {text}"
    );
}

#[test]
fn explore_dfs_finds_shortest_distances() {
    let src = "def s():\n    a()\n    b()\n\ndef a():\n    c()\n\ndef c():\n    x()\n\n\
               def b():\n    x()\n\ndef x():\n    y()\n\ndef y():\n    pass\n";
    let repo = TempRepo::new("dfs", &[("m.py", src.to_string())]);
    let (ok, text) = tool_text_in(
        &repo.0,
        "explore",
        json!({ "question": "s", "seeds": ["m.py::s"], "mode": "dfs", "edge_kinds": ["calls"], "depth": 3 }),
    );
    assert!(ok, "{text}");
    assert!(text.contains("[2] m.py::x (function)"), "{text}");
    assert!(text.contains("[3] m.py::y (function)"), "{text}");
}

#[test]
fn explore_seed_selection_edge_cases() {
    // Words found only in paths do not add unrelated seeds.
    let (_, base) = tool_text(
        "explore",
        json!({ "question": "how does UserService create users" }),
    );
    let (_, with_path_word) = tool_text(
        "explore",
        json!({ "question": "how does UserService create users in src app" }),
    );
    let seeds = |t: &str| {
        t.lines()
            .find(|l| l.starts_with("Traversal:"))
            .unwrap()
            .to_string()
    };
    assert_eq!(
        seeds(&base),
        seeds(&with_path_word).replace("in src app", "")
    );

    // A bare suffix is not a file lookup.
    let (_, text) = tool_text("explore", json!({ "question": "ts" }));
    assert!(!text.contains("seeds: web/src/lib/api.ts\n"), "{text}");

    // Questions made only of relation words are rejected.
    let (ok, text) = tool_text("explore", json!({ "question": "who calls" }));
    assert!(!ok && text.contains("no term naming code"), "{text}");

    // Blank seeds fall back to the question.
    let (ok, text) = tool_text(
        "explore",
        json!({ "question": "UserService", "seeds": " , " }),
    );
    assert!(
        ok && text.contains("★ py/app/services/users.py::UserService (class)"),
        "{text}"
    );

    // Wrong argument types are errors, not silent defaults.
    let (ok, text) = tool_text(
        "explore",
        json!({ "question": "UserService", "depth": "3" }),
    );
    assert!(
        !ok && text.contains("`depth` must be a non-negative integer"),
        "{text}"
    );
    let (ok, text) = tool_text(
        "explore",
        json!({ "question": "UserService", "seeds": [1] }),
    );
    assert!(
        !ok && text.contains("`seeds` must contain strings"),
        "{text}"
    );
    let (ok, _) = tool_text(
        "explore",
        json!({ "question": "UserService", "mode": "DFS" }),
    );
    assert!(ok);
}

#[test]
fn explore_prefers_exact_names() {
    let src = "def resolve():\n    pass\n\ndef resolve_imports():\n    resolve()\n\n\
               def resolve_names():\n    resolve_imports()\n\ndef resolve_types():\n    resolve_imports()\n";
    let repo = TempRepo::new("exact", &[("m.py", src.to_string())]);
    let (ok, text) = tool_text_in(
        &repo.0,
        "explore",
        json!({ "question": "who calls resolve" }),
    );
    assert!(ok, "{text}");
    assert!(text.contains("★ m.py::resolve (function)"), "{text}");
}

#[test]
fn mcp_stdio_handshake() {
    let mut child = codenav()
        .arg("serve")
        .arg(fixture())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_symbols","arguments":{"query":"ItemStore"}}}),
    ];
    {
        let stdin = child.stdin.as_mut().unwrap();
        for r in &requests {
            writeln!(stdin, "{r}").unwrap();
        }
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let lines: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "notifications must not be answered");
    assert_eq!(lines[0]["result"]["serverInfo"]["name"], "codenav");
    let tools = lines[1]["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["name"] == "explore"));
    let text = lines[2]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("ios/Sources/App/Model.swift::ItemStore"),
        "{text}"
    );
}
