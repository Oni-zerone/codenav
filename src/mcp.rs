//! Minimal MCP server over stdio (newline-delimited JSON-RPC 2.0), exposing the code
//! graph as tools. Logs go to stderr: stdout is reserved for protocol messages.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::cycles;
use crate::explore;
use crate::graph::{Confidence, EdgeKind, Graph, SymKind};
use crate::indexer::{self, IndexOptions};
use crate::insights;
use crate::output;
use crate::query::{self, Direction, EdgeFilter};
use crate::tests_for;
use crate::vcs;

const PROTOCOL_VERSION: &str = "2025-06-18";
const STALE_CHECK_INTERVAL: Duration = Duration::from_secs(20);

const INSTRUCTIONS: &str = "codenav exposes a pre-built graph of this repository. \
Nodes are files, symbols (classes, structs, interfaces/traits/protocols, functions, methods, ...) \
and external packages; edges are contains, imports, calls, inherits and references. \
Start with graph_overview or module_graph for the big picture (get_community for structural clusters); for a question about specific code \
call explore first, which returns the relevant subgraph in one call. Use search_symbols to get node ids, \
then get_symbol, get_neighbors, find_paths and impact_analysis to explore relations. \
Call resolution is name-based, so each edge has a confidence: exact (same file, resolved import, \
self/type qualifier), inferred (found by proximity or unique in the repository) or ambiguous (one of \
several same-named candidates: may be a false positive, verify with read_code). Only non-exact edges \
are labelled. Pass min_confidence to get_neighbors, find_paths, impact_analysis or explore to follow \
only surer edges; audit_edges lists the guesses.";

pub struct Server {
    root: PathBuf,
    graph: Graph,
    last_stale_check: Instant,
    auto_refresh: bool,
    index_options: IndexOptions,
    /// insights for (graph build time in ms, top): costly, changes only on reindex.
    insights_cache: Option<((u64, usize), Value)>,
}

impl Server {
    pub fn new(
        root: PathBuf,
        graph: Graph,
        auto_refresh: bool,
        index_options: IndexOptions,
    ) -> Self {
        Server {
            root,
            graph,
            last_stale_check: Instant::now(),
            auto_refresh,
            index_options,
            insights_cache: None,
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
                "capabilities": {
                    "tools": { "listChanged": false },
                    "resources": { "listChanged": false, "subscribe": false },
                    "prompts": { "listChanged": false },
                },
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
            "resources/list" => Ok(json!({ "resources": resource_list() })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "resources/read" => match params.get("uri").and_then(Value::as_str) {
                None => Err((-32602, "resources/read needs a `uri` string".to_string())),
                Some(uri) => match RESOURCES.iter().find(|r| r.uri == uri) {
                    None => Err((-32002, format!("resource not found: {uri}"))),
                    Some(r) => {
                        // The tool's defaults, with a larger budget; a reader cannot narrow
                        // a resource, so a cut points to the tool instead.
                        let args = json!({ "token_budget": RESOURCE_TOKEN_BUDGET });
                        let hint = format!("call the {} tool with narrower arguments", r.tool);
                        match self.render_tool(r.tool, &args, &hint) {
                            Ok(text) => Ok(json!({ "contents": [{
                                "uri": r.uri,
                                "mimeType": "text/plain",
                                "text": text,
                            }] })),
                            Err(e) => Err((-32603, format!("{e:#}"))),
                        }
                    }
                },
            },
            "prompts/list" => Ok(json!({ "prompts": [architecture_review_definition()] })),
            "prompts/get" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                if name != "architecture-review" {
                    Err((-32602, format!("unknown prompt: {name}")))
                } else {
                    match params.get("arguments").and_then(|a| a.get("focus")) {
                        None | Some(Value::Null) => Ok(architecture_review(None)),
                        Some(Value::String(f)) => {
                            // Backticks would break the inline code it is quoted in.
                            let f = f.replace('`', "").trim().to_string();
                            Ok(architecture_review((!f.is_empty()).then_some(f.as_str())))
                        }
                        Some(v) => Err((-32602, format!("`focus` must be a string, got {v}"))),
                    }
                }
            }
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
        } else if vcs::refresh_state(&mut self.graph, &self.root)
            && let Err(e) = self.graph.save(&self.root)
        {
            eprintln!("codenav: could not save index: {e:#}");
        }
    }

    fn reindex(&mut self) -> Result<Value> {
        let (graph, stats) = indexer::index_with(&self.root, &self.index_options)?;
        if let Err(e) = graph.save(&self.root) {
            eprintln!("codenav: could not save index: {e:#}");
        }
        self.graph = graph;
        Ok(json!({
            "files": stats.files,
            "symbols": stats.symbols,
            "edges": stats.edges,
            "communities": stats.communities,
            "parse_errors": stats.errors,
            "elapsed_ms": stats.elapsed.as_millis(),
        }))
    }

    /// Runs a tool and renders its result: plain text tools as is, structured ones as
    /// compact text cut to `token_budget`, or as full JSON with `format: "json"`.
    pub fn call_tool(&mut self, name: &str, a: &Value) -> Result<String> {
        self.render_tool(name, a, narrow_hint(name))
    }

    /// A tool's answer as text (or JSON); `hint` says how to get a smaller one when cut.
    fn render_tool(&mut self, name: &str, a: &Value, hint: &str) -> Result<String> {
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
            v => output::fit_value(&v, budget, hint),
        })
    }

    fn run_tool(&mut self, name: &str, a: &Value) -> Result<Value> {
        if name == "reindex" {
            self.last_stale_check = Instant::now();
            return self.reindex();
        }
        self.refresh_if_stale();
        if name == "insights" {
            let key = (self.graph.indexed_at_ms, uint(a, "top", 10)?.clamp(1, 50));
            if let Some((k, v)) = &self.insights_cache
                && *k == key
            {
                return Ok(v.clone());
            }
            let v = insights::insights(&self.graph, key.1);
            self.insights_cache = Some((key, v.clone()));
            return Ok(v);
        }
        let g = &self.graph;
        match name {
            "graph_overview" => {
                let mut v = query::overview(g, uint(a, "top", 15)?);
                // With auto-refresh, stale sources are re-indexed on their own.
                v["index"] = vcs::index_state(g, &self.root, !self.auto_refresh);
                Ok(v)
            }
            "get_community" => {
                let (c, from) = match a.get("community").filter(|v| !v.is_null()) {
                    Some(_) => (uint(a, "community", 0)?, None),
                    None => {
                        let i = query::resolve(g, &string(a, "id").map_err(|_| {
                            anyhow!("pass `id` (a node in the community) or `community` (its number)")
                        })?)?;
                        let c = g.nodes[i].community.ok_or_else(|| {
                            anyhow!(
                                "`{}` has no community (external packages are not clustered)",
                                g.nodes[i].id
                            )
                        })?;
                        (c as usize, Some(i))
                    }
                };
                anyhow::ensure!(
                    c < g.communities.len(),
                    "no community {c}: there are {} (0-{})",
                    g.communities.len(),
                    g.communities.len().saturating_sub(1)
                );
                Ok(query::community(
                    g,
                    c as u32,
                    from,
                    uint(a, "limit", 30)?.max(1),
                ))
            }
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
                        min_confidence: min_confidence(a)?,
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
                    EdgeFilter {
                        kinds: &edge_kinds(a)?,
                        min: min_confidence(a)?,
                    },
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
                    EdgeFilter {
                        kinds: &edge_kinds(a)?,
                        min: min_confidence(a)?,
                    },
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
                    min_confidence(a)?,
                ))
            }
            "tests_for" => {
                let i = query::resolve(g, &string(a, "id")?)?;
                // Guesses among same-named candidates would list unrelated tests.
                let min = match opt_str(a, "min_confidence")? {
                    Some(c) => parse_confidence(c)?,
                    None => Confidence::Inferred,
                };
                Ok(tests_for::tests_for(
                    g,
                    i,
                    uint(a, "depth", 4)?.clamp(1, 8) as u32,
                    uint(a, "limit", 50)?.max(1),
                    min,
                ))
            }
            "find_cycles" => {
                let level = match opt_str(a, "level")?.unwrap_or("file") {
                    "file" => cycles::Level::File,
                    "module" => cycles::Level::Module(uint(a, "depth", 2)?.max(1)),
                    l => return Err(anyhow!("level must be file or module, got `{l}`")),
                };
                let kinds = match edge_kinds(a)? {
                    k if k.is_empty() => vec![EdgeKind::Imports],
                    k if k.contains(&EdgeKind::Contains) => {
                        return Err(anyhow!(
                            "find_cycles follows dependencies: contains is not one"
                        ));
                    }
                    k => k,
                };
                let max_length = uint(a, "max_length", 6)?;
                anyhow::ensure!(
                    (2..=20).contains(&max_length),
                    "max_length must be between 2 and 20"
                );
                let limit = uint(a, "limit", 20)?;
                anyhow::ensure!(limit >= 1, "limit must be at least 1");
                let min = match opt_str(a, "min_confidence")? {
                    Some(c) => parse_confidence(c)?,
                    None => Confidence::Inferred,
                };
                Ok(cycles::find_cycles(
                    g,
                    level,
                    EdgeFilter { kinds: &kinds, min },
                    max_length,
                    limit,
                ))
            }
            "audit_edges" => {
                let level = match opt_str(a, "confidence")? {
                    Some(c) => parse_confidence(c)?,
                    None => Confidence::Ambiguous,
                };
                Ok(query::audit_edges(
                    g,
                    level,
                    opt_str(a, "path")?,
                    &edge_kinds(a)?,
                    uint(a, "limit", 100)?,
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
        "get_community" => "lower limit",
        "audit_edges" => "narrow with path or edge_kinds, or lower limit",
        "find_cycles" => "lower limit or max_length",
        "tests_for" => "lower depth or limit, or raise min_confidence",
        "insights" => "lower top",
        "find_paths" => "lower max_paths or restrict edge_kinds",
        _ => "narrow the request",
    }
}

/// A resource: a fixed read-only view, one tool's answer with its default arguments.
struct Resource {
    uri: &'static str,
    name: &'static str,
    description: &'static str,
    tool: &'static str,
}

/// Resources are larger than a default tool answer: they are read once, as context.
const RESOURCE_TOKEN_BUDGET: usize = 8000;

const RESOURCES: &[Resource] = &[
    Resource {
        uri: "codenav://overview",
        name: "Repository overview",
        description: "Index state, languages, counts, most depended-on symbols and files, external dependencies, largest communities (graph_overview).",
        tool: "graph_overview",
    },
    Resource {
        uri: "codenav://modules",
        name: "Module graph",
        description: "Directories as components and the dependencies between them (module_graph, depth 2).",
        tool: "module_graph",
    },
    Resource {
        uri: "codenav://insights",
        name: "Insights",
        description: "Surprising connections, bridge nodes between communities and suggested questions (insights).",
        tool: "insights",
    },
    Resource {
        uri: "codenav://cycles",
        name: "Import cycles",
        description: "Import cycles between files (find_cycles).",
        tool: "find_cycles",
    },
    Resource {
        uri: "codenav://audit",
        name: "Resolver audit",
        description: "Edge counts by confidence and the edges guessed by name, to verify (audit_edges).",
        tool: "audit_edges",
    },
];

fn resource_list() -> Value {
    json!(
        RESOURCES
            .iter()
            .map(|r| json!({
                "uri": r.uri,
                "name": r.name,
                "description": r.description,
                "mimeType": "text/plain",
            }))
            .collect::<Vec<_>>()
    )
}

fn architecture_review_definition() -> Value {
    json!({
        "name": "architecture-review",
        "description": "Review the architecture of this repository (or of one area) with codenav's tools: components, dependencies, cycles, hotspots, risky guesses.",
        "arguments": [{
            "name": "focus",
            "description": "Optional area to concentrate on: a directory, file or symbol.",
            "required": false,
        }],
    })
}

fn architecture_review(focus: Option<&str>) -> Value {
    let scope = match focus {
        Some(f) => format!(
            "Concentrate on `{f}`: locate it with search_symbols or file_outline, and read the rest \
             of the repository only as far as it explains `{f}`.\n\n"
        ),
        None => String::new(),
    };
    let text = format!(
        "Review the architecture of this repository using the codenav tools.\n\n{scope}\
         1. Call graph_overview (also offered as the codenav://overview resource): languages, size, central symbols, \
         largest communities, and whether the index matches the current commit.\n\
         2. Call module_graph, then get_community on the largest communities: what the real \
         components are, and where they differ from the directory layout.\n\
         3. Call find_cycles (level file, then module) and say which cycles look accidental.\n\
         4. Call insights: explain the bridge nodes and the surprising connections. Verify every \
         edge marked ambiguous or inferred with read_code before relying on it.\n\
         5. For the most central symbols, call impact_analysis and tests_for to judge how risky \
         and how covered a change there would be.\n\n\
         Report: the components and their responsibilities, dependency problems (cycles, \
         unexpected couplings, overloaded hubs), and concrete suggestions, each citing the files \
         and symbols involved."
    );
    let description = match focus {
        Some(f) => format!("Architecture review of {f}"),
        None => "Architecture review".to_string(),
    };
    json!({
        "description": description,
        "messages": [{ "role": "user", "content": { "type": "text", "text": text } }],
    })
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

fn parse_confidence(s: &str) -> Result<Confidence> {
    Confidence::parse(&s.to_lowercase())
        .ok_or_else(|| anyhow!("confidence must be exact, inferred or ambiguous, got `{s}`"))
}

/// `min_confidence`: follow only edges at least this sure (default: all).
fn min_confidence(a: &Value) -> Result<Confidence> {
    opt_str(a, "min_confidence")?
        .map(parse_confidence)
        .transpose()
        .map(|c| c.unwrap_or(Confidence::Ambiguous))
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
    let min_confidence = json!({ "type": "string", "enum": ["exact", "inferred", "ambiguous"], "description": "Follow only edges at least this sure: exact, inferred (exact + inferred) or ambiguous (all, default)." });
    for tool in tools.as_array_mut().into_iter().flatten() {
        let name = tool["name"].as_str().unwrap_or("").to_string();
        let Some(props) = tool["inputSchema"]["properties"].as_object_mut() else {
            continue;
        };
        if name == "reindex" {
            continue;
        }
        props.entry("token_budget").or_insert(budget.clone());
        if matches!(
            name.as_str(),
            "get_neighbors" | "find_paths" | "impact_analysis" | "explore"
        ) {
            props.insert("min_confidence".into(), min_confidence.clone());
        }
        if !matches!(name.as_str(), "explore" | "read_code") {
            props.insert("format".into(), format.clone());
        }
    }
    tools
}

fn tests_for_description() -> String {
    format!(
        "Tests to run after changing a symbol or file: tests that depend on it (or on its \
         members, or on the interface methods they implement), split into direct (the test \
         calls, references or imports it) and indirect (through other code, with distance and \
         the shortest path), closest first. Tests are recognised by path only: {}. No matching \
         by name similarity; edges at least inferred by default.",
        tests_for::CONVENTIONS
    )
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
            "name": "get_community",
            "description": "Structural community (cluster of files and symbols that depend on each other more than on the rest, found by Louvain at index time) of a node, or by number: label, size, files, most connected members, and the communities it depends on / is used by. Shows real components when the directory layout doesn't.",
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "string", "description": "A node in the community (id, unique name or file path)." },
                "community": { "type": "integer", "description": "Community number instead of id (0 is the largest; see graph_overview)." },
                "limit": { "type": "integer", "description": "Members listed, most connected first (default 30)." }
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
            "description": "Find files, symbols or external packages by name, qualified id fragment or doc text. Returns node ids for the other tools; each result says how it matched (exact, prefix, fragment, doc), and a note warns when no symbol has the name searched.",
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
            "name": "insights",
            "description": "What deserves a closer look: surprising cross-file connections (low confidence, crossing communities with few links, peripheral code reaching a hub), bridge nodes joining communities (approximate betweenness), and suggested questions to investigate. Deterministic.",
            "inputSchema": { "type": "object", "properties": {
                "top": { "type": "integer", "description": "Entries per section (1-50, default 10)." }
            } }
        },
        {
            "name": "tests_for",
            "description": tests_for_description(),
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "string", "description": "Symbol or file (id, unique name or path)." },
                "depth": { "type": "integer", "description": "Max dependency hops from a test to the target (1-8, default 4)." },
                "min_confidence": { "type": "string", "enum": ["exact", "inferred", "ambiguous"], "description": "Least confidence of the edges followed (default inferred; ambiguous adds tests reached through name guesses)." },
                "limit": { "type": "integer", "description": "Test files listed (default 50)." }
            }, "required": ["id"] }
        },
        {
            "name": "find_cycles",
            "description": "Dependency cycles between files or modules (directories, as in module_graph): strongly connected components, largest first, each with its members and short cycles (for each member searched, one shortest cycle through it: `a --imports--> b --imports--> a`). By default import cycles only, over edges at least inferred.",
            "inputSchema": { "type": "object", "properties": {
                "level": { "type": "string", "enum": ["file", "module"], "description": "Units to find cycles between (default file)." },
                "depth": { "type": "integer", "description": "module level: directory depth of a module (default 2)." },
                "edge_kinds": { "type": "array", "items": { "type": "string", "enum": ["imports", "calls", "inherits", "references"] }, "description": "Dependencies to follow (default imports)." },
                "min_confidence": { "type": "string", "enum": ["exact", "inferred", "ambiguous"], "description": "Least confidence of the edges followed (default inferred: guesses would add spurious cycles)." },
                "max_length": { "type": "integer", "description": "Longest cycle reported, in units (2-20, default 6)." },
                "limit": { "type": "integer", "description": "Components listed (default 20)." }
            } }
        },
        {
            "name": "audit_edges",
            "description": "Check the name-based resolver: edge counts per kind and confidence (exact / inferred / ambiguous), then the edges at one confidence level grouped by source file with their line, to verify with read_code.",
            "inputSchema": { "type": "object", "properties": {
                "confidence": { "type": "string", "enum": ["exact", "inferred", "ambiguous"], "description": "Level to list (default ambiguous)." },
                "path": { "type": "string", "description": "Only edges whose source file starts with this path." },
                "edge_kinds": edge_kinds.clone(),
                "limit": { "type": "integer", "description": "Edges listed, default 100." }
            } }
        },
        {
            "name": "impact_analysis",
            "description": "Everything that transitively depends on a symbol or file (and its members): what may break if it changes. Grouped by file. Methods also count callers of the interface/base-class methods they override (listed in overridden_members).",
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
