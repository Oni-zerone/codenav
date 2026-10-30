//! Minimal MCP server over stdio (newline-delimited JSON-RPC 2.0), exposing the code
//! graph as tools. Logs go to stderr: stdout is reserved for protocol messages.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::graph::{EdgeKind, Graph, SymKind};
use crate::indexer;
use crate::query::{self, Direction};

const PROTOCOL_VERSION: &str = "2025-06-18";
const STALE_CHECK_INTERVAL: Duration = Duration::from_secs(20);

const INSTRUCTIONS: &str = "codenav exposes a pre-built graph of this repository. \
Nodes are files, symbols (classes, structs, interfaces/traits/protocols, functions, methods, ...) \
and external packages; edges are contains, imports, calls, inherits and references. \
Start with graph_overview or module_graph for the big picture, use search_symbols to get node ids, \
then get_symbol, get_neighbors, find_paths and impact_analysis to explore relations. \
Call resolution is name-based: edges flagged `ambiguous` may be false positives, verify them with read_code.";

pub struct Server {
    root: PathBuf,
    graph: Graph,
    last_stale_check: Instant,
    auto_refresh: bool,
}

impl Server {
    pub fn new(root: PathBuf, graph: Graph, auto_refresh: bool) -> Self {
        Server {
            root,
            graph,
            last_stale_check: Instant::now(),
            auto_refresh,
        }
    }

    pub fn run(mut self) -> Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout().lock();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let responses = match serde_json::from_str::<Value>(&line) {
                Ok(Value::Array(batch)) => {
                    batch.into_iter().filter_map(|m| self.handle(m)).collect()
                }
                Ok(msg) => self.handle(msg).into_iter().collect(),
                Err(e) => vec![error_response(
                    Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                )],
            };
            for r in responses {
                writeln!(stdout, "{}", serde_json::to_string(&r)?)?;
            }
            stdout.flush()?;
        }
        Ok(())
    }

    /// Returns `None` for notifications.
    fn handle(&mut self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let id = id?; // notifications (initialized, cancelled, ...) need no reply

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOL_VERSION),
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "codenav", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match self.call_tool(name, &args) {
                    Ok(v) => json!({
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }],
                        "isError": false,
                    }),
                    Err(e) => json!({
                        "content": [{ "type": "text", "text": format!("{e:#}") }],
                        "isError": true,
                    }),
                })
            }
            "resources/list" => Ok(json!({ "resources": [] })),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, m)) => error_response(id, code, &m),
        })
    }

    fn refresh_if_stale(&mut self) {
        if !self.auto_refresh || self.last_stale_check.elapsed() < STALE_CHECK_INTERVAL {
            return;
        }
        self.last_stale_check = Instant::now();
        if indexer::is_stale(&self.root, &self.graph) {
            eprintln!("codenav: sources changed, re-indexing…");
            if let Err(e) = self.reindex() {
                eprintln!("codenav: re-index failed: {e:#}");
            }
        }
    }

    fn reindex(&mut self) -> Result<Value> {
        let (graph, stats) = indexer::index(&self.root)?;
        if let Err(e) = graph.save(&self.root) {
            eprintln!("codenav: could not save index: {e:#}");
        }
        self.graph = graph;
        Ok(json!({
            "files": stats.files,
            "symbols": stats.symbols,
            "edges": stats.edges,
            "parse_errors": stats.errors,
            "elapsed_ms": stats.elapsed.as_millis(),
        }))
    }

    pub fn call_tool(&mut self, name: &str, a: &Value) -> Result<Value> {
        if name == "reindex" {
            self.last_stale_check = Instant::now();
            return self.reindex();
        }
        self.refresh_if_stale();
        let g = &self.graph;
        match name {
            "graph_overview" => Ok(query::overview(g, uint(a, "top", 15))),
            "module_graph" => Ok(query::module_graph(
                g,
                uint(a, "depth", 2).max(1),
                uint(a, "min_weight", 1) as u32,
            )),
            "search_symbols" => {
                let kinds = list(a, "kinds")
                    .iter()
                    .map(|k| SymKind::parse(k).ok_or_else(|| anyhow!("unknown kind `{k}`")))
                    .collect::<Result<Vec<_>>>()?;
                Ok(query::search(
                    g,
                    a.get("query").and_then(Value::as_str).unwrap_or(""),
                    &kinds,
                    a.get("path").and_then(Value::as_str),
                    uint(a, "limit", 20),
                ))
            }
            "get_symbol" => {
                let i = query::resolve(g, &string(a, "id")?)?;
                let code = a
                    .get("include_code")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                Ok(query::symbol_details(g, &self.root, i, code))
            }
            "get_neighbors" => {
                let i = query::resolve(g, &string(a, "id")?)?;
                let dir = a
                    .get("direction")
                    .and_then(Value::as_str)
                    .map(|d| {
                        Direction::parse(d).ok_or_else(|| anyhow!("direction must be in/out/both"))
                    })
                    .transpose()?
                    .unwrap_or(Direction::Both);
                Ok(query::neighbors(
                    g,
                    i,
                    dir,
                    &edge_kinds(a)?,
                    uint(a, "depth", 1).clamp(1, 5) as u32,
                    uint(a, "limit", 60),
                ))
            }
            "find_paths" => {
                let from = query::resolve(g, &string(a, "from")?)?;
                let to = query::resolve(g, &string(a, "to")?)?;
                Ok(query::find_paths(
                    g,
                    from,
                    to,
                    &edge_kinds(a)?,
                    a.get("directed").and_then(Value::as_bool).unwrap_or(false),
                    uint(a, "max_depth", 6).clamp(1, 12) as u32,
                    uint(a, "max_paths", 3).clamp(1, 20),
                ))
            }
            "file_outline" => {
                let i = query::resolve(g, &string(a, "path")?)?;
                let file = if g.nodes[i].kind == SymKind::File {
                    i
                } else {
                    g.file_of(i).ok_or_else(|| anyhow!("node has no file"))?
                };
                Ok(query::file_outline(g, file))
            }
            "impact_analysis" => {
                let i = query::resolve(g, &string(a, "id")?)?;
                Ok(query::impact(
                    g,
                    i,
                    uint(a, "depth", 3).clamp(1, 8) as u32,
                    uint(a, "limit", 200),
                ))
            }
            "read_code" => {
                let file = string(a, "file")?;
                let start = uint(a, "start_line", 1) as u32;
                let end = uint(a, "end_line", start as usize + 80) as u32;
                let path = self.root.join(&file).canonicalize()?;
                anyhow::ensure!(
                    path.starts_with(self.root.canonicalize()?),
                    "path outside repository"
                );
                let code = query::read_lines(&self.root, &file, start, end)
                    .ok_or_else(|| anyhow!("cannot read {file}"))?;
                Ok(json!({ "file": file, "code": code }))
            }
            _ => Err(anyhow!("unknown tool `{name}`")),
        }
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn string(a: &Value, key: &str) -> Result<String> {
    a.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing required argument `{key}`"))
}

fn uint(a: &Value, key: &str, default: usize) -> usize {
    a.get(key)
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(default)
}

fn list(a: &Value, key: &str) -> Vec<String> {
    match a.get(key) {
        Some(Value::Array(v)) => v
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => s.split(',').map(|s| s.trim().to_string()).collect(),
        _ => Vec::new(),
    }
}

fn edge_kinds(a: &Value) -> Result<Vec<EdgeKind>> {
    list(a, "edge_kinds")
        .iter()
        .map(|k| EdgeKind::parse(k).ok_or_else(|| anyhow!("unknown edge kind `{k}`")))
        .collect()
}

fn tool_definitions() -> Value {
    let edge_kinds = json!({
        "type": "array",
        "items": { "type": "string", "enum": ["contains", "imports", "calls", "inherits", "references"] },
        "description": "Edge kinds to follow (default: all)."
    });
    json!([
        {
            "name": "graph_overview",
            "description": "Summary of the indexed repository: languages, counts, the most depended-on symbols and files, top external dependencies. Good first call.",
            "inputSchema": { "type": "object", "properties": {
                "top": { "type": "integer", "description": "Entries per ranking (default 15)." }
            } }
        },
        {
            "name": "module_graph",
            "description": "Component-level architecture: dependencies (calls/imports/inherits/references) aggregated between directories, plus each component's external packages.",
            "inputSchema": { "type": "object", "properties": {
                "depth": { "type": "integer", "description": "Directory depth defining a component (default 2)." },
                "min_weight": { "type": "integer", "description": "Hide dependencies weaker than this (default 1)." }
            } }
        },
        {
            "name": "search_symbols",
            "description": "Find files, symbols or external packages by name, qualified id fragment or doc text. Returns node ids for the other tools.",
            "inputSchema": { "type": "object", "properties": {
                "query": { "type": "string", "description": "Name or fragment, e.g. `UserService`, `auth login`, `Graph::load`." },
                "kinds": { "type": "array", "items": { "type": "string", "enum": ["file", "external", "module", "class", "struct", "enum", "interface", "type", "function", "method", "constant", "macro"] } },
                "path": { "type": "string", "description": "Only nodes whose file path contains this string." },
                "limit": { "type": "integer", "description": "Default 20." }
            }, "required": ["query"] }
        },
        {
            "name": "get_symbol",
            "description": "Details of one node: signature, doc, parent, all incoming/outgoing relations grouped by kind, and its source code.",
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "string", "description": "Node id (or unique name / file path)." },
                "include_code": { "type": "boolean", "description": "Include source (default true)." }
            }, "required": ["id"] }
        },
        {
            "name": "get_neighbors",
            "description": "Breadth-first exploration around a node, e.g. callers (direction=in, edge_kinds=[calls]) or everything a class depends on (direction=out, depth=2).",
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "string" },
                "direction": { "type": "string", "enum": ["out", "in", "both"], "description": "Default both." },
                "edge_kinds": edge_kinds,
                "depth": { "type": "integer", "description": "1-5, default 1." },
                "limit": { "type": "integer", "description": "Max nodes, default 60." }
            }, "required": ["id"] }
        },
        {
            "name": "find_paths",
            "description": "Shortest relation chains between two nodes, explaining how two components are connected.",
            "inputSchema": { "type": "object", "properties": {
                "from": { "type": "string" },
                "to": { "type": "string" },
                "edge_kinds": edge_kinds,
                "directed": { "type": "boolean", "description": "Only follow edges forward (default false)." },
                "max_depth": { "type": "integer", "description": "Default 6." },
                "max_paths": { "type": "integer", "description": "Default 3." }
            }, "required": ["from", "to"] }
        },
        {
            "name": "file_outline",
            "description": "Symbols defined in a file (nested), its imports, importers, and the files it interacts with most.",
            "inputSchema": { "type": "object", "properties": {
                "path": { "type": "string", "description": "Repo-relative file path." }
            }, "required": ["path"] }
        },
        {
            "name": "impact_analysis",
            "description": "Everything that transitively depends on a symbol or file (and its members): what may break if it changes. Grouped by file.",
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "string" },
                "depth": { "type": "integer", "description": "1-8, default 3." },
                "limit": { "type": "integer", "description": "Default 200." }
            }, "required": ["id"] }
        },
        {
            "name": "read_code",
            "description": "Read a line range of a repository file.",
            "inputSchema": { "type": "object", "properties": {
                "file": { "type": "string" },
                "start_line": { "type": "integer" },
                "end_line": { "type": "integer" }
            }, "required": ["file"] }
        },
        {
            "name": "reindex",
            "description": "Rebuild the graph from the current sources (normally automatic when files change).",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}
