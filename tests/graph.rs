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

/// Calls a tool asking for the full structured result.
fn tool(name: &str, mut args: Value) -> Value {
    args["format"] = json!("json");
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
fn impact_reaches_callers_of_overridden_members() {
    let impact = tool(
        "impact_analysis",
        json!({ "id": "ios/Sources/App/Model.swift::ItemStore::load" }),
    );
    let swift = impact["by_file"]["ios/Sources/App/ViewModel.swift"].to_string();
    assert!(swift.contains("ViewModel::refresh"), "{impact:#}");
    assert_eq!(
        impact["overridden_members"],
        json!(["ios/Sources/App/Model.swift::Repository::load"]),
        "{impact:#}"
    );

    let impact = tool(
        "impact_analysis",
        json!({ "id": "web/src/lib/api.ts::HttpClient::get" }),
    );
    assert!(
        impact["by_file"]["web/src/components/Profile.tsx"].is_array(),
        "{impact:#}"
    );
    assert_eq!(
        impact["overridden_members"],
        json!(["web/src/lib/api.ts::Client::get"]),
        "{impact:#}"
    );
}

/// The call resolves to the interface method only (the caller imports just the
/// interface), so the implementation is reached only through the override link.
#[test]
fn impact_follows_overrides_across_files_and_levels() {
    let repo = TempRepo::new(
        "overrides",
        &[
            (
                "lib/client.ts",
                "export interface Client { get(path: string): string; }\n".into(),
            ),
            (
                "impl/http.ts",
                "import { Client } from '../lib/client';\n\
                 export class HttpClient implements Client {\n  get(path: string): string { return path; }\n}\n"
                    .into(),
            ),
            (
                "impl/cached.ts",
                "import { HttpClient } from './http';\n\
                 export class CachedClient extends HttpClient {\n  get(path: string): string { return path; }\n  clear() {}\n}\n"
                    .into(),
            ),
            (
                "impl/mock.ts",
                "import { Client } from '../lib/client';\n\
                 export class MockClient implements Client {\n  get(path: string): string { return ''; }\n}\n"
                    .into(),
            ),
            (
                "app/use.ts",
                "import { Client } from '../lib/client';\n\
                 export function load(c: Client) { return c.get('/x'); }\n"
                    .into(),
            ),
        ],
    );
    let impact = |id: &str| {
        let (ok, text) = tool_text_in(
            &repo.0,
            "impact_analysis",
            json!({ "id": id, "format": "json" }),
        );
        assert!(ok, "{text}");
        serde_json::from_str::<Value>(&text).unwrap()
    };

    // Precondition: the call is linked to the interface only.
    let (ok, text) = tool_text_in(
        &repo.0,
        "get_symbol",
        json!({ "id": "app/use.ts::load", "format": "json" }),
    );
    assert!(ok && text.contains("lib/client.ts::Client::get"), "{text}");
    assert!(!text.contains("impl/http.ts::HttpClient::get"), "{text}");

    let v = impact("impl/http.ts::HttpClient::get");
    assert!(v["by_file"]["app/use.ts"].is_array(), "{v:#}");
    assert_eq!(
        v["overridden_members"],
        json!(["lib/client.ts::Client::get"]),
        "{v:#}"
    );

    // Two levels up: CachedClient -> HttpClient -> Client.
    let v = impact("impl/cached.ts::CachedClient::get");
    assert!(v["by_file"]["app/use.ts"].is_array(), "{v:#}");
    assert_eq!(
        v["overridden_members"],
        json!([
            "impl/http.ts::HttpClient::get",
            "lib/client.ts::Client::get"
        ]),
        "{v:#}"
    );

    // Analysing the whole class covers its methods' overrides too.
    let v = impact("impl/cached.ts::CachedClient");
    assert!(v["by_file"]["app/use.ts"].is_array(), "{v:#}");

    // Only supertypes: a sibling implementation is not pulled in, and a method that
    // overrides nothing reports no overridden members.
    let v = impact("impl/http.ts::HttpClient::get");
    assert!(!v.to_string().contains("MockClient"), "{v:#}");
    let v = impact("impl/cached.ts::CachedClient::clear");
    assert!(v.get("overridden_members").is_none(), "{v:#}");
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
        json!({ "question": "Profile component", "token_budget": 200 }),
    );
    assert!(ok, "{text}");
    assert!(text.contains("[!] TRUNCATED"), "{text}");
    assert!(text.len() <= 200 * 3, "output too long: {}", text.len());
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
        json!({ "question": "hub", "token_budget": 200 }),
    );
    assert!(ok, "{text}");
    assert!(
        text.len() <= 200 * 3,
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
fn every_structured_tool_respects_token_budget() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let calls = [
        ("graph_overview", json!({ "top": 50 })),
        ("module_graph", json!({ "depth": 3 })),
        ("search_symbols", json!({ "query": "e", "limit": 200 })),
        ("get_symbol", json!({ "id": "src/indexer.rs::Builder" })),
        (
            "get_neighbors",
            json!({ "id": "src/indexer.rs::Builder", "depth": 2 }),
        ),
        ("file_outline", json!({ "path": "src/indexer.rs" })),
        ("impact_analysis", json!({ "id": "src/graph.rs::Graph" })),
        ("explore", json!({ "question": "how is the graph built" })),
    ];
    let mut truncated = 0;
    for (name, mut args) in calls {
        args["token_budget"] = json!(200);
        let (ok, text) = tool_text_in(repo, name, args.clone());
        assert!(ok, "{name}: {text}");
        let text = text.trim_end();
        assert!(
            text.len() <= 200 * 3,
            "{name}: {} bytes\n{text}",
            text.len()
        );
        // Small outputs may fit; the others must announce the cut.
        let Some(notice) = text.lines().find(|l| l.starts_with("[!] TRUNCATED")) else {
            continue;
        };
        truncated += 1;

        // The suggested budget really shows everything.
        let suggested: usize = notice
            .split("token_budget=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("{name}: no suggested budget in `{notice}`"));
        args["token_budget"] = json!(suggested);
        let (ok, full) = tool_text_in(repo, name, args);
        assert!(
            ok && !full.contains("[!] TRUNCATED"),
            "{name} with {suggested}: {full}"
        );
    }
    assert!(
        truncated >= 6,
        "only {truncated} tools were cut at a 200-token budget"
    );
}

#[test]
fn read_code_is_budgeted_and_clips_long_lines() {
    let repo = TempRepo::new(
        "minified",
        &[("min.js", format!("var a = {};\n", "1+".repeat(250_000)))],
    );
    let (ok, text) = tool_text_in(
        &repo.0,
        "read_code",
        json!({ "file": "min.js", "token_budget": 200 }),
    );
    assert!(ok, "{text}");
    assert!(text.len() <= 600, "{} bytes", text.len());
    assert!(text.contains("…[+"), "{text}");
}

#[test]
fn text_and_json_formats() {
    let (ok, text) = tool_text("get_symbol", json!({ "id": "UserService::create" }));
    assert!(ok, "{text}");
    assert!(
        text.starts_with("id: py/app/services/users.py::UserService::create\n"),
        "{text}"
    );
    assert!(
        text.contains("  calls:\n    - py/app/models.py::User\n"),
        "{text}"
    );
    assert!(text.contains("code: |\n      6 |     def create"), "{text}");

    let (ok, json_text) = tool_text(
        "get_symbol",
        json!({ "id": "UserService::create", "format": "json" }),
    );
    assert!(ok);
    let v: Value = serde_json::from_str(&json_text).unwrap();
    assert_eq!(v["kind"], "method");

    let (ok, text) = tool_text("graph_overview", json!({ "format": "xml" }));
    assert!(
        !ok && text.contains("format must be text or json"),
        "{text}"
    );

    let (ok, text) = tool_text(
        "read_code",
        json!({ "file": "py/app/models.py", "start_line": 1, "end_line": 2 }),
    );
    assert!(
        ok && text.starts_with("py/app/models.py\n    1 | class Base:"),
        "{text}"
    );
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
    let get_symbol = tools.iter().find(|t| t["name"] == "get_symbol").unwrap();
    let props = &get_symbol["inputSchema"]["properties"];
    assert!(props["token_budget"].is_object() && props["format"].is_object());
    let text = lines[2]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("ios/Sources/App/Model.swift::ItemStore"),
        "{text}"
    );
}
