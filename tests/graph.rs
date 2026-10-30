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
    assert!(lines[1]["result"]["tools"].as_array().unwrap().len() >= 9);
    let text = lines[2]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("ios/Sources/App/Model.swift::ItemStore"),
        "{text}"
    );
}
