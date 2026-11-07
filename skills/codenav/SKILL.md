---
name: codenav
description: Navigate a repository as a code graph (files, symbols, calls, imports, inheritance) through the codenav MCP server. Use for architecture questions, "who calls X", "how are A and B connected", the impact of a change, which tests to run after it, and dependency cycles, in Rust, Python, TypeScript/JavaScript, Go and Swift repositories. Also use when the user asks to install or set up codenav.
license: MIT
---

# codenav

codenav indexes a repository into a graph and serves it over MCP. One graph query replaces
many rounds of grep and file reads when the question is about how code relates. Answers are
compact and cut to a token budget.

Prefer it for questions about **structure**. Keep using grep and plain reads for string
literals, comments, config files and languages it does not parse.

## Setup

Skip this if the `codenav` MCP tools are already available.

1. Check for the binary: `codenav --version`.
2. If it is missing, install it with one of:
   - `cargo install codenav --locked` (needs Rust 1.88 or later);
   - a prebuilt binary from <https://github.com/Oni-zerone/codenav/releases/latest>, placed
     on the `PATH`.
3. Register the server for the repository. Ask before changing a config file the user shares
   with a team.

   **Claude Code**, for the current repository:

   ```sh
   claude mcp add codenav -- codenav serve "$(pwd)"
   ```

   **Claude Code**, shared with the team, in `.mcp.json` at the root of the project:

   ```json
   { "mcpServers": { "codenav": { "command": "codenav", "args": ["serve", "."] } } }
   ```

   **Claude Desktop**, in `claude_desktop_config.json` (use absolute paths):

   ```json
   { "mcpServers": { "codenav": { "command": "/absolute/path/to/codenav", "args": ["serve", "/path/to/repo"] } } }
   ```

   **Codex CLI**, in `~/.codex/config.toml`:

   ```toml
   [mcp_servers.codenav]
   command = "codenav"
   args = ["serve", "/path/to/repo"]
   ```

4. MCP clients load servers at startup, so tell the user to restart the session.

`serve` builds the index on first use and keeps it up to date while it runs. The index lives
in `.codenav/`, which ignores itself in Git, so there is nothing to commit.

**Without MCP**, every tool also runs from the shell, which works straight after installing:

```sh
codenav tool explore '{"question":"how are users created"}' /path/to/repo
```

## Which tool to use

| Question | Tool |
|---|---|
| What is this repository? Where do I start? | `graph_overview`, then `module_graph` |
| How does X work? (start here for specific code) | `explore` with `question` |
| Where is X defined? What is its node id? | `search_symbols` |
| What is this symbol: signature, doc, relations, source? | `get_symbol` |
| Who calls X? | `get_neighbors` with `id`, `direction: "in"`, `edge_kinds: ["calls"]` |
| What does X depend on? | `get_neighbors` with `direction: "out"`, `depth: 2` |
| How are A and B connected? | `find_paths` |
| What is in this file? | `file_outline` |
| What breaks if I change X? | `impact_analysis` |
| What does my current diff affect? | `diff_impact` (`base: "main"` for a branch, `staged: true` for the index) |
| Which tests should I run? | `tests_for` |
| Are there dependency cycles? | `find_cycles` (`level: "module"` for directories) |
| Which parts form a real component? | `get_community` |
| What deserves a closer look? | `insights` |
| Can I trust these links? | `audit_edges` |
| Show me these lines | `read_code` |

Wherever a tool takes an `id`, a unique name (`Graph::load`, `UserService`) or a file path also
works. When the name is ambiguous the tool answers with the candidate ids.

## Working well with the results

- **Start broad, then narrow.** Use `explore` before chaining `search_symbols`, `get_symbol`
  and `get_neighbors`: it often answers in one call. If it returns `low_confidence: true`, the
  question barely matched the code, so retry with names from the code, or pass `seeds`.
- **Respect the budget.** Output defaults to about 2000 tokens. When it is cut, the first line
  gives the `token_budget` needed to see everything and a hint to narrow the request. Narrow
  first, and raise `token_budget` (up to 100000) only if you need the rest. `format: "json"`
  returns the full structured result with no budget, except in `explore` and `read_code`.
- **Check the confidence.** Calls are resolved by name, without a type checker, so an edge can
  be wrong. Answers label edges that are not `exact`:
  - `inferred`: one candidate, found by proximity or unique in the repository;
  - `ambiguous`: one of several same-named candidates, possibly a false positive.

  Pass `min_confidence: "exact"` or `"inferred"` to `get_neighbors`, `find_paths`,
  `impact_analysis`, `explore` or `diff_impact` to follow only surer edges. `tests_for` and
  `find_cycles` already default to `inferred`. Before stating an `ambiguous` link as fact, check
  it with `read_code`.
- **Treat "no result" with care.** Dynamic dispatch, reflection and generated code can hide
  callers. Before you delete code on the strength of an empty answer, confirm with grep.
- **Several repositories.** With `codenav serve repo-a repo-b`, call `list_projects` and pass
  `project` to any tool. Without it, tools answer about the first repository.
- **Stale results.** The graph refreshes by itself. `graph_overview` shows the state of the
  index, and `reindex` rebuilds it from scratch if something looks wrong.
