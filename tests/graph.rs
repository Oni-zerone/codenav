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

/// Community of every non-external node in the fixture, by node id.
fn fixture_communities() -> (Value, Vec<(String, u64)>) {
    let out = codenav().arg("index").arg(fixture()).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let g: Value =
        serde_json::from_slice(&std::fs::read(fixture().join(".codenav/graph.cnav")).unwrap())
            .unwrap();
    let mut nodes = Vec::new();
    for n in g["nodes"].as_array().unwrap() {
        match n["community"].as_u64() {
            Some(c) => nodes.push((n["id"].as_str().unwrap().to_string(), c)),
            None => assert_eq!(n["kind"], "external", "{n}"),
        }
    }
    (g, nodes)
}

#[test]
fn communities_separate_the_fixture_apps() {
    let (g, nodes) = fixture_communities();
    let communities = g["communities"].as_array().unwrap();
    let mut app_of = std::collections::HashMap::new();
    for (id, c) in &nodes {
        let app = id.split('/').next().unwrap();
        let prev = app_of.entry(*c).or_insert(app);
        assert_eq!(*prev, app, "community {c} mixes {prev} and {app}");
    }
    for app in ["py", "web", "goapp", "ios"] {
        assert!(app_of.values().any(|a| *a == app), "{app} has no community");
    }
    // Ids are dense, largest first, and sizes match the nodes.
    for (c, info) in communities.iter().enumerate() {
        let size = nodes.iter().filter(|(_, n)| *n == c as u64).count();
        assert_eq!(info["size"], json!(size), "{info}");
        assert!(!info["label"].as_str().unwrap().is_empty());
    }
    let sizes: Vec<u64> = communities
        .iter()
        .map(|c| c["size"].as_u64().unwrap())
        .collect();
    assert!(sizes.windows(2).all(|w| w[0] >= w[1]), "{sizes:?}");

    // Deterministic: re-indexing gives the same assignment.
    let (_, again) = fixture_communities();
    assert_eq!(nodes, again);
}

#[test]
fn community_tools() {
    let ios = "ios/Sources/App/Model.swift::ItemStore";
    let c = tool("get_community", json!({ "id": ios }));
    assert_eq!(c["node"], ios, "{c:#}");
    let files = c["files"].to_string();
    assert!(files.contains("ios/Sources/App/Model.swift"), "{c:#}");
    assert!(!files.contains("py/"), "{c:#}");
    let members = c["members"].to_string();
    assert!(members.contains(ios), "{c:#}");

    // Same community by number, and from get_symbol / graph_overview.
    let id = c["id"].clone();
    let by_number = tool("get_community", json!({ "community": id }));
    assert_eq!(by_number["label"], c["label"]);
    assert!(by_number.get("node").is_none());
    let sym = tool("get_symbol", json!({ "id": ios, "include_code": false }));
    assert_eq!(sym["community"]["id"], id, "{sym:#}");
    assert_eq!(sym["community"]["label"], c["label"]);
    let overview = tool("graph_overview", json!({}));
    let largest = overview["largest_communities"].as_array().unwrap();
    assert_eq!(
        overview["community_count"],
        json!(largest.len()),
        "{overview:#}"
    );
    assert!(
        largest
            .iter()
            .any(|l| l["id"] == id && l["label"] == c["label"])
    );

    // Limit, and the error paths.
    let small = tool("get_community", json!({ "id": ios, "limit": 1 }));
    assert_eq!(small["members"].as_array().unwrap().len(), 1);
    assert_eq!(small["truncated"], true);
    for (args, expected) in [
        (json!({ "community": 999 }), "no community 999"),
        (json!({}), "pass `id`"),
        (json!({ "id": "ext:Foundation" }), "has no community"),
    ] {
        let (ok, text) = tool_text("get_community", args);
        assert!(!ok && text.contains(expected), "{text}");
    }
}

#[test]
fn hub_percentile_is_kept_across_reindexing() {
    let repo = TempRepo::new(
        "hubpct",
        &[("a.py", "def f():\n    g()\n\ndef g():\n    pass\n".into())],
    );
    let saved = || -> Value {
        let g: Value =
            serde_json::from_slice(&std::fs::read(repo.0.join(".codenav/graph.cnav")).unwrap())
                .unwrap();
        g["hub_percentile"].clone()
    };
    let run = |args: &[&str]| -> String {
        let out = codenav()
            .args(args)
            .arg("--path")
            .arg(&repo.0)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let index = |extra: &[&str]| {
        let out = codenav()
            .arg("index")
            .args(extra)
            .arg(&repo.0)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };

    index(&["--hub-percentile", "90"]);
    assert_eq!(saved(), json!(90.0));
    // A plain `index` keeps the saved value.
    index(&[]);
    assert_eq!(saved(), json!(90.0));
    // `tool` re-indexing a stale graph keeps it too.
    std::fs::write(repo.0.join("b.py"), "def h():\n    pass\n").unwrap();
    let log = run(&["tool", "graph_overview"]);
    assert!(log.contains("stale"), "{log}");
    assert_eq!(saved(), json!(90.0));
    // An explicit value replaces it.
    index(&["--hub-percentile", "100"]);
    assert_eq!(saved(), json!(100.0));
    // `serve` with a different value re-indexes instead of reusing the fresh graph.
    let out = codenav()
        .args(["serve", "--hub-percentile", "80"])
        .arg(&repo.0)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && log.contains("hub percentile changed (100 → 80)"),
        "{log}"
    );
    assert_eq!(saved(), json!(80.0));

    // An index in an old format is reported as such, not as missing.
    std::fs::write(
        repo.0.join(".codenav/graph.cnav"),
        r#"{"version":1,"root":"","indexed_at":0,"nodes":[],"edges":[]}"#,
    )
    .unwrap();
    let log = run(&["tool", "graph_overview"]);
    assert!(
        log.contains("outdated") && !log.contains("no index found"),
        "{log}"
    );
    // The saved percentile of an outdated index is not trusted: back to the default.
    assert_eq!(saved(), json!(99.0));
}

/// Confidence of the edge `from -kind-> to` as get_symbol reports it.
fn confidence(from: &str, kind: &str, to: &str) -> String {
    let v = tool("get_symbol", json!({ "id": from, "include_code": false }));
    let list = v["outgoing"][kind]
        .as_array()
        .unwrap_or_else(|| panic!("{from} has no {kind} edges: {v:#}"));
    for item in list {
        if item.as_str() == Some(to) {
            return "exact".into();
        }
        if item["id"] == to {
            return item["confidence"].as_str().unwrap_or("exact").into();
        }
    }
    panic!("no {kind} edge {from} -> {to}: {v:#}")
}

#[test]
fn edges_carry_the_expected_confidence() {
    let cases = [
        // Same file / imported file / self / package qualifier: exact.
        (
            "py/app/models.py::User",
            "inherits",
            "py/app/models.py::Base",
            "exact",
        ),
        (
            "py/app/services/users.py::UserService::create",
            "calls",
            "py/app/models.py::User",
            "exact",
        ),
        (
            "ios/Sources/App/ViewModel.swift::ViewModel::reset",
            "calls",
            "ios/Sources/App/ViewModel.swift::ViewModel::refresh",
            "exact",
        ),
        (
            "goapp/main.go::main",
            "calls",
            "goapp/internal/store/store.go::New",
            "exact",
        ),
        (
            "py/app/services/users.py",
            "imports",
            "py/app/models.py",
            "exact",
        ),
        // Same directory, no import (Swift) / unknown receiver with a unique method: inferred.
        (
            "ios/Sources/App/ViewModel.swift::ViewModel",
            "references",
            "ios/Sources/App/Model.swift::Repository",
            "inferred",
        ),
        (
            "goapp/main.go::main",
            "calls",
            "goapp/internal/store/store.go::Store::Get",
            "inferred",
        ),
        // Several same-named candidates: ambiguous.
        (
            "ios/Sources/App/ViewModel.swift::ViewModel::refresh",
            "calls",
            "ios/Sources/App/Model.swift::Repository::load",
            "ambiguous",
        ),
        (
            "ios/Sources/App/ViewModel.swift::ViewModel::refresh",
            "calls",
            "ios/Sources/App/Model.swift::ItemStore::load",
            "ambiguous",
        ),
        (
            "web/src/components/Profile.tsx::Profile",
            "calls",
            "web/src/lib/api.ts::HttpClient::get",
            "ambiguous",
        ),
    ];
    for (from, kind, to, expected) in cases {
        assert_eq!(
            confidence(from, kind, to),
            expected,
            "{from} -{kind}-> {to}"
        );
    }

    let overview = tool("graph_overview", json!({}));
    let shares = &overview["edge_confidence"];
    let total: f64 = ["exact", "inferred", "ambiguous"]
        .iter()
        .map(|c| shares[c]["percent"].as_f64().unwrap())
        .sum();
    assert!((total - 100.0).abs() < 0.5, "{shares}");
    assert!(
        shares["ambiguous"]["edges"].as_u64().unwrap() >= 3,
        "{shares}"
    );
}

#[test]
fn min_confidence_filters_traversals() {
    let refresh = "ios/Sources/App/ViewModel.swift::ViewModel::refresh";
    let load = "ios/Sources/App/Model.swift::ItemStore::load";

    let all = tool(
        "get_neighbors",
        json!({ "id": refresh, "direction": "out" }),
    )
    .to_string();
    assert!(
        all.contains(load) && all.contains("\"confidence\":\"ambiguous\""),
        "{all}"
    );
    let sure = tool(
        "get_neighbors",
        json!({ "id": refresh, "direction": "out", "min_confidence": "inferred" }),
    )
    .to_string();
    assert!(
        !sure.contains(load) && sure.contains("ViewModel::render"),
        "{sure}"
    );

    let impact = tool("impact_analysis", json!({ "id": load }));
    let text = impact["by_file"].to_string();
    assert!(
        text.contains("ViewModel::refresh (method) via called_by [depth 1, ambiguous]"),
        "{impact:#}"
    );
    // The weakest link is reported along the path.
    assert!(
        text.contains("ViewModel::reset (method) via called_by [depth 2, ambiguous]"),
        "{impact:#}"
    );
    let impact = tool(
        "impact_analysis",
        json!({ "id": load, "min_confidence": "exact" }),
    );
    assert_eq!(impact["affected_symbols"], 0, "{impact:#}");

    let path = |min: &str| {
        tool(
            "find_paths",
            json!({ "from": refresh, "to": load, "directed": true, "min_confidence": min }),
        )["found"]
            .clone()
    };
    assert_eq!(path("ambiguous"), true);
    assert_eq!(path("exact"), false);

    let (ok, text) = tool_text(
        "explore",
        json!({ "seeds": [refresh], "question": "", "min_confidence": "exact" }),
    );
    assert!(
        ok && text.contains("min confidence: exact") && !text.contains(load),
        "{text}"
    );

    let (ok, text) = tool_text(
        "get_neighbors",
        json!({ "id": refresh, "min_confidence": "sure" }),
    );
    assert!(
        !ok && text.contains("exact, inferred or ambiguous"),
        "{text}"
    );
}

#[test]
fn audit_edges_lists_guesses() {
    let v = tool("audit_edges", json!({}));
    assert_eq!(v["confidence"], "ambiguous");
    assert_eq!(
        v["totals_by_kind"]["calls"]["ambiguous"], v["matching_edges"],
        "{v:#}"
    );
    let swift = v["by_file"]["ios/Sources/App/ViewModel.swift"].to_string();
    assert!(
        swift
            .contains("L7 ViewModel::refresh calls → ios/Sources/App/Model.swift::ItemStore::load"),
        "{v:#}"
    );

    let v = tool(
        "audit_edges",
        json!({ "confidence": "inferred", "path": "goapp/" }),
    );
    assert_eq!(v["matching_edges"], 1, "{v:#}");
    assert!(
        v["by_file"]["goapp/main.go"]
            .to_string()
            .contains("Store::Get"),
        "{v:#}"
    );

    let v = tool(
        "audit_edges",
        json!({ "confidence": "exact", "edge_kinds": ["imports"], "limit": 2 }),
    );
    assert_eq!(
        v["by_file"]
            .as_object()
            .unwrap()
            .values()
            .map(|l| l.as_array().unwrap().len())
            .sum::<usize>(),
        2
    );
    assert_eq!(v["truncated"], true);
}

#[test]
fn owner_and_overload_confidence() {
    let repo = TempRepo::new(
        "confowner",
        &[
            ("src/a.rs", "pub struct Foo;\n".into()),
            ("src/b.rs", "pub struct Foo;\n".into()),
            // `impl Foo` with two candidate owners in the directory: a guess.
            ("src/c.rs", "impl Foo {\n    pub fn m(&self) {}\n}\n".into()),
            // Two same-named members of the current type: the owner is certain.
            (
                "src/d.rs",
                "pub struct T;\nimpl T {\n    fn f(&self) {}\n    fn show(&self) { self.f(); }\n}\n\
                 impl Other for T {\n    fn f(&self) {}\n}\n"
                    .into(),
            ),
        ],
    );
    let symbol = |id: &str| -> Value {
        let (ok, text) = tool_text_in(
            &repo.0,
            "get_symbol",
            json!({ "id": id, "include_code": false, "format": "json" }),
        );
        assert!(ok, "{text}");
        serde_json::from_str(&text).unwrap()
    };
    let foo = symbol("src/a.rs::Foo");
    assert_eq!(
        foo["outgoing"]["contains"],
        json!([{ "id": "src/c.rs::Foo::m", "confidence": "ambiguous" }]),
        "{foo:#}"
    );
    let show = symbol("src/d.rs::T::show");
    let calls = show["outgoing"]["calls"].to_string();
    assert!(
        calls.contains("src/d.rs::T::f") && !calls.contains("confidence"),
        "{show:#}"
    );

    // audit_edges paths are whole components.
    let (ok, text) = tool_text_in(
        &repo.0,
        "audit_edges",
        json!({ "confidence": "exact", "path": "src/a", "format": "json" }),
    );
    assert!(ok, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["matching_edges"],
        0,
        "{text}"
    );
}

/// get_symbol (JSON) of `id` in a temporary repository.
fn symbol_in(repo: &TempRepo, id: &str) -> Value {
    let (ok, text) = tool_text_in(
        &repo.0,
        "get_symbol",
        json!({ "id": id, "include_code": false, "format": "json" }),
    );
    assert!(ok, "{id}: {text}");
    serde_json::from_str(&text).unwrap()
}

#[test]
fn jsx_lowercase_tags_are_not_calls() {
    let repo = TempRepo::new(
        "jsx",
        &[
            (
                "a.ts",
                "export function render() {\n  const p = (t: string) => `<p>${t}</p>`;\n  return [\"x\"].map(p).join(\"\");\n}\n\
                 function summary() { return 1; }\n"
                    .into(),
            ),
            (
                "ui.tsx",
                "export function Card() { return null; }\n".into(),
            ),
            (
                "b.tsx",
                "import * as ui from './ui';\nimport { Card } from './ui';\n\
                 export function Page() {\n  return <details><summary>s</summary><p>ciao</p><Card /><ui.Card></ui.Card></details>;\n}\n"
                    .into(),
            ),
        ],
    );
    let page = symbol_in(&repo, "b.tsx::Page");
    let calls = page["outgoing"]["calls"].to_string();
    assert!(calls.contains("ui.tsx::Card"), "{page:#}");
    assert!(
        !calls.contains("render") && !calls.contains("summary"),
        "{page:#}"
    );
    // Both the plain and the qualified tag count as calls to the component.
    assert!(
        page["outgoing"]["calls"]
            .to_string()
            .contains("\"count\":2"),
        "{page:#}"
    );
    let p = symbol_in(&repo, "a.ts::render::p");
    assert!(p["incoming"].get("called_by").is_none(), "{p:#}");
}

#[test]
fn js_resolution_respects_module_scope() {
    let repo = TempRepo::new(
        "jsscope",
        &[
            // Same-named private helpers in two files that don't import each other.
            ("lib/a.ts", "function helper() {}\nexport function a() { helper(); }\n".into()),
            ("lib/b.ts", "function helper() {}\nexport function shared() {}\nexport class Repo { load() {} }\n".into()),
            ("app/c.ts", "export function c() { helper(); }\n".into()),
            // Barrel re-export, explicit import, nested function, instance method.
            ("lib/index.ts", "export * from './b';\n".into()),
            (
                "app/d.ts",
                "import { shared } from '../lib/index';\n\
                 export function d(r: any) {\n  shared();\n  r.load();\n  function inner() {}\n  inner();\n}\n\
                 export function e() { inner(); }\n"
                    .into(),
            ),
            // A plain script: its top-level functions are globals.
            ("web/globals.js", "function legacyGlobal() {}\n".into()),
            ("web/page.js", "legacyGlobal();\n".into()),
        ],
    );
    let a = symbol_in(&repo, "lib/a.ts::a");
    assert_eq!(
        a["outgoing"]["calls"],
        json!([{ "id": "lib/a.ts::helper", "call_sites": [2] }]),
        "{a:#}"
    );
    let c = symbol_in(&repo, "app/c.ts::c");
    assert!(
        c["outgoing"].get("calls").is_none(),
        "no import, no edge: {c:#}"
    );

    let d = symbol_in(&repo, "app/d.ts::d");
    let calls = d["outgoing"]["calls"].to_string();
    assert!(
        calls.contains("\"lib/b.ts::shared\""),
        "barrel import is exact: {d:#}"
    );
    assert!(calls.contains("lib/b.ts::Repo::load"), "{d:#}");
    assert!(calls.contains("app/d.ts::d::inner"), "{d:#}");
    let e = symbol_in(&repo, "app/d.ts::e");
    assert!(
        e["outgoing"].get("calls").is_none(),
        "nested functions are local: {e:#}"
    );

    let page = symbol_in(&repo, "web/page.js");
    assert!(
        page["outgoing"]["calls"]
            .to_string()
            .contains("web/globals.js::legacyGlobal"),
        "{page:#}"
    );
}

#[test]
fn js_top_level_constants_are_indexed_and_referenced() {
    let repo = TempRepo::new(
        "jsconst",
        &[
            (
                "schema.ts",
                "import { z } from 'zod';\nexport const planSchema = z.object({});\nconst LIMITS = new Set([1]);\n\
                 export const build = () => 1;\nexport function check(x: unknown) {\n  const local = 1;\n  return LIMITS.has(local) && planSchema.parse(x);\n}\n"
                    .into(),
            ),
            (
                "use.ts",
                "import { planSchema } from './schema';\nexport function save(x: unknown) { return planSchema.parse(x); }\n\
                 export const cfg = { planSchema };\n"
                    .into(),
            ),
        ],
    );
    let schema = symbol_in(&repo, "schema.ts::planSchema");
    assert_eq!(schema["kind"], "constant", "{schema:#}");
    assert_eq!(symbol_in(&repo, "schema.ts::LIMITS")["kind"], "constant");
    assert_eq!(
        symbol_in(&repo, "schema.ts::build")["kind"],
        "function",
        "functions win"
    );
    let (ok, text) = tool_text_in(
        &repo.0,
        "search_symbols",
        json!({ "query": "local", "kinds": ["constant"] }),
    );
    assert!(
        ok && !text.contains("schema.ts::check::local"),
        "local consts are not indexed: {text}"
    );

    let check = symbol_in(&repo, "schema.ts::check");
    let refs = check["outgoing"]["references"].to_string();
    assert!(
        refs.contains("schema.ts::planSchema") && refs.contains("schema.ts::LIMITS"),
        "{check:#}"
    );
    let save = symbol_in(&repo, "use.ts::save");
    assert_eq!(
        save["outgoing"]["references"],
        json!(["schema.ts::planSchema"]),
        "imported: exact {save:#}"
    );
    let cfg = symbol_in(&repo, "use.ts::cfg");
    assert_eq!(
        cfg["outgoing"]["references"],
        json!(["schema.ts::planSchema"]),
        "shorthand {cfg:#}"
    );
}

#[test]
fn js_locals_shadow_constants_and_imports_are_not_uses() {
    let repo = TempRepo::new(
        "jsshadow",
        &[
            (
                "a.ts",
                "export const config = { x: 1 };\n\
                 export function param(config: any) { return config.x; }\n\
                 export function destructured({ config }: any, [cfg] = []) { return config; }\n\
                 export const arrow = (config: any) => config;\n\
                 export function local() { const config = 2; try {} catch (config) {} return config; }\n\
                 export function loop(xs: any[]) { for (const config of xs) { config; } }\n\
                 export function real() { return config.x; }\n"
                    .into(),
            ),
            (
                "b.ts",
                "import { config } from './a';\nexport function other(config: any) { return config; }\n\
                 export function uses() { return config; }\n"
                    .into(),
            ),
        ],
    );
    for id in [
        "a.ts::param",
        "a.ts::destructured",
        "a.ts::arrow",
        "a.ts::local",
        "a.ts::loop",
        "b.ts::other",
        "b.ts",
    ] {
        let v = symbol_in(&repo, id);
        assert!(
            v["outgoing"].get("references").is_none(),
            "{id} does not use the constant: {v:#}"
        );
    }
    for id in ["a.ts::real", "b.ts::uses"] {
        let v = symbol_in(&repo, id);
        assert_eq!(
            v["outgoing"]["references"],
            json!(["a.ts::config"]),
            "{id}: {v:#}"
        );
    }
}

#[test]
fn js_module_edge_cases() {
    let repo = TempRepo::new(
        "jsedges",
        &[
            // Workspace package import the resolver cannot map to a file: still linked.
            ("packages/utils/index.ts", "export function helper() {}\n".into()),
            ("packages/app/x.ts", "import { helper } from '@acme/utils';\nexport function run() { helper(); }\n".into()),
            // `<motion.div>` is an intrinsic element, not a call to a `div` method.
            ("ui/anim.ts", "export class Motion { div() {} }\n".into()),
            ("ui/view.tsx", "import { Motion } from './anim';\nexport function View() { return <motion.div />; }\n".into()),
            // A private helper sharing a re-exported name stays private.
            ("lib/other.ts", "export function x() {}\n".into()),
            ("lib/barrel.ts", "function x() {}\nexport { x } from './other';\nexport function y() { x(); }\n".into()),
            ("lib/user.ts", "import { y } from './barrel';\nexport function z() { x(); y(); }\n".into()),
            // Scripts: a dynamic import keeps a file a script; CommonJS exports make a module.
            ("web/globals.js", "function legacyGlobal() {}\nfunction lazy() { return import('./chart.js'); }\n".into()),
            ("web/util.js", "function cjsOnly() {}\nmodule.exports = { cjsOnly };\n".into()),
            ("web/page.js", "legacyGlobal();\ncjsOnly();\n".into()),
            // Members of a namespaced class or an unexported class, through an instance.
            (
                "api/client.ts",
                "export namespace Api { export class Client { fetchAll() {} } }\nclass Hidden { secret() {} }\n\
                 export function make(): any { return new Hidden(); }\n"
                    .into(),
            ),
            ("api/use.ts", "export function go(c: any) { c.fetchAll(); c.secret(); }\n".into()),
        ],
    );
    let run = symbol_in(&repo, "packages/app/x.ts::run");
    assert!(
        run["outgoing"]["calls"]
            .to_string()
            .contains("packages/utils/index.ts::helper"),
        "{run:#}"
    );

    let view = symbol_in(&repo, "ui/view.tsx::View");
    assert!(view["outgoing"].get("calls").is_none(), "{view:#}");

    let z = symbol_in(&repo, "lib/user.ts::z");
    let calls = z["outgoing"]["calls"].to_string();
    assert!(
        calls.contains("lib/barrel.ts::y") && !calls.contains("lib/barrel.ts::x"),
        "{z:#}"
    );

    let page = symbol_in(&repo, "web/page.js");
    let calls = page["outgoing"]["calls"].to_string();
    assert!(calls.contains("web/globals.js::legacyGlobal"), "{page:#}");
    assert!(
        !calls.contains("cjsOnly"),
        "CommonJS needs require: {page:#}"
    );

    let go = symbol_in(&repo, "api/use.ts::go");
    let calls = go["outgoing"]["calls"].to_string();
    assert!(
        calls.contains("api/client.ts::Api::Client::fetchAll"),
        "{go:#}"
    );
    assert!(calls.contains("api/client.ts::Hidden::secret"), "{go:#}");
}

#[test]
fn indexes_from_an_older_analysis_are_rebuilt() {
    let repo = TempRepo::new("analysis", &[("a.py", "def f():\n    pass\n".into())]);
    let out = codenav().arg("index").arg(&repo.0).output().unwrap();
    assert!(out.status.success());
    let path = repo.0.join(".codenav/graph.cnav");
    let mut g: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(g["analysis"].as_u64().unwrap() >= 1, "{}", g["analysis"]);
    g.as_object_mut().unwrap().remove("analysis");
    std::fs::write(&path, g.to_string()).unwrap();

    let out = codenav()
        .args(["tool", "graph_overview"])
        .arg("--path")
        .arg(&repo.0)
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && log.contains("stale"), "{log}");
}

#[test]
fn find_cycles_between_files_and_modules() {
    let repo = TempRepo::new(
        "cycles",
        &[
            (
                "app/a.ts",
                "import { b } from './b';\nexport function a() { b(); }\n".into(),
            ),
            (
                "app/b.ts",
                "import { a } from './a';\nexport function b() { a(); }\n".into(),
            ),
            // A three-file ring across two directories, plus a file outside it.
            (
                "lib/x.ts",
                "import { y } from './y';\nexport function x() { y(); }\n".into(),
            ),
            (
                "lib/y.ts",
                "import { z } from '../core/z';\nexport function y() { z(); }\n".into(),
            ),
            (
                "core/z.ts",
                "import { x } from '../lib/x';\nexport function z() { x(); }\n".into(),
            ),
            (
                "core/leaf.ts",
                "import { z } from './z';\nexport function leaf() { z(); }\n".into(),
            ),
        ],
    );
    let cycles = |args: Value| -> Value {
        let mut args = args;
        args["format"] = json!("json");
        let (ok, text) = tool_text_in(&repo.0, "find_cycles", args);
        assert!(ok, "{text}");
        serde_json::from_str(&text).unwrap()
    };

    let v = cycles(json!({}));
    assert_eq!(v["cyclic_components"], 2, "{v:#}");
    let comps = v["components"].as_array().unwrap();
    assert_eq!(comps[0]["size"], 3, "largest first: {v:#}");
    assert_eq!(
        comps[0]["cycles"],
        json!(["core/z.ts --imports--> lib/x.ts --imports--> lib/y.ts --imports--> core/z.ts"]),
        "{v:#}"
    );
    assert_eq!(
        comps[1]["cycles"],
        json!(["app/a.ts --imports--> app/b.ts --imports--> app/a.ts"]),
        "{v:#}"
    );
    assert!(
        !v.to_string().contains("leaf"),
        "not part of a cycle: {v:#}"
    );

    // Bounded length, limit, other edge kinds, module level.
    let v = cycles(json!({ "max_length": 2 }));
    assert!(
        comps_note(&v).contains("no cycle within max_length 2"),
        "{v:#}"
    );
    let v = cycles(json!({ "limit": 1 }));
    assert_eq!(v["truncated"], true);
    assert_eq!(v["components"].as_array().unwrap().len(), 1);
    let v = cycles(json!({ "edge_kinds": ["calls"] }));
    assert_eq!(
        v["cyclic_components"], 2,
        "calls form the same rings: {v:#}"
    );
    let v = cycles(json!({ "level": "module", "depth": 1 }));
    assert_eq!(
        v["components"][0]["cycles"],
        json!(["core --imports--> lib --imports--> core"]),
        "{v:#}"
    );

    let (ok, text) = tool_text_in(&repo.0, "find_cycles", json!({ "level": "dir" }));
    assert!(!ok && text.contains("file or module"), "{text}");
    for (args, expected) in [
        (json!({ "edge_kinds": ["contains"] }), "contains is not one"),
        (json!({ "max_length": 1 }), "between 2 and 20"),
        (json!({ "limit": 0 }), "at least 1"),
    ] {
        let (ok, text) = tool_text_in(&repo.0, "find_cycles", args);
        assert!(!ok && text.contains(expected), "{text}");
    }
}

fn comps_note(v: &Value) -> String {
    v["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["note"].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn fixture_has_no_cycles() {
    let all = json!(["imports", "calls", "inherits", "references"]);
    for level in ["file", "module"] {
        let v = tool(
            "find_cycles",
            json!({ "level": level, "edge_kinds": all, "min_confidence": "ambiguous" }),
        );
        assert_eq!(v["cyclic_components"], 0, "{level}: {v:#}");
    }
}

#[test]
fn overview_reports_the_indexed_commit() {
    let repo = TempRepo::new("vcs", &[("a.py", "def f():\n    pass\n".into())]);
    let overview = |root: &Path| -> Value {
        let (ok, text) = tool_text_in(root, "graph_overview", json!({ "format": "json" }));
        assert!(ok, "{text}");
        serde_json::from_str::<Value>(&text).unwrap()["index"].clone()
    };
    // Not a Git repository: no commit, no error.
    let index = overview(&repo.0);
    assert!(index["built_at_unix"].is_u64(), "{index}");
    assert!(
        index.get("commit").is_none() && index.get("warnings").is_none(),
        "{index}"
    );

    let git = |args: &[&str]| -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo.0)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.0.join(".gitignore"), ".codenav/\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "one"]);
    let first = git(&["rev-parse", "HEAD"]);
    let index = overview(&repo.0);
    assert_eq!(index["commit"], first, "{index}");
    assert_eq!(index["branch"], "main", "{index}");
    assert_eq!(index["uncommitted_changes"], false, "{index}");
    assert!(index.get("warnings").is_none(), "{index}");

    // A commit that changes no source: the graph is kept and its Git state updated.
    git(&["commit", "-q", "--allow-empty", "-m", "two"]);
    let second = git(&["rev-parse", "HEAD"]);
    let index = overview(&repo.0);
    assert_eq!(index["commit"], second, "{index}");
    assert!(index.get("warnings").is_none(), "{index}");

    // An untracked file that is indexed counts as an uncommitted change; others don't.
    std::fs::write(repo.0.join("notes.txt"), "x").unwrap();
    std::fs::write(repo.0.join("b.py"), "def g():\n    pass\n").unwrap();
    assert_eq!(overview(&repo.0)["uncommitted_changes"], true);
    std::fs::remove_file(repo.0.join("b.py")).unwrap();
    assert_eq!(overview(&repo.0)["uncommitted_changes"], false);
    // So does an edit to a tracked file.
    std::fs::write(repo.0.join("a.py"), "def f():\n    return 1\n").unwrap();
    assert_eq!(overview(&repo.0)["uncommitted_changes"], true);

    // A tracked subfolder indexed on its own belongs to the repository...
    std::fs::create_dir_all(repo.0.join("pkg")).unwrap();
    std::fs::write(repo.0.join("pkg/m.py"), "def m():\n    pass\n").unwrap();
    git(&["add", "pkg"]);
    git(&["commit", "-q", "-m", "pkg"]);
    let pkg = git(&["rev-parse", "HEAD"]);
    assert_eq!(overview(&repo.0.join("pkg"))["commit"], pkg);
    // ...an unversioned one does not inherit its HEAD.
    std::fs::create_dir_all(repo.0.join("scratch")).unwrap();
    std::fs::write(repo.0.join("scratch/s.py"), "def s():\n    pass\n").unwrap();
    let index = overview(&repo.0.join("scratch"));
    assert!(index.get("commit").is_none(), "{index}");
}

#[test]
fn tests_for_finds_direct_and_indirect_tests() {
    let repo = TempRepo::new(
        "testsfor",
        &[
            ("src/a.ts", "export function a() { return 1; }\n".into()),
            ("src/b.ts", "import { a } from './a';\nexport function b() { return a(); }\n".into()),
            (
                "src/a.test.ts",
                "import { a } from './a';\ntest('a', () => { a(); });\n".into(),
            ),
            (
                "src/b.test.ts",
                "import { b } from './b';\ntest('b', () => { b(); });\n".into(),
            ),
            // Named like the target but does not use it: not a test of it.
            ("src/c.test.ts", "export function unrelated() {}\n".into()),
            ("src/c.ts", "export function c() {}\n".into()),
            // Python and Rust conventions.
            ("py/models.py", "def save():\n    pass\n".into()),
            (
                "py/test_models.py",
                "from models import save\n\ndef test_save():\n    save()\n".into(),
            ),
            (
                "rs/lib.rs",
                "pub fn add() -> i32 { 1 }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn adds() { let x = add(); }\n}\n"
                    .into(),
            ),
        ],
    );
    let tests_for = |id: &str| -> Value {
        let (ok, text) = tool_text_in(&repo.0, "tests_for", json!({ "id": id, "format": "json" }));
        assert!(ok, "{text}");
        serde_json::from_str(&text).unwrap()
    };

    let v = tests_for("src/a.ts");
    assert_eq!(v["test_files"], 2, "{v:#}");
    assert_eq!(v["direct"][0]["file"], "src/a.test.ts", "{v:#}");
    assert_eq!(v["indirect"][0]["file"], "src/b.test.ts", "{v:#}");
    assert_eq!(v["indirect"][0]["distance"], 2, "{v:#}");
    let path = v["indirect"][0]["path"].as_str().unwrap();
    assert!(
        path.starts_with("src/b.test.ts") && path.ends_with("src/a.ts"),
        "{path}"
    );
    assert!(!v.to_string().contains("c.test.ts"), "{v:#}");

    // A symbol target, and nothing for code no test reaches.
    let v = tests_for("src/b.ts::b");
    assert_eq!(v["direct"][0]["file"], "src/b.test.ts", "{v:#}");
    assert_eq!(tests_for("src/c.ts")["test_files"], 0);

    let v = tests_for("py/models.py::save");
    assert_eq!(v["direct"][0]["file"], "py/test_models.py", "{v:#}");
    assert_eq!(v["direct"][0]["symbols"], json!(["test_save"]), "{v:#}");

    let v = tests_for("rs/lib.rs::add");
    assert_eq!(v["direct"][0]["symbols"], json!(["tests::adds"]), "{v:#}");
    let v = tests_for("rs/lib.rs");
    assert!(
        v["direct"][0]["path"]
            .as_str()
            .unwrap()
            .contains("inside the target"),
        "{v:#}"
    );
}

#[test]
fn tests_for_review_cases() {
    let repo = TempRepo::new(
        "testsfor2",
        &[
            // Importing a file is not using every symbol in it.
            (
                "src/b.ts",
                "export function b() {}\nexport function b2() {}\n".into(),
            ),
            (
                "src/b2.test.ts",
                "import { b2 } from './b';\ntest('b2', () => { b2(); });\n".into(),
            ),
            // Tests calling a helper in their own file.
            (
                "src/h.test.ts",
                "function makeUser() { return 1; }\ntest('u', () => { makeUser(); });\n".into(),
            ),
            // A test calling through the interface the target implements.
            (
                "src/repo.ts",
                "export interface Repo { fetchAll(): void; }\n".into(),
            ),
            (
                "src/db.ts",
                "import { Repo } from './repo';\nexport class Db implements Repo { fetchAll() {} }\n"
                    .into(),
            ),
            (
                "src/repo.test.ts",
                "import { Repo } from './repo';\nexport function check(r: Repo) { r.fetchAll(); }\n"
                    .into(),
            ),
            // Rust tests in their own file; fixtures are not tests.
            (
                "rs/parser.rs",
                "pub fn parse() {}\n#[cfg(test)]\nmod tests;\n".into(),
            ),
            (
                "rs/parser/tests.rs",
                "use super::*;\n#[test]\nfn parses() { parse(); }\n".into(),
            ),
            (
                "tests/fixtures/app.ts",
                "import { b } from '../../src/b';\nb();\n".into(),
            ),
        ],
    );
    let tests_for = |id: &str| -> Value {
        let (ok, text) = tool_text_in(&repo.0, "tests_for", json!({ "id": id, "format": "json" }));
        assert!(ok, "{text}");
        serde_json::from_str(&text).unwrap()
    };
    let v = tests_for("src/b.ts::b");
    assert_eq!(
        v["test_files"], 0,
        "b2.test.ts only uses b2; fixtures are not tests: {v:#}"
    );
    assert_eq!(
        tests_for("src/b.ts")["test_files"],
        1,
        "the file is imported"
    );

    let v = tests_for("src/h.test.ts::makeUser");
    assert_eq!(v["direct"][0]["file"], "src/h.test.ts", "{v:#}");

    let v = tests_for("src/db.ts::Db::fetchAll");
    assert_eq!(
        v["direct"][0]["file"], "src/repo.test.ts",
        "through Repo::fetchAll: {v:#}"
    );

    let v = tests_for("rs/parser.rs::parse");
    assert_eq!(v["direct"][0]["file"], "rs/parser/tests.rs", "{v:#}");
}

#[test]
fn insights_are_deterministic_on_the_fixture() {
    let a = tool("insights", json!({}));
    let b = tool("insights", json!({}));
    assert_eq!(a, b);
    for section in ["surprising_connections", "bridges", "suggested_questions"] {
        assert!(a[section].is_array(), "{section}: {a:#}");
    }
    // The Swift call to `load` is guessed between Repository and ItemStore.
    let questions = a["suggested_questions"].to_string();
    assert!(questions.contains("ViewModel::refresh"), "{a:#}");
}

#[test]
fn insights_find_lone_links_and_bridges() {
    // Two tight clusters (a*, b*) joined only through `bridge`.
    let mut files: Vec<(String, String)> = Vec::new();
    for c in ["a", "b"] {
        let mut src = String::new();
        for i in 0..6 {
            src.push_str(&format!("def {c}{i}():\n"));
            for j in 0..6 {
                if j != i {
                    src.push_str(&format!("    {c}{j}()\n"));
                }
            }
        }
        files.push((format!("{c}/{c}.py"), src));
    }
    files.push((
        "hub/bridge.py".into(),
        "from a.a import a0\nfrom b.b import b0\n\ndef bridge():\n    a0()\n    b0()\n".into(),
    ));
    let files: Vec<(&str, String)> = files.iter().map(|(p, s)| (p.as_str(), s.clone())).collect();
    let repo = TempRepo::new("insights", &files);
    let (ok, text) = tool_text_in(&repo.0, "insights", json!({ "format": "json" }));
    assert!(ok, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();

    assert_eq!(v["bridges"][0]["id"], "hub/bridge.py::bridge", "{v:#}");
    assert_eq!(
        v["bridges"][0]["connects"].as_array().unwrap().len(),
        2,
        "{v:#}"
    );
    let surprises = v["surprising_connections"].as_array().unwrap();
    assert_eq!(
        surprises.len(),
        2,
        "only the two links out of the hub: {v:#}"
    );
    for s in surprises {
        assert!(
            s["why"]
                .as_str()
                .unwrap()
                .contains("the only link between them"),
            "{v:#}"
        );
    }
    let questions = v["suggested_questions"].to_string();
    assert!(
        questions.contains("Why does hub/bridge.py::bridge connect"),
        "{v:#}"
    );
}

#[test]
fn call_sites_record_every_line() {
    let repo = TempRepo::new(
        "callsites",
        &[(
            "m.py",
            "def g():\n    pass\n\ndef f():\n    g()\n    x = 1\n    g()\n\ndef h():\n    g(); g()\n".into(),
        )],
    );
    let f = symbol_in(&repo, "m.py::f");
    assert_eq!(
        f["outgoing"]["calls"],
        json!([{ "id": "m.py::g", "count": 2, "call_sites": [5, 7] }]),
        "{f:#}"
    );
    // Two calls on one line: counted twice, one site.
    let h = symbol_in(&repo, "m.py::h");
    assert_eq!(
        h["outgoing"]["calls"],
        json!([{ "id": "m.py::g", "count": 2, "call_sites": [10] }]),
        "{h:#}"
    );
    // The callee sees where it is called from.
    let g = symbol_in(&repo, "m.py::g");
    assert!(
        g["incoming"]["called_by"]
            .to_string()
            .contains("\"call_sites\":[5,7]"),
        "{g:#}"
    );

    let (ok, text) = tool_text_in(
        &repo.0,
        "get_neighbors",
        json!({ "id": "m.py::f", "direction": "out", "format": "json" }),
    );
    assert!(ok && text.contains("\"call_sites\":[5,7]"), "{text}");
    let (ok, text) = tool_text_in(
        &repo.0,
        "explore",
        json!({ "question": "", "seeds": ["m.py::f"] }),
    );
    assert!(ok && text.contains("calls → m.py::g (×2) L5,7"), "{text}");
}

#[test]
fn call_sites_are_capped_and_stay_in_their_file() {
    let mut many = String::from("def g():\n    pass\n\ndef f():\n");
    for _ in 0..15 {
        many.push_str("    g()\n");
    }
    let repo = TempRepo::new(
        "callsites2",
        &[
            ("m.py", many),
            ("Sources/A.swift", "class Foo {}\n".into()),
            (
                "Sources/B.swift",
                "protocol P {}\nextension Foo: P {}\n".into(),
            ),
        ],
    );
    let f = symbol_in(&repo, "m.py::f");
    let call = &f["outgoing"]["calls"][0];
    assert_eq!(call["count"], 15, "{f:#}");
    assert_eq!(call["call_sites"].as_array().unwrap().len(), 10, "{f:#}");
    assert_eq!(call["more_sites"], 5, "{f:#}");
    let (ok, text) = tool_text_in(
        &repo.0,
        "explore",
        json!({ "question": "", "seeds": ["m.py::f"] }),
    );
    assert!(
        ok && text.contains("L5,6,7,8,9,10,11,12,13,14,…(+5)"),
        "{text}"
    );

    // The conformance is declared in B.swift but belongs to Foo (A.swift): no line of
    // B.swift is reported as a line of A.swift.
    let (ok, text) = tool_text_in(
        &repo.0,
        "get_neighbors",
        json!({ "id": "Sources/A.swift::Foo", "direction": "out", "edge_kinds": ["inherits"], "format": "json" }),
    );
    assert!(ok, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["edges"][0]["to"], "Sources/B.swift::P", "{v:#}");
    assert!(v["edges"][0].get("lines").is_none(), "{v:#}");

    // Incoming edges in explore name the file their lines belong to.
    let (ok, text) = tool_text_in(
        &repo.0,
        "explore",
        json!({ "question": "", "seeds": ["m.py::g"] }),
    );
    assert!(
        ok && text.contains("called_by ← m.py::f (×15) L5,") && text.contains(" in m.py"),
        "{text}"
    );
}

#[test]
fn search_says_how_results_match() {
    let repo = TempRepo::new(
        "searchmatch",
        &[(
            "m.py",
            "def validate(x):\n    \"\"\"Checks x against planContentSchema.\"\"\"\n    pass\n\n\
             def plan():\n    pass\n\ndef planner():\n    pass\n"
                .into(),
        )],
    );
    let search = |q: &str| -> Value {
        let (ok, text) = tool_text_in(
            &repo.0,
            "search_symbols",
            json!({ "query": q, "format": "json" }),
        );
        assert!(ok, "{text}");
        serde_json::from_str(&text).unwrap()
    };
    // Absent name, mentioned in a doc only.
    let v = search("planContentSchema");
    assert_eq!(
        v["note"], "no symbol named \"planContentSchema\"; showing doc/text matches",
        "{v:#}"
    );
    assert_eq!(v["results"][0]["id"], "m.py::validate");
    assert_eq!(v["results"][0]["match"], "doc");

    let v = search("plan");
    assert!(v.get("note").is_none(), "{v:#}");
    assert_eq!(v["results"][0]["match"], "exact");
    assert_eq!(v["results"][1]["match"], "prefix");

    let v = search("plann");
    assert_eq!(
        v["note"], "no symbol named \"plann\"; showing partial name matches",
        "{v:#}"
    );
    let v = search("lann");
    assert_eq!(v["results"][0]["match"], "fragment", "{v:#}");

    // Qualified ids and paths are exact when they name a node.
    let v = search("m.py::plan");
    assert!(v.get("note").is_none(), "{v:#}");
    assert_eq!(v["results"][0]["id"], "m.py::plan");
    assert_eq!(v["results"][0]["match"], "exact");
    assert_eq!(search("m.py")["results"][0]["match"], "exact");
    // A match only through the file path is labelled as such.
    let v = search("m");
    assert!(
        v["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["match"] == "id"),
        "{v:#}"
    );
    // The symbol exists, but the filters leave it out.
    let (ok, text) = tool_text_in(
        &repo.0,
        "search_symbols",
        json!({ "query": "plan", "kinds": ["class"], "format": "json" }),
    );
    assert!(
        ok && text.contains("matches the kinds/path filters"),
        "{text}"
    );
    let v = search("nothing_like_this");
    assert!(v["note"].as_str().unwrap().ends_with("no matches"), "{v:#}");
    // Several words are not a name: no note.
    assert!(search("m.py plan").get("note").is_none());
}

/// Copies a directory tree (without `.codenav`).
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == ".codenav" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            copy_tree(&path, &to.join(&name));
        } else {
            std::fs::copy(&path, to.join(&name)).unwrap();
        }
    }
}

#[test]
fn diff_impact_maps_changes_to_symbols() {
    let repo = TempRepo::new("diffimpact", &[]);
    copy_tree(fixture(), &repo.0);
    let git = |args: &[&str]| -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo.0)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let diff = |args: Value| -> Value {
        let mut args = args;
        args["format"] = json!("json");
        let (ok, text) = tool_text_in(&repo.0, "diff_impact", args);
        assert!(ok, "{text}");
        serde_json::from_str(&text).unwrap()
    };

    // Not a repository yet.
    let (ok, text) = tool_text_in(&repo.0, "diff_impact", json!({}));
    assert!(!ok && text.contains("git diff failed"), "{text}");

    git(&["init", "-q", "-b", "main"]);
    // User diff settings must not change the paths parsed.
    git(&["config", "diff.mnemonicPrefix", "true"]);
    std::fs::write(repo.0.join(".gitignore"), ".codenav/\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "base"]);

    // Change the body of format_name (line 14).
    let models = repo.0.join("py/app/models.py");
    let text = std::fs::read_to_string(&models).unwrap();
    std::fs::write(
        &models,
        text.replace("return n.title()", "return n.strip().title()"),
    )
    .unwrap();
    let v = diff(json!({}));
    assert_eq!(v["diff"], "uncommitted changes vs HEAD");
    assert_eq!(
        v["changed"]["py/app/models.py"],
        json!(["format_name (function) L13-14"]),
        "{v:#}"
    );
    let affected = v["by_file"].to_string();
    assert!(
        affected.contains("User::greet") && affected.contains("UserService::notify"),
        "{v:#}"
    );
    assert!(!affected.contains("ios/"), "{v:#}");
    assert!(v["modules"].to_string().contains("py/app"), "{v:#}");

    // Only staged changes: none yet, then the staged file.
    assert_eq!(diff(json!({ "staged": true }))["changed_files"], 0);
    git(&["add", "py/app/models.py"]);
    assert_eq!(diff(json!({ "staged": true }))["changed_files"], 1);
    git(&["commit", "-q", "-m", "strip"]);

    // A branch: a new file and a deleted one, against main.
    git(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(repo.0.join("py/app/extra.py"), "def extra():\n    pass\n").unwrap();
    std::fs::remove_file(repo.0.join("web/src/components/Avatar.tsx")).unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "feature"]);
    let v = diff(json!({ "base": "main" }));
    assert_eq!(v["diff"], "main...HEAD");
    assert_eq!(v["new_files"], json!(["py/app/extra.py"]), "{v:#}");
    assert_eq!(
        v["deleted_files"],
        json!(["web/src/components/Avatar.tsx"]),
        "{v:#}"
    );
    assert_eq!(
        v["changed"]["py/app/extra.py"],
        json!(["extra (function) L1-2"]),
        "{v:#}"
    );

    // Review cases on top of the branch.
    git(&["checkout", "-q", "main"]);
    // An untracked file counts as uncommitted.
    std::fs::write(repo.0.join("py/app/loose.py"), "def loose():\n    pass\n").unwrap();
    let v = diff(json!({}));
    assert_eq!(v["new_files"], json!(["py/app/loose.py"]), "{v:#}");
    assert_eq!(
        v["changed"]["py/app/loose.py"],
        json!(["loose (function) L1-2"]),
        "{v:#}"
    );
    std::fs::remove_file(repo.0.join("py/app/loose.py")).unwrap();
    // Removing `greet` (lines 10-11) reports it removed, and its enclosing class as changed
    // only if lines of the class itself changed: here the deletion is inside User.
    let text = std::fs::read_to_string(&models).unwrap();
    std::fs::write(
        &models,
        text.replace(
            "\n    def greet(self) -> str:\n        return format_name(self.name)\n",
            "\n",
        ),
    )
    .unwrap();
    let v = diff(json!({}));
    assert_eq!(
        v["removed_symbols"],
        json!(["py/app/models.py::User::greet (method)"]),
        "{v:#}"
    );
    assert!(
        v["note"].as_str().unwrap().contains("search its names"),
        "{v:#}"
    );
    assert!(!v["changed"].to_string().contains("format_name"), "{v:#}");
    git(&["checkout", "-q", "--", "py/app/models.py"]);
    // A rename.
    git(&[
        "mv",
        "py/app/services/users.py",
        "py/app/services/accounts.py",
    ]);
    let v = diff(json!({}));
    assert_eq!(
        v["renamed_files"],
        json!(["py/app/services/users.py → py/app/services/accounts.py"]),
        "{v:#}"
    );
    git(&["reset", "-q", "--hard"]);
    // Against a base, uncommitted edits in the same files are flagged.
    git(&["checkout", "-q", "feature"]);
    std::fs::write(
        repo.0.join("py/app/extra.py"),
        "def extra():\n    return 1\n",
    )
    .unwrap();
    let v = diff(json!({ "base": "main" }));
    assert!(
        v["warning"].as_str().unwrap().contains("py/app/extra.py"),
        "{v:#}"
    );

    for (args, expected) in [
        (json!({ "base": "nope" }), "git diff failed"),
        (json!({ "base": "--output=x" }), "must be a branch"),
        (json!({ "base": "main", "staged": true }), "not both"),
    ] {
        let (ok, text) = tool_text_in(&repo.0, "diff_impact", args);
        assert!(!ok && text.contains(expected), "{text}");
    }
}

#[test]
fn explore_seeds_use_docs_and_word_variants() {
    // English code, Italian comments.
    let repo = TempRepo::new(
        "exploreseeds",
        &[
            (
                "src/cycles.ts",
                "/** Propaga il risultato di un test ai cicli collegati. */\n\
                 export function recordInLinkedCycles(result: string) { return computeCycleStats(); }\n\
                 export function computeCycleStats() { return 0; }\n"
                    .into(),
            ),
            (
                "src/athlete.ts",
                "/** Carica l'atleta corrente con le sue schede visibili. */\n\
                 export function loadAthlete(id: string) { return visiblePlans(id); }\n\
                 export function visiblePlans(athleteId: string) {\n  // Filtra le schede (plans) visibili all'atleta.\n  return [];\n}\n"
                    .into(),
            ),
            (
                "src/oauth.ts",
                "/** PKCE: verifica il code_verifier della richiesta OAuth. */\n\
                 export function validatePkce(v: string) { return v; }\n\
                 /** Scope OAuth del coach sui propri atleti. */\n\
                 export function coachScopes() { return []; }\n"
                    .into(),
            ),
        ],
    );
    // Seeds as listed in explore's header, and the header itself.
    let seeds = |question: &str| -> (Vec<String>, String) {
        let (ok, text) = tool_text_in(
            &repo.0,
            "explore",
            json!({ "question": question, "depth": 1 }),
        );
        assert!(ok, "{question}: {text}");
        let line = text.lines().find(|l| l.starts_with("Traversal:")).unwrap();
        let list = line
            .split("seeds: ")
            .nth(1)
            .unwrap()
            .split(" | ")
            .next()
            .unwrap();
        (list.split(", ").map(str::to_string).collect(), text)
    };
    let position = |list: &[String], id: &str| list.iter().position(|s| s == id);

    // (question, expected seed, best position allowed)
    let cases = [
        (
            "how is a test result propagated to linked cycles",
            "src/cycles.ts::recordInLinkedCycles",
            0,
        ),
        (
            "come accede l'atleta alle proprie schede",
            "src/athlete.ts::loadAthlete",
            1,
        ),
        (
            "come accede l'atleta alle proprie schede",
            "src/athlete.ts::visiblePlans",
            1,
        ),
        (
            "which plans can an athlete see",
            "src/athlete.ts::visiblePlans",
            1,
        ),
    ];
    for (question, expected, max_pos) in cases {
        let (list, text) = seeds(question);
        let pos = position(&list, expected);
        assert!(
            pos.is_some_and(|p| p <= max_pos),
            "{question}: {expected} at {pos:?}\n{text}"
        );
        assert!(!text.contains("low_confidence"), "{question}\n{text}");
    }
    let (list, _) = seeds("come accede l'atleta alle proprie schede");
    assert!(
        position(&list, "src/oauth.ts::validatePkce").is_none(),
        "{list:?}"
    );

    // Mostly unknown words: the answer says it is unsure instead of pretending.
    let (_, text) = seeds("athlete quantum flux capacitor warp");
    assert!(
        text.contains("low_confidence: true") && text.contains("unmatched: quantum"),
        "{text}"
    );
}

#[test]
fn explore_seed_review_cases() {
    let repo = TempRepo::new(
        "exploreseeds2",
        &[(
            "src/a.ts",
            "export function parseConfig(text: string) { return text; }\n\
             export function load() { return parseConfig(''); }\n\
             export class Store {\n  /** Deletes a user. */\n  remove() {}\n}\n\
             export function audit() {\n  // TODO: remove legacy auth hack\n  return 1;\n}\n\
             /** Max retries */\nexport const MAX = 3, handler = () => 0;\n"
                .into(),
        )],
    );
    // A relation word does not count as an unmatched term.
    let (ok, text) = tool_text_in(
        &repo.0,
        "explore",
        json!({ "question": "what uses parseConfig" }),
    );
    assert!(ok && !text.contains("low_confidence"), "{text}");

    let doc = |id: &str| symbol_in(&repo, id)["doc"].clone();
    assert_eq!(doc("src/a.ts::Store::remove"), "Deletes a user.");
    assert!(
        doc("src/a.ts::Store").is_null(),
        "a class does not take its member's doc"
    );
    assert!(
        doc("src/a.ts::audit").is_null(),
        "a TODO is not a description"
    );
    assert!(
        doc("src/a.ts::handler").is_null(),
        "the comment belongs to the whole statement"
    );
}

/// Runs `codenav serve <args>` with `tools/call` requests (name, arguments) and returns
/// each call's text and whether it was an error.
fn serve_calls(args: &[&std::ffi::OsStr], calls: &[(&str, Value)]) -> Vec<(bool, String)> {
    let mut child = codenav()
        .arg("serve")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        for (i, (name, arguments)) in calls.iter().enumerate() {
            let r = json!({"jsonrpc":"2.0","id":i,"method":"tools/call","params":{"name":name,"arguments":arguments}});
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
    assert_eq!(lines.len(), calls.len());
    lines
        .iter()
        .map(|l| {
            let r = &l["result"];
            (
                r["isError"] == false,
                r["content"][0]["text"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn one_server_answers_about_several_projects() {
    let other = TempRepo::new(
        "multi-b",
        &[("lib/tools.py", "def sharpen():\n    pass\n".into())],
    );
    let third = TempRepo::new(
        "multi-c",
        &[("x.py", "def only_in_third():\n    pass\n".into())],
    );
    let poly = fixture().as_os_str();
    let b = other.0.as_os_str();
    let b_name = other.0.file_name().unwrap().to_str().unwrap().to_string();
    let search = |q: &str, project: Option<&str>| {
        let mut a = json!({ "query": q, "format": "json" });
        if let Some(p) = project {
            a["project"] = json!(p);
        }
        ("search_symbols", a)
    };

    let r = serve_calls(
        &[poly, b],
        &[
            ("list_projects", json!({ "format": "json" })),
            search("sharpen", Some(&b_name)),
            search("sharpen", None),
            search("ItemStore", None),
            search("sharpen", Some(other.0.to_str().unwrap())),
            search("x", Some("nope")),
            search("only_in_third", Some(third.0.to_str().unwrap())),
        ],
    );
    let projects: Value = serde_json::from_str(&r[0].1).unwrap();
    let list = projects["projects"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{projects:#}");
    assert_eq!(list[0]["name"], "poly");
    assert_eq!(list[0]["default"], true);
    assert_eq!(list[1]["name"], b_name.as_str());
    assert_eq!(list[1]["loaded"], false, "loaded on first use");
    assert!(
        r[1].0 && r[1].1.contains("lib/tools.py::sharpen"),
        "{}",
        r[1].1
    );
    let default: Value = serde_json::from_str(&r[2].1).unwrap();
    assert_eq!(
        default["total_matches"], 0,
        "the default is the first project: {default:#}"
    );
    assert!(r[3].0 && r[3].1.contains("ItemStore"), "{}", r[3].1);
    assert!(
        r[4].0 && r[4].1.contains("lib/tools.py::sharpen"),
        "by path: {}",
        r[4].1
    );
    assert!(
        !r[5].0 && r[5].1.contains("unknown project `nope`"),
        "{}",
        r[5].1
    );
    assert!(!r[6].0 && r[6].1.contains("--any-project"), "{}", r[6].1);

    // One graph in memory at a time: switching drops and reloads, answers stay right.
    let max1: &std::ffi::OsStr = "--max-loaded".as_ref();
    let one: &std::ffi::OsStr = "1".as_ref();
    let r = serve_calls(
        &[poly, b, max1, one],
        &[
            search("ItemStore", None),
            search("sharpen", Some(&b_name)),
            search("ItemStore", None),
            ("list_projects", json!({ "format": "json" })),
        ],
    );
    assert!(
        r[0].1.contains("ItemStore") && r[1].1.contains("sharpen") && r[2].1.contains("ItemStore")
    );
    let projects: Value = serde_json::from_str(&r[3].1).unwrap();
    let loaded: Vec<bool> = projects["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["loaded"].as_bool().unwrap())
        .collect();
    assert_eq!(loaded, vec![true, false], "{projects:#}");

    // Listing does not switch: after a call on b, b is the active one.
    let r = serve_calls(
        &[poly, b, max1, one],
        &[
            search("sharpen", Some(&b_name)),
            ("list_projects", json!({ "format": "json" })),
        ],
    );
    let projects: Value = serde_json::from_str(&r[1].1).unwrap();
    assert_eq!(projects["projects"][1]["active"], true, "{projects:#}");
    assert_eq!(projects["projects"][0]["loaded"], false, "{projects:#}");

    // --any-project opens other repositories by absolute path, but only repositories.
    let any: &std::ffi::OsStr = "--any-project".as_ref();
    let not_repo = TempRepo::new("multi-d", &[("y.py", "def y():\n    pass\n".into())]);
    std::fs::create_dir_all(third.0.join(".git")).unwrap();
    let r = serve_calls(
        &[poly, any],
        &[
            search("only_in_third", Some(third.0.to_str().unwrap())),
            search("y", Some(not_repo.0.to_str().unwrap())),
            search("y", Some("relative/path")),
            ("list_projects", json!({ "format": "json" })),
        ],
    );
    assert!(
        r[0].0 && r[0].1.contains("x.py::only_in_third"),
        "{}",
        r[0].1
    );
    assert!(
        !r[1].0 && r[1].1.contains("is not a repository"),
        "{}",
        r[1].1
    );
    assert!(!r[2].0 && r[2].1.contains("absolute path"), "{}", r[2].1);
    let projects: Value = serde_json::from_str(&r[3].1).unwrap();
    assert_eq!(
        projects["projects"].as_array().unwrap().len(),
        2,
        "no entry for failures: {projects:#}"
    );
    assert!(
        !not_repo.0.join(".codenav").exists(),
        "nothing indexed in a non-repository"
    );
}

/// Several occurrences of one relation: a single sure one makes the edge sure.
#[test]
fn merged_edges_keep_the_best_confidence() {
    let repo = TempRepo::new(
        "confmerge",
        &[(
            "a.py",
            "class A:\n    def helper(self):\n        pass\n\n    def run(self, other):\n        other.helper()\n        self.helper()\n"
                .into(),
        )],
    );
    let (ok, text) = tool_text_in(
        &repo.0,
        "get_symbol",
        json!({ "id": "a.py::A::run", "include_code": false, "format": "json" }),
    );
    assert!(ok, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["outgoing"]["calls"],
        json!([{ "id": "a.py::A::helper", "count": 2, "call_sites": [6, 7] }]),
        "{v:#}"
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
        (
            "get_community",
            json!({ "id": "src/graph.rs::Graph", "limit": 200 }),
        ),
        (
            "audit_edges",
            json!({ "confidence": "exact", "limit": 500 }),
        ),
        (
            "tests_for",
            json!({ "id": "src/graph.rs::Graph", "depth": 8 }),
        ),
        ("insights", json!({ "top": 50 })),
        (
            "find_cycles",
            json!({ "edge_kinds": ["imports", "calls", "references"], "min_confidence": "ambiguous" }),
        ),
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
        text.contains("  calls:\n    - id: py/app/models.py::User\n      call_sites: [7]\n"),
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
fn tool_takes_the_path_positionally_or_as_a_flag() {
    let run = |args: &[&std::ffi::OsStr]| -> (Option<i32>, String) {
        let out = codenav().arg("tool").args(args).output().unwrap();
        let text = if out.status.success() {
            &out.stdout
        } else {
            &out.stderr
        };
        (
            out.status.code(),
            String::from_utf8_lossy(text).into_owned(),
        )
    };
    let f = fixture().as_os_str();
    let name: &std::ffi::OsStr = "graph_overview".as_ref();
    let empty: &std::ffi::OsStr = "{}".as_ref();
    let flag: &std::ffi::OsStr = "--path".as_ref();
    let positional = run(&[name, empty, f]);
    assert_eq!(positional.0, Some(0), "{}", positional.1);
    assert_eq!(run(&[name, empty, flag, f]), positional);
    assert_eq!(run(&[name, flag, f]), positional);
    // Without JSON, a single extra argument is the path, as for `index PATH`.
    assert_eq!(run(&[name, f]), positional);

    // Both at once is a usage error.
    assert_eq!(run(&[name, empty, f, flag, f]).0, Some(2));

    // Bad arguments fail before indexing anything.
    let dir = TempRepo::new("toolargs", &[("a.py", "def f():\n    pass\n".into())]);
    let d = dir.0.as_os_str();
    for bad in ["[1]", "{oops"] {
        let (code, text) = run(&[name, bad.as_ref(), d]);
        assert_eq!(code, Some(1), "{text}");
        assert!(text.contains("must be a JSON object"), "{text}");
    }
    assert!(!dir.0.join(".codenav").exists(), "nothing was indexed");
}

/// Sends JSON-RPC requests to `codenav serve` on the fixture and returns the responses.
fn mcp_session(requests: &[Value]) -> Vec<Value> {
    let mut child = codenav()
        .arg("serve")
        .arg(fixture())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        for r in requests {
            writeln!(stdin, "{r}").unwrap();
        }
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn mcp_resources_and_prompts() {
    let lines = mcp_session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"resources/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"codenav://overview"}}),
        json!({"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"codenav://insights"}}),
        json!({"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"codenav://nope"}}),
        json!({"jsonrpc":"2.0","id":6,"method":"prompts/list"}),
        json!({"jsonrpc":"2.0","id":7,"method":"prompts/get","params":{"name":"architecture-review","arguments":{"focus":"py/app"}}}),
        json!({"jsonrpc":"2.0","id":8,"method":"prompts/get","params":{"name":"other"}}),
    ]);
    let caps = &lines[0]["result"]["capabilities"];
    assert!(
        caps["resources"].is_object() && caps["prompts"].is_object(),
        "{caps}"
    );

    let resources = lines[1]["result"]["resources"].as_array().unwrap();
    let uris: Vec<&str> = resources
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    for uri in [
        "codenav://overview",
        "codenav://modules",
        "codenav://insights",
        "codenav://cycles",
        "codenav://audit",
    ] {
        assert!(uris.contains(&uri), "{uris:?}");
    }

    // Every listed resource can be read; the overview carries the index state.
    let overview = &lines[2]["result"]["contents"][0];
    assert_eq!(overview["uri"], "codenav://overview");
    assert!(
        overview["text"].as_str().unwrap().contains("index:"),
        "{overview}"
    );
    let insights = lines[3]["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(insights.contains("suggested_questions"), "{insights}");
    assert_eq!(lines[4]["error"]["code"], -32002, "{}", lines[4]);

    let prompts = lines[5]["result"]["prompts"].as_array().unwrap();
    assert_eq!(prompts[0]["name"], "architecture-review");
    let text = lines[6]["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(
        text.contains("`py/app`") && text.contains("find_cycles"),
        "{text}"
    );
    assert_eq!(lines[7]["error"]["code"], -32602, "{}", lines[7]);

    // Invalid parameters are errors, not silent defaults; backticks don't break the prompt.
    let lines = mcp_session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"resources/read","params":{}}),
        json!({"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"architecture-review","arguments":{"focus":42}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"prompts/get","params":{"name":"architecture-review","arguments":{"focus":"a`b"}}}),
    ]);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["error"]["code"], -32602, "{}", lines[0]);
    assert_eq!(lines[1]["error"]["code"], -32602, "{}", lines[1]);
    let text = lines[2]["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("`ab`"), "{text}");

    // The other resources read without errors too.
    let reads: Vec<Value> = uris
        .iter()
        .enumerate()
        .map(|(i, uri)| json!({"jsonrpc":"2.0","id":i,"method":"resources/read","params":{"uri":uri}}))
        .collect();
    let responses = mcp_session(&reads);
    assert_eq!(responses.len(), uris.len());
    for r in responses {
        assert!(
            r.get("error").is_none() && r["result"]["contents"][0]["text"].is_string(),
            "{r}"
        );
    }
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
