# codenav

A command-line tool written in Rust that indexes a repository, builds a **graph of the codebase**
and serves it over **MCP** (Model Context Protocol), so that an LLM (Claude, ChatGPT/Codex, etc.)
can navigate it to understand how the components relate to each other.

## How it works

```
repo ──walk (.gitignore)──▶ tree-sitter parse (parallel) ──▶ facts per file ──▶ name resolution ──▶ graph ──▶ .codenav/graph.cnav
                                                                                                       │
                                                       LLM ◀── MCP stdio (JSON-RPC) ◀── codenav serve ◀┘
```

- **Nodes**: files, symbols (`class`, `struct`, `enum`, `interface` = trait/protocol, `type`,
  `function`, `method`, `module`, `constant`, `macro`), external packages (`ext:requests`) and, in
  JS/TS, the runtime global APIs the code uses (`global:localStorage`, `global:fetch`,
  `document`, `window`, `navigator`, `sessionStorage`, `indexedDB`, `globalThis`, `process`), so
  that "who uses localStorage" has an answer (`get_neighbors`, `explore`).
- **Edges**: `contains`, `imports`, `calls`, `inherits` (extends/implements/conformance),
  `references` (use of a type). Every edge has an occurrence count, the lines where it occurs
  (`call_sites` for calls, shown by `get_symbol`, `get_neighbors` and `explore`) and a confidence
  (`exact`, `inferred`, `ambiguous`).
- **Stable, readable ids**: `src/graph.rs::Graph::load`, `py/app/models.py::User`.

Supported languages: **Rust, Python, TypeScript/TSX, JavaScript/JSX, Go, Swift**.

### Reference resolution

The analysis is syntactic (no type checker), so references are linked by name, in order of
priority: same file → imported files → same directory → same crate (Rust) → global candidates
(at most 3). A few heuristics reduce false positives:

- `Vec::new()`, `Math.max()`: a static qualifier that is not in the repository → dropped;
- `x.foo()`: can only target methods; very common names (`get`, `len`, `push`…) are not resolved
  globally;
- `self.foo()` / `this.foo()`: members of the current type come first;
- `store.New()` (Go), `utils.helper()` (Python): the qualifier may be a package/module;
- JS/TS respect module scope: a symbol from another file is a candidate only if it is top-level
  and exported (`export`, `export { … }`, `export default`) and, for a bare name (`f()`,
  `new C()`, `x: T`), if it is imported: from its file, from a barrel file re-exporting it
  (`export … from`), or with `import { f } from '…'` from a source the resolver cannot map to a
  file (a workspace package, a tsconfig alias; the edge stays `inferred`). Members of classes and
  namespaces are reachable through an instance or a qualifier, even when the class is not
  exported. A nested function is visible only inside the function that contains it. Files with no
  `import`, `export` or `module.exports` are scripts: their top-level definitions are global; a
  CommonJS file without `export` exposes everything to whoever imports it;
- monorepos: imports of local packages (npm workspaces with their `package.json`, Cargo crates
  with their `Cargo.toml`) are linked to the package's files (its entry point, or the file of a
  subpath such as `@acme/utils/strings`) instead of staying external packages;
- JSX: only tags starting with an uppercase letter (`<Card>`, `<ui.Card>`) are components; `<p>`,
  `<summary>` are HTML elements and do not become calls;
- JS/TS: top-level `const`s that are not functions (objects, `Set`s, Zod schemas…) are `constant`
  nodes, and the identifiers using them become `references` edges (except those shadowed by a
  parameter or a local variable with the same name, and names inside `import`/`export`).

When the way the graph is extracted or resolved changes, indexes built by earlier versions are
considered out of date and rebuilt automatically.

Every edge says how sure the link is (`confidence`):

| Confidence | When |
|---|---|
| `exact` | containment and imports; a single candidate in the same file, in an imported file, or named by a qualifier (`self.foo()`, `Store::new()`, `store.New()`) |
| `inferred` | a single candidate found by proximity (same directory or crate) or unique in the repository, or a unique method called on a value of unknown type (`x.save()`) |
| `ambiguous` | one of several same-named candidates, or a common method on a value of unknown type: it may be a false positive |

When the same relation occurs several times, one sure occurrence is enough to make it sure.
`graph_overview` reports the share of each level; `get_neighbors`, `find_paths`,
`impact_analysis` and `explore` accept `min_confidence` (`exact` or `inferred`) to follow only the
surer edges; `audit_edges` lists the edges of one level with file and line, to check them.
Answers show the confidence only when it is not `exact`.

Imports are resolved to files of the repository (relative paths, `crate::`/`super::`, Python
modules, `@/` and `~/` aliases, Go packages) or to `ext:*` nodes.

## Installation

```sh
cargo install --path .
```

## Command line

```sh
codenav index [PATH]              # create/update PATH/.codenav/graph.cnav
codenav serve [PATH]...           # MCP server on stdio (several repositories: the first is the default)
codenav tool <name> '<json>' [PATH]  # call an MCP tool from the terminal (also --path PATH)
codenav export [PATH]             # print the saved graph as JSON (PATH: a repository or a .cnav file)
codenav ast <file>                # tree-sitter syntax tree (to write new queries)
```

Examples:

```sh
codenav tool explore '{"question":"how UserService creates users"}'
codenav tool search_symbols '{"query":"UserService"}'
codenav tool find_paths '{"from":"Server::run","to":"extract","directed":true}'
codenav tool module_graph '{"depth":2}'
codenav tool graph_overview ../other-repo   # positional path, as for index and serve
codenav tool get_community '{"id":"UserService"}'
```

### Excluding paths: `.codenavignore`

Besides `.gitignore` and the directories excluded by default (`node_modules`, `target`,
`vendor`, `Pods`, `build`, `dist`…), codenav reads `.codenavignore` files, which use the same
syntax as `.gitignore`. A file at the root applies to the whole repository, one in a
subdirectory only there. Use it to leave test fixtures, generated or vendored code out of the
graph:

```gitignore
tests/fixtures/
**/*.generated.ts
```

The rules can only **narrow** the index: a `!pattern` makes an exception to `.codenavignore`
rules, but never re-includes files excluded by `.gitignore`, hidden directories or the
directories excluded by default. Only the `.codenavignore` files of the root and its
subdirectories are read, so an excluded directory can always be indexed by passing it directly as
the root. On macOS and Windows patterns are case-insensitive, as in git.

The index is saved in `.codenav/` (which holds its own `.gitignore`, so it stays out of version
control): `graph.cnav`, in a binary format that loads in a few milliseconds. To read it or process
it with other tools, `codenav export` prints it as JSON. JSON indexes from earlier versions are
read as they are and rewritten in the new format by the next save. `codenav tool` checks for
sources newer than the index and re-indexes if there are any; `--no-refresh` (or the
`CODENAV_NO_REFRESH` variable) skips the check, which helps scripts making many calls in a row.

`serve` starts right away from the saved index and, while it runs, keeps it up to date by itself
(turn it off with `--no-refresh`): a background thread checks every few seconds whether the
sources changed or HEAD moved, reads only the modified files again (the facts extracted from the
others stay in memory), recomputes the links and swaps the graph in. Queries always answer from
the latest graph without waiting for scans or re-indexing; `graph_overview` reports `reindexing`
while one is running. The graph is identical to the one a full `codenav index` builds, which
remains the way to start from scratch, like the `reindex` tool. `diff_impact` updates the graph
before answering, because it must match the current files.

## VS Code extension

[`vscode/`](vscode/README.md) holds an extension that shows `.codenav/graph.cnav` inside VS Code
(just open the file): module, file and symbol-focus views, opening the source on click and
refreshing by itself when the index changes. It reads the binary file with codenav's own Rust
code, compiled to WebAssembly (the `wasm/` crate).

## MCP tools

| Tool | What it is for |
|---|---|
| `explore` | A natural-language question → the relevant subgraph in one call (BFS/DFS, explicit seeds, `token_budget`). Starting nodes are picked from names and doc comments (word variants included); when the question barely matches the code, the answer says so (`low_confidence: true`) |
| `graph_overview` | State of the index (the Git commit and branch it was built from, uncommitted changes, warnings when HEAD moved or sources changed), languages, counts, most depended-on symbols/files, main external dependencies, largest communities |
| `module_graph` | Architecture: dependencies aggregated between directories/components |
| `insights` | What deserves a look: surprising connections between files (low confidence, few links between communities, peripheral code reaching a hub), bridge nodes between communities (approximate betweenness) and suggested questions |
| `diff_impact` | Impact of a set of Git changes (uncommitted, staged, or `base...HEAD`): changed symbols, new/deleted files, everything that depends on them, communities and modules touched |
| `tests_for` | Tests to run after a change: test files (by path convention, never by name similarity) that depend on the symbol or file, direct and indirect, with distance and path |
| `find_cycles` | Dependency cycles between files or modules: strongly connected components and shortest cycles (default: imports, edges at least `inferred`) |
| `get_community` | The structural community of a node (or by number): files, most connected members, communities it depends on and that use it |
| `search_symbols` | Search by name, id fragment or doc text; filters by kind and path. Each result says how it matched (`exact`, `prefix`, `fragment`, `id`, `doc`), and a note warns when no symbol has the name searched for |
| `get_symbol` | Details of a node: signature, doc, incoming/outgoing relations by kind, source code |
| `get_neighbors` | BFS around a node (e.g. who calls X: `direction=in`, `edge_kinds=[calls]`) |
| `find_paths` | Shortest chains of relations between two nodes ("how are A and B connected?") |
| `file_outline` | Symbols of a file (nested), imports, importers, most related files |
| `audit_edges` | Checking the resolver: edges by kind and confidence, then those of one level (default `ambiguous`) with file and line |
| `impact_analysis` | Everything that transitively depends on a symbol/file: what breaks if it changes. For a method it also counts callers of the methods it overrides (interfaces, base classes). When the text exceeds the budget it starts with a summary (counts per directory, test files, every direct dependent) and only the detail of indirect dependents is cut |
| `read_code` | Reads a range of lines of a file of the repository |
| `list_projects` | The repositories served (the name to pass as `project` to any tool), which one is the default and which are loaded |
| `reindex` | Rebuilds the graph from scratch (reads every file again) |

Every tool returning structured data answers by default in **compact text** (YAML-like, without
quotes or braces) cut to `token_budget` (default 2000, from 200 to 100000). When the output is
cut, the first line says so and gives the `token_budget` needed to see everything, plus a hint to
narrow the request. With `format: "json"` you get the full JSON result instead, with no budget.

### Synonyms in `explore` (optional)

`codenav serve --embeddings words.vec` (or `codenav tool … --embeddings words.vec`) loads a table
of static word embeddings in the word2vec, fastText and GloVe text format (`token v1 … vN`, with
or without a `count dimensions` header line). `explore` then also uses synonyms: a word of the
question matches a word of a name or of a doc comment with a similar meaning (cosine similarity
≥ 0.6), so it finds the code even when the question does not use its terms (other words, or
another language if the model is multilingual). Without the file everything works as before.
Small models are enough (the vocabulary of the code's and comments' language); the file must be
downloaded separately, codenav does not ship one.

### Several repositories

`codenav serve repo-a repo-b …` serves several repositories from one server: every tool accepts
`project` (the name shown by `list_projects`, or the path), and without `project` it answers
about the first. Graphs load on first use; at most `--max-loaded` (default 4) stay in memory, the
least recently used are dropped and reloaded from their index when needed. Each loaded project
is kept up to date only while it is the active one. `list_projects` also lists the packages each
project declares, and `get_symbol` on an external package says which served project provides it
(`provided_by_project`). With `--any-project` tools can also open other repositories by absolute
path (only directories with `.git` or a codenav index). `--fresh` and `--hub-percentile` apply to
every repository.

### MCP resources and prompts

Besides tools, the server exposes read-only **resources**, useful as initial context (compact
text, 8000-token budget): `codenav://overview`, `codenav://modules`, `codenav://insights`,
`codenav://cycles` and `codenav://audit`. The **`architecture-review`** prompt (optional
argument `focus`: a directory, a file or a symbol) guides the LLM through an architecture review
with codenav's tools: components, cycles, hot spots and links to verify.

### Communities

While indexing, the graph is split into **communities**: groups of files and symbols that depend
on each other more than on the rest of the repository (the Louvain algorithm on `calls`,
`references`, `inherits`, `imports` and `contains` edges). They show the actual components even
when the directory layout does not. Each community has a number (0 is the largest) and an
automatic label: the dominant directory, or the dominant file or symbol when the directory is
shared. The computation is deterministic, so the same code always gives the same numbers.

The dependency edges of *hubs* (the most connected nodes, above the 99th percentile) do not
count, so utilities used everywhere do not glue unrelated parts together. Change the threshold
with `codenav index --hub-percentile N` (or `serve --hub-percentile N`); `100` turns the exclusion
off. The value is saved in the index and reused by later re-indexing (including `serve`'s
automatic one) until another is passed. External packages have no community, and `ambiguous`
edges weigh half; `get_community` counts them separately (`ambiguous_edges`).

Wherever an id is expected you can also pass a unique name (`Graph::load`, `UserService`) or a
path: when it is ambiguous the tool answers with the list of candidate ids.

## Client configuration

**Claude Code** (for the current repository):

```sh
claude mcp add codenav -- codenav serve "$(pwd)"
```

or, shared with the team, a `.mcp.json` at the root of the project:

```json
{ "mcpServers": { "codenav": { "command": "codenav", "args": ["serve", "."] } } }
```

**Claude Desktop** (`~/Library/Application Support/Claude/claude_desktop_config.json`):

```json
{ "mcpServers": { "codenav": { "command": "/Users/<you>/.cargo/bin/codenav", "args": ["serve", "/path/to/repo"] } } }
```

**OpenAI Codex CLI** (`~/.codex/config.toml`):

```toml
[mcp_servers.codenav]
command = "codenav"
args = ["serve", "/path/to/repo"]
```

Any other MCP client supporting the **stdio** transport works the same way. ChatGPT on the web
accepts only remote (HTTP) MCP servers: that needs an HTTP transport (see the roadmap) or a
stdio→HTTP bridge.

## Code layout

| File | Role |
|---|---|
| `src/lang.rs` | Supported languages, tree-sitter grammars and queries (the capture convention is documented at the top) |
| `src/extract.rs` | Parsing a file → definitions, impl/extension blocks, calls, imports, types |
| `src/indexer.rs` | Repository walk, parallel extraction (rayon), cross-file resolution, graph construction, facts cache for incremental updates |
| `src/graph.rs` | Data model (nodes, edges), the `graph.cnav` format, in-memory indexes |
| `src/query.rs` | Graph analyses: search, neighbours, paths, outline, impact, module graph |
| `src/live.rs` | The served graph kept up to date in the background |
| `src/mcp.rs` | MCP JSON-RPC server on stdio and tool definitions |
| `src/lib.rs` | The library: the graph format always, everything else with the default `indexer` feature |
| `wasm/` | `graph.cnav` decoder in WebAssembly, built from the same code (for the VS Code extension) |

Adding a language: the grammar in `Cargo.toml`, a variant in `Lang`, a query following the
capture convention in `lang.rs`, and possibly import resolution in `indexer.rs`.
`codenav ast file.ext` helps to find the node names.

## Known limits and roadmap

- Resolution by name: with very common names some calls stay unresolved or `ambiguous`. An
  optional integration with language servers (LSP/SCIP) would give compiler precision.
- An update re-parses only the modified files, but recomputes the links of the whole repository
  (tens of milliseconds on a few hundred files, in the background); resolving only what a change
  affects would make it faster on large repositories.
- HTTP (streamable) MCP transport for remote clients such as ChatGPT.
- Semantic search over symbols and docs beyond word synonyms.
- More languages: Java/Kotlin, C#, C/C++, Ruby, PHP.

## Tests

```sh
cargo test --workspace
```

The integration tests (`tests/graph.rs`) index the multi-language fixture in
`tests/fixtures/poly` and check the expected edges and the MCP handshake. The extension's tests
run with `npm test` in `vscode/` (see its README).
