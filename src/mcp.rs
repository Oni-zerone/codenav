//! Minimal MCP server over stdio (newline-delimited JSON-RPC 2.0), exposing the code
//! graph as tools. Logs go to stderr: stdout is reserved for protocol messages.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::explore;
use crate::graph::{EdgeKind, Graph, SymKind};
use crate::indexer;
use crate::output;
use crate::query::{self, Direction};

const PROTOCOL_VERSION: &str = "2025-06-18";
const STALE_CHECK_INTERVAL: Duration = Duration::from_secs(20);

const INSTRUCTIONS: &str = "codenav exposes a pre-built graph of this repository. \
Nodes are files, symbols (classes, structs, interfaces/traits/protocols, functions, methods, ...) \
and external packages; edges are contains, imports, calls, inherits and references. \
Start with graph_overview or module_graph for the big picture; for a question about specific code \
call explore first, which returns the relevant subgraph in one call. Use search_symbols to get node ids, \
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
                    Ok(text) => json!({
                        "content": [{ "type": "text", "text": text }],
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

    /// Runs a tool and renders its result: plain text tools as is, structured ones as
    /// compact text cut to `token_budget`, or as full JSON with `format: "json"`.
    pub fn call_tool(&mut self, name: &str, a: &Value) -> Result<String> {
        let format = opt_str(a, "format")?.unwrap_or("text").to_lowercase();
        anyhow::ensure!(
            format == "text" || format == "json",
            "format must be text or json, got `{format}`"
        );
        if format == "json" && matches!(name, "explore" | "read_code") {
            anyhow::bail!("`{name}` always returns text; format=json is not supported");
        }
        let budget = token_budget(a)?;
        Ok(match self.run_tool(name, a)? {
            Value::String(text) => text,
            v if format == "json" => serde_json::to_string(&v)?,
            v => output::fit_value(&v, budget, narrow_hint(name)),
        })
    }

    fn run_tool(&mut self, name: &str, a: &Value) -> Result<Value> {
        if name == "reindex" {
            self.last_stale_check = Instant::now();
            return self.reindex();
        }
        self.refresh_if_stale();
        let g = &self.graph;
        match name {
            "graph_overview" => Ok(query::overview(g, uint(a, "top", 15)?)),
            "module_graph" => Ok(query::module_graph(
                g,
                uint(a, "depth", 2)?.max(1),
                uint(a, "min_weight", 1)? as u32,
            )),
            "explore" => {
                let mode = match opt_str(a, "mode")?.unwrap_or("bfs").to_lowercase().as_str() {
                    "bfs" => explore::Mode::Bfs,
                    "dfs" => explore::Mode::Dfs,
                    m => return Err(anyhow!("mode must be bfs or dfs, got `{m}`")),
                };
                let direction = match opt_str(a, "direction")? {
                    Some(d) => Direction::parse(&d.to_lowercase())
                        .ok_or_else(|| anyhow!("direction must be in/out/both"))?,
                    // DFS follows dependency chains forward by default.
                    None if mode == explore::Mode::Dfs => Direction::Out,
                    None => Direction::Both,
                };
                let kinds = edge_kinds(a)?;
                let text = explore::explore(
                    g,
                    opt_str(a, "question")?.unwrap_or(""),
                    &list(a, "seeds")?,
                    &explore::Options {
                        mode,
                        depth: uint(a, "depth", 2)?.clamp(1, 6) as u32,
                        direction,
                        kinds: &kinds,
                        token_budget: token_budget(a)?,
                    },
                )?;
                Ok(Value::String(text))
            }
            "search_symbols" => {
                let kinds = list(a, "kinds")?
                    .iter()
                    .map(|k| SymKind::parse(k).ok_or_else(|| anyhow!("unknown kind `{k}`")))
                    .collect::<Result<Vec<_>>>()?;
                Ok(query::search(
                    g,
                    a.get("query").and_then(Value::as_str).unwrap_or(""),
                    &kinds,
                    a.get("path").and_then(Value::as_str),
                    uint(a, "limit", 20)?,
                ))
            }
            "get_symbol" => {
                let i = query::resolve(g, &string(a, "id")?)?;
                let code = opt_bool(a, "include_code")?.unwrap_or(true);
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
                    uint(a, "depth", 1)?.clamp(1, 5) as u32,
                    uint(a, "limit", 60)?,
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
                    opt_bool(a, "directed")?.unwrap_or(false),
                    uint(a, "max_depth", 6)?.clamp(1, 12) as u32,
                    uint(a, "max_paths", 3)?.clamp(1, 20),
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
                    uint(a, "depth", 3)?.clamp(1, 8) as u32,
                    uint(a, "limit", 200)?,
                ))
            }
            "read_code" => {
                let file = string(a, "file")?;
                let start = uint(a, "start_line", 1)? as u32;
                let end = uint(a, "end_line", start as usize + 80)? as u32;
                let path = self.root.join(&file).canonicalize()?;
                anyhow::ensure!(
                    path.starts_with(self.root.canonicalize()?),
                    "path outside repository"
                );
                let code = query::read_lines(&self.root, &file, start, end)
                    .ok_or_else(|| anyhow!("cannot read {file}"))?;
                let lines: Vec<Vec<String>> = code.lines().map(|l| vec![l.to_string()]).collect();
                Ok(Value::String(output::fit_blocks(
                    &file,
                    &lines,
                    token_budget(a)?,
                    "lines",
                    "request a smaller line range",
                )))
            }
            _ => Err(anyhow!("unknown tool `{name}`")),
        }
    }
}

fn token_budget(a: &Value) -> Result<usize> {
    Ok(uint(a, "token_budget", output::DEFAULT_TOKEN_BUDGET)?
        .clamp(output::MIN_TOKEN_BUDGET, output::MAX_TOKEN_BUDGET))
}

/// How to get a smaller answer from each tool, shown when its output is cut.
fn narrow_hint(tool: &str) -> &'static str {
    match tool {
        "get_symbol" => "pass include_code=false, or use get_neighbors with edge_kinds",
        "get_neighbors" => "narrow with direction, edge_kinds, depth or limit",
        "search_symbols" => "narrow with kinds, path or limit",
        "file_outline" => "use get_symbol on the members you need",
        "impact_analysis" => "lower depth or limit",
        "module_graph" => "lower depth or raise min_weight",
        "find_paths" => "lower max_paths or restrict edge_kinds",
        _ => "narrow the request",
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

/// An optional non-negative integer argument; a wrong type is an error, not a default.
fn uint(a: &Value, key: &str, default: usize) -> Result<usize> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|v| v as usize)
            .ok_or_else(|| anyhow!("argument `{key}` must be a non-negative integer, got {v}")),
    }
}

fn opt_bool(a: &Value, key: &str) -> Result<Option<bool>> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(v) => Err(anyhow!("argument `{key}` must be true or false, got {v}")),
    }
}

fn opt_str<'a>(a: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(v) => Err(anyhow!("argument `{key}` must be a string, got {v}")),
    }
}

/// A list argument given as a JSON array of strings or a comma-separated string.
/// Blank entries are dropped.
fn list(a: &Value, key: &str) -> Result<Vec<String>> {
    let items: Vec<String> = match a.get(key) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(v)) => v
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("argument `{key}` must contain strings, got {x}"))
            })
            .collect::<Result<_>>()?,
        Some(Value::String(s)) => s.split(',').map(str::to_string).collect(),
        Some(v) => {
            return Err(anyhow!(
                "argument `{key}` must be an array of strings, got {v}"
            ));
        }
    };
    Ok(items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

fn edge_kinds(a: &Value) -> Result<Vec<EdgeKind>> {
    list(a, "edge_kinds")?
        .iter()
        .map(|k| EdgeKind::parse(k).ok_or_else(|| anyhow!("unknown edge kind `{k}`")))
        .collect()
}

fn tool_definitions() -> Value {
    let mut tools = base_tool_definitions();
    let budget = json!({ "type": "integer", "description": "Approximate max output tokens, 200-100000, default 2000. Output beyond it is cut with a notice at the top." });
    let format = json!({ "type": "string", "enum": ["text", "json"], "description": "text (default): compact, cut to token_budget. json: the full structured result, no budget." });
    for tool in tools.as_array_mut().into_iter().flatten() {
        let name = tool["name"].as_str().unwrap_or("").to_string();
        let Some(props) = tool["inputSchema"]["properties"].as_object_mut() else {
            continue;
        };
        if name == "reindex" {
            continue;
        }
        props.entry("token_budget").or_insert(budget.clone());
        if !matches!(name.as_str(), "explore" | "read_code") {
            props.insert("format".into(), format.clone());
        }
    }
    tools
}

fn base_tool_definitions() -> Value {
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
            "name": "explore",
            "description": "Answer a question about the code in one call: picks the most relevant symbols for the question (or the given seeds), expands their neighborhood and returns the subgraph as text, cut to a token budget. Use mode=dfs to follow a dependency chain, bfs (default) for broad context.",
            "inputSchema": { "type": "object", "properties": {
                "question": { "type": "string", "description": "Natural-language question or keywords, e.g. `how does UserService create users`." },
                "seeds": { "type": "array", "items": { "type": "string" }, "description": "Explicit start nodes (ids, unique names or file paths) instead of matching the question." },
                "mode": { "type": "string", "enum": ["bfs", "dfs"], "description": "Default bfs." },
                "depth": { "type": "integer", "description": "1-6, default 2." },
                "direction": { "type": "string", "enum": ["out", "in", "both"], "description": "Default both (bfs) / out (dfs)." },
                "edge_kinds": edge_kinds.clone(),
                "token_budget": { "type": "integer", "description": "Approximate max output tokens, 200-100000, default 2000." }
            }, "required": ["question"] }
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
