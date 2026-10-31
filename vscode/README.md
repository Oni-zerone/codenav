# CodeNav Graph (VS Code)

Shows the codebase graph built by [codenav](../README.md) (`.codenav/graph.cnav`) inside VS Code:
the same graph LLMs navigate over MCP.

To see it, just **open a `.cnav` file** (from the Explorer, from Quick Open or with
`code .codenav/graph.cnav`): the extension opens it as a graph. The file is binary: it is decoded
by codenav's own Rust code, compiled to WebAssembly and shipped with the extension (to see it as
JSON: `codenav export`). JSON `.cnav` files from earlier versions open too. A `.cnav` copied out
of `.codenav/` works as well: sources are looked up in the root recorded at indexing time.

## Views

| View | What it shows |
|---|---|
| **Modules** | Directories (at the chosen depth) as components, with the dependencies aggregated between them. External packages optional. |
| **Files** | Files grouped by directory (nested groups), with imports/calls/references aggregated between files. Double-click a directory to narrow the view: the outside files linked to it stay visible, dimmed. |
| **Focus** | The neighbourhood of a symbol or a file: BFS at depth N, incoming/outgoing edges, symbols nested in their file. |

Interactions:

- **click** a node: details panel (signature, doc, location, relations by kind). Click a
  relation (e.g. *called by 5*) to see only those;
- **double-click**: goes one level down (directory → file → focus, or re-centres the focus);
- **Open** / click the path: opens the source at the symbol's line;
- symbol search in the toolbar (`Cmd/Ctrl+F`), `Alt+←` to go back;
- filters by edge kind (`calls`, `imports`, `references`, `inherits`, `contains`) and layouts
  (force, tree, concentric, circle, grid).

**Colours by community**: with a recent codenav index (format v2 or later) nodes are coloured by
structural *community*, that is the groups of code that depend on each other more than on the
rest (see `get_community` in the [codenav README](../README.md)). A node's shape still shows its
kind. A directory in the Modules view takes the most common community among its files. The legend
at the bottom left lists the visible communities: click an entry to highlight its nodes, again to
go back. The *Color* selector in the toolbar switches back to colours by kind.

Dashed edges = `ambiguous` references (resolved by name among several candidates). The width grows
with the number of occurrences.

## Commands

| Command | |
|---|---|
| `CodeNav: Show Code Graph` | Opens the graph panel (also from the icon in the editor title bar). |
| `CodeNav: Reveal in Code Graph` | Focuses on the symbol under the cursor (editor context menu) or on the file (Explorer menu). |
| `CodeNav: Re-index Repository` | Runs `codenav index` on the folder. |

The graph refreshes by itself when the `.cnav` file changes (for instance when `codenav serve`
re-indexes in the background).

## Settings

| Setting | Default | |
|---|---|---|
| `codenav.executable` | `codenav` | Binary used by *Re-index*. |
| `codenav.maxNodes` | `400` | Maximum nodes per view; beyond it, the most connected are kept. |
| `codenav.moduleDepth` | `2` | Initial depth of the Modules view. |

## Development

```sh
cd vscode
npm install
npm run compile      # tsc + copies cytoscape/fcose into media/vendor
npm run build:wasm   # .cnav decoder into wasm/ (needs Rust: rustup target add wasm32-unknown-unknown)
npm test             # tests of the model, and of the decoder if it was built (node:test, no VS Code)
```

To try it: open the `vscode/` folder in VS Code and press `F5` (Extension Development Host), then
open a repository indexed with `codenav index`; the decoder must have been built with
`npm run build:wasm`. To create the package: `npx @vscode/vsce package` (it builds the decoder
too).

Layout:

- `src/graphModel.ts`: graph parsing and view construction (pure, tested);
- `src/cnavFile.ts`: reading `.cnav` files with the WebAssembly decoder (the `../wasm` crate,
  which uses codenav's graph code without the indexing part);
- `src/graphStore.ts`: loading `.cnav` files (cached per file) and the file watcher;
- `src/graphPanel.ts`: webview and message protocol, command panel and custom editor for `*.cnav`;
- `src/extension.ts`: commands;
- `media/main.js`, `media/main.css`: webview UI (Cytoscape.js + the fcose layout).
