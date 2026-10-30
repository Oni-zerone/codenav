# codenav

Tool da terminale in Rust che indicizza una repository, ne costruisce un **grafo della codebase**
e lo espone via **MCP** (Model Context Protocol) così che un LLM (Claude, ChatGPT/Codex, ecc.)
possa navigarlo per capire le relazioni tra i componenti.

## Come funziona

```
repo ──walk (.gitignore)──▶ tree-sitter parse (parallelo) ──▶ fatti per file ──▶ risoluzione nomi ──▶ grafo ──▶ .codenav/graph.json
                                                                                                       │
                                                       LLM ◀── MCP stdio (JSON-RPC) ◀── codenav serve ◀┘
```

- **Nodi**: file, simboli (`class`, `struct`, `enum`, `interface` = trait/protocol, `type`,
  `function`, `method`, `module`, `constant`, `macro`) e pacchetti esterni (`ext:requests`).
- **Archi**: `contains`, `imports`, `calls`, `inherits` (extends/implements/conformance),
  `references` (uso di un tipo). Ogni arco ha un conteggio delle occorrenze e un flag `ambiguous`.
- **Id stabili** leggibili: `src/graph.rs::Graph::load`, `py/app/models.py::User`.

Linguaggi supportati: **Rust, Python, TypeScript/TSX, JavaScript/JSX, Go, Swift**.

### Risoluzione dei riferimenti

L'analisi è sintattica (nessun type checker), quindi i riferimenti sono collegati per nome con
priorità: stesso file → file importati → stessa directory → stesso crate (Rust) → candidati
globali (al massimo 3, marcati `ambiguous`). Alcune euristiche riducono i falsi positivi:

- `Vec::new()`, `Math.max()`: qualificatore statico non presente nel repo → scartato;
- `x.foo()`: può puntare solo a metodi; nomi molto comuni (`get`, `len`, `push`…) non vengono
  risolti globalmente;
- `self.foo()` / `this.foo()`: preferisce i membri del tipo corrente;
- `store.New()` (Go), `utils.helper()` (Python): il qualificatore può essere package/modulo.

Gli import sono risolti a file del repo (path relativi, `crate::`/`super::`, moduli Python,
alias `@/` e `~/`, package Go) oppure a nodi `ext:*`.

## Installazione

```sh
cargo install --path .
```

## Uso da CLI

```sh
codenav index [PATH]              # crea/aggiorna PATH/.codenav/graph.json
codenav serve [PATH]              # server MCP su stdio
codenav tool <nome> '<json>'      # invoca un tool MCP da terminale (debug)
codenav ast <file>                # albero tree-sitter (per scrivere nuove query)
```

Esempi:

```sh
codenav tool explore '{"question":"come UserService crea gli utenti"}'
codenav tool search_symbols '{"query":"UserService"}'
codenav tool find_paths '{"from":"Server::run","to":"extract","directed":true}'
codenav tool module_graph '{"depth":2}'
```

### Escludere percorsi: `.codenavignore`

Oltre a `.gitignore` e alle cartelle escluse di default (`node_modules`, `target`, `vendor`,
`Pods`, `build`, `dist`…), codenav legge i file `.codenavignore`, che hanno la stessa sintassi di
`.gitignore`. Un file nella root vale per tutto il repo, uno in una sottocartella vale solo lì.
Serve per escludere fixture di test, codice generato o vendorizzato che non si vuole nel grafo:

```gitignore
tests/fixtures/
**/*.generated.ts
```

Le regole possono solo **restringere** l'indice: un `!pattern` crea un'eccezione alle regole di
`.codenavignore`, ma non reinclude file esclusi da `.gitignore`, cartelle nascoste o cartelle
escluse di default. Vengono letti solo i `.codenavignore` della root e delle sue sottocartelle,
quindi una cartella esclusa si può sempre indicizzare passandola direttamente come root. Su macOS e
Windows i pattern non distinguono maiuscole e minuscole, come fa git.

L'indice viene salvato in `.codenav/` (che contiene un proprio `.gitignore`, quindi non finisce
nel versionamento). `serve` riusa l'indice se è aggiornato e, mentre gira, re-indicizza da solo
quando i sorgenti cambiano (controllo ogni 20s; disattivabile con `--no-refresh`).

## Estensione VS Code

In [`vscode/`](vscode/README.md) c'è un'estensione che visualizza `.codenav/graph.json` dentro
VS Code: vista per moduli, per file e focus su un simbolo, con apertura del sorgente al clic e
aggiornamento automatico quando l'indice cambia.

## Tool MCP

| Tool | A cosa serve |
|---|---|
| `explore` | Domanda in linguaggio naturale → sottografo rilevante in una sola chiamata (BFS/DFS, seed espliciti, `token_budget`) |
| `graph_overview` | Linguaggi, conteggi, simboli/file più dipendenti, dipendenze esterne principali |
| `module_graph` | Architettura: dipendenze aggregate tra directory/componenti |
| `search_symbols` | Ricerca per nome, frammento di id o testo della doc; filtri per kind e path |
| `get_symbol` | Dettaglio di un nodo: firma, doc, relazioni in/out per tipo, codice sorgente |
| `get_neighbors` | BFS attorno a un nodo (es. chi chiama X: `direction=in`, `edge_kinds=[calls]`) |
| `find_paths` | Catene di relazioni più brevi tra due nodi ("come sono collegati A e B?") |
| `file_outline` | Simboli di un file (annidati), import, importatori, file più correlati |
| `impact_analysis` | Tutto ciò che dipende transitivamente da un simbolo/file: cosa si rompe se cambia |
| `read_code` | Legge un intervallo di righe di un file del repo |
| `reindex` | Ricostruisce il grafo |

Tutti i tool che restituiscono dati strutturati rispondono di default in **testo compatto**
(simile a YAML, senza virgolette e parentesi) tagliato a `token_budget` (default 2000, da 200 a
100000). Se l'output viene tagliato, la prima riga lo dice e indica il `token_budget` necessario
per vedere tutto, più un suggerimento per restringere la richiesta. Con `format: "json"` si ottiene
invece il risultato JSON completo, senza budget.

Dove serve un id si può passare anche un nome univoco (`Graph::load`, `UserService`) o un path:
se è ambiguo il tool risponde con la lista degli id candidati.

## Configurazione dei client

**Claude Code** (per la repo corrente):

```sh
claude mcp add codenav -- codenav serve "$(pwd)"
```

oppure, condiviso col team, un `.mcp.json` nella root del progetto:

```json
{ "mcpServers": { "codenav": { "command": "codenav", "args": ["serve", "."] } } }
```

**Claude Desktop** (`~/Library/Application Support/Claude/claude_desktop_config.json`):

```json
{ "mcpServers": { "codenav": { "command": "/Users/<tu>/.cargo/bin/codenav", "args": ["serve", "/percorso/repo"] } } }
```

**OpenAI Codex CLI** (`~/.codex/config.toml`):

```toml
[mcp_servers.codenav]
command = "codenav"
args = ["serve", "/percorso/repo"]
```

Qualsiasi altro client MCP che supporti il trasporto **stdio** funziona allo stesso modo.
ChatGPT web accetta solo server MCP remoti (HTTP): serve un trasporto HTTP (vedi roadmap) o un
bridge stdio→HTTP.

## Struttura del codice

| File | Ruolo |
|---|---|
| `src/lang.rs` | Linguaggi supportati, grammatiche e query tree-sitter (convenzione dei capture documentata in testa) |
| `src/extract.rs` | Parsing di un file → definizioni, blocchi impl/extension, chiamate, import, tipi |
| `src/indexer.rs` | Walk della repo, estrazione parallela (rayon), risoluzione cross-file, costruzione del grafo |
| `src/graph.rs` | Modello dati (nodi, archi), persistenza, indici in memoria |
| `src/query.rs` | Analisi sul grafo: ricerca, vicini, cammini, outline, impatto, grafo dei moduli |
| `src/mcp.rs` | Server MCP JSON-RPC su stdio e definizione dei tool |

Aggiungere un linguaggio: grammatica in `Cargo.toml`, variante in `Lang`, una query che segue la
convenzione dei capture in `lang.rs`, ed eventualmente la risoluzione degli import in `indexer.rs`.
`codenav ast file.ext` aiuta a scoprire i nomi dei nodi.

## Limiti noti e roadmap

- Risoluzione per nome: con nomi molto ripetuti alcune chiamate restano non risolte o `ambiguous`.
  Un'integrazione opzionale con i language server (LSP/SCIP) darebbe precisione da compilatore.
- Re-indicizzazione completa a ogni cambio (veloce: ~1.200 file in ~1.5s); possibile versione
  incrementale per file modificati.
- Trasporto MCP HTTP (streamable) per client remoti come ChatGPT.
- Ricerca semantica (embedding) sui simboli e sulla doc.
- Altri linguaggi: Java/Kotlin, C#, C/C++, Ruby, PHP.

## Test

```sh
cargo test
```

I test d'integrazione (`tests/graph.rs`) indicizzano la fixture multi-linguaggio in
`tests/fixtures/poly` e verificano archi attesi e handshake MCP.
