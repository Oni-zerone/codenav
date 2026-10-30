# codenav

Tool da terminale in Rust che indicizza una repository, ne costruisce un **grafo della codebase**
e lo espone via **MCP** (Model Context Protocol) così che un LLM (Claude, ChatGPT/Codex, ecc.)
possa navigarlo per capire le relazioni tra i componenti.

## Come funziona

```
repo ──walk (.gitignore)──▶ tree-sitter parse (parallelo) ──▶ fatti per file ──▶ risoluzione nomi ──▶ grafo ──▶ .codenav/graph.cnav
                                                                                                       │
                                                       LLM ◀── MCP stdio (JSON-RPC) ◀── codenav serve ◀┘
```

- **Nodi**: file, simboli (`class`, `struct`, `enum`, `interface` = trait/protocol, `type`,
  `function`, `method`, `module`, `constant`, `macro`) e pacchetti esterni (`ext:requests`).
- **Archi**: `contains`, `imports`, `calls`, `inherits` (extends/implements/conformance),
  `references` (uso di un tipo). Ogni arco ha un conteggio delle occorrenze, le righe in cui
  compare (per le chiamate `call_sites`, mostrate da `get_symbol`, `get_neighbors` ed `explore`)
  e una confidenza (`exact`, `inferred`, `ambiguous`).
- **Id stabili** leggibili: `src/graph.rs::Graph::load`, `py/app/models.py::User`.

Linguaggi supportati: **Rust, Python, TypeScript/TSX, JavaScript/JSX, Go, Swift**.

### Risoluzione dei riferimenti

L'analisi è sintattica (nessun type checker), quindi i riferimenti sono collegati per nome con
priorità: stesso file → file importati → stessa directory → stesso crate (Rust) → candidati
globali (al massimo 3). Alcune euristiche riducono i falsi positivi:

- `Vec::new()`, `Math.max()`: qualificatore statico non presente nel repo → scartato;
- `x.foo()`: può puntare solo a metodi; nomi molto comuni (`get`, `len`, `push`…) non vengono
  risolti globalmente;
- `self.foo()` / `this.foo()`: preferisce i membri del tipo corrente;
- `store.New()` (Go), `utils.helper()` (Python): il qualificatore può essere package/modulo;
- JS/TS rispettano lo scope dei moduli: un simbolo di un altro file è un candidato solo se è di
  primo livello ed esportato (`export`, `export { … }`, `export default`) e, per un nome usato da
  solo (`f()`, `new C()`, `x: T`), se è importato: dal suo file, da un file barile che lo
  riesporta (`export … from`), oppure con `import { f } from '…'` da una sorgente che il resolver
  non sa mappare su un file (package del workspace, alias di tsconfig; l'arco resta `inferred`).
  I membri di classi e namespace sono raggiungibili da un'istanza o da un qualificatore, anche se
  la classe non è esportata. Una funzione annidata è visibile solo dentro la funzione che la
  contiene. I file senza `import`, `export` né `module.exports` sono script: le loro definizioni
  di primo livello sono globali; un file CommonJS senza `export` espone tutto a chi lo importa;
- JSX: solo i tag con iniziale maiuscola (`<Card>`, `<ui.Card>`) sono componenti; `<p>`,
  `<summary>` sono elementi HTML e non diventano chiamate;
- JS/TS: le `const` di primo livello che non sono funzioni (oggetti, `Set`, schemi Zod…) sono nodi
  `constant`, e gli identificatori che le usano diventano archi `references` (non quelli coperti da
  un parametro o da una variabile locale con lo stesso nome, né i nomi dentro `import`/`export`).

Quando cambia il modo in cui il grafo viene estratto o risolto, gli indici costruiti da versioni
precedenti vengono considerati da rifare e ricostruiti automaticamente.

Ogni arco dice quanto è sicuro il collegamento (`confidence`):

| Confidenza | Quando |
|---|---|
| `exact` | contenimento e import; candidato unico nello stesso file, in un file importato, o indicato da un qualificatore (`self.foo()`, `Store::new()`, `store.New()`) |
| `inferred` | candidato unico trovato per vicinanza (stessa directory o crate) o unico nel repo, oppure metodo unico chiamato su un valore di tipo sconosciuto (`x.save()`) |
| `ambiguous` | uno tra più candidati con lo stesso nome, o un metodo comune su un valore di tipo sconosciuto: può essere un falso positivo |

Se la stessa relazione compare più volte, basta un'occorrenza sicura per renderla sicura.
`graph_overview` riporta la percentuale di ogni livello; `get_neighbors`, `find_paths`,
`impact_analysis` ed `explore` accettano `min_confidence` (`exact` o `inferred`) per seguire solo
gli archi più sicuri; `audit_edges` elenca gli archi di un livello con file e riga, per verificarli.
Nelle risposte la confidenza compare solo quando non è `exact`.

Gli import sono risolti a file del repo (path relativi, `crate::`/`super::`, moduli Python,
alias `@/` e `~/`, package Go) oppure a nodi `ext:*`.

## Installazione

```sh
cargo install --path .
```

## Uso da CLI

```sh
codenav index [PATH]              # crea/aggiorna PATH/.codenav/graph.cnav
codenav serve [PATH]...           # server MCP su stdio (più repository: il primo è il default)
codenav tool <nome> '<json>' [PATH]  # invoca un tool MCP da terminale (anche --path PATH)
codenav ast <file>                # albero tree-sitter (per scrivere nuove query)
```

Esempi:

```sh
codenav tool explore '{"question":"come UserService crea gli utenti"}'
codenav tool search_symbols '{"query":"UserService"}'
codenav tool find_paths '{"from":"Server::run","to":"extract","directed":true}'
codenav tool module_graph '{"depth":2}'
codenav tool graph_overview ../altro-repo   # path posizionale, come per index e serve
codenav tool get_community '{"id":"UserService"}'
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

In [`vscode/`](vscode/README.md) c'è un'estensione che visualizza `.codenav/graph.cnav` dentro
VS Code (basta aprire il file): vista per moduli, per file e focus su un simbolo, con apertura del sorgente al clic e
aggiornamento automatico quando l'indice cambia.

## Tool MCP

| Tool | A cosa serve |
|---|---|
| `explore` | Domanda in linguaggio naturale → sottografo rilevante in una sola chiamata (BFS/DFS, seed espliciti, `token_budget`). I nodi di partenza si scelgono da nomi e doc comment (anche varianti delle parole); se la domanda trova poco riscontro nel codice la risposta lo dice (`low_confidence: true`) |
| `graph_overview` | Stato dell'indice (commit e branch Git da cui è stato costruito, modifiche non committate, avvisi se HEAD è cambiato o i sorgenti sono stati modificati), linguaggi, conteggi, simboli/file più dipendenti, dipendenze esterne principali, community più grandi |
| `module_graph` | Architettura: dipendenze aggregate tra directory/componenti |
| `insights` | Cosa merita un'occhiata: connessioni inattese tra file (confidenza bassa, poche connessioni tra community, codice periferico che raggiunge un hub), nodi ponte tra community (betweenness approssimata) e domande suggerite |
| `diff_impact` | Impatto di un insieme di modifiche Git (non committate, in stage, o `base...HEAD`): simboli modificati, file nuovi/cancellati, tutto ciò che ne dipende, community e moduli toccati |
| `tests_for` | Test da eseguire dopo una modifica: file di test (per convenzione di percorso, mai per somiglianza di nome) che dipendono dal simbolo o file, diretti e indiretti con distanza e percorso |
| `find_cycles` | Cicli di dipendenze tra file o moduli: componenti fortemente connesse e cicli più corti (default: import, archi almeno `inferred`) |
| `get_community` | Community strutturale di un nodo (o per numero): file, membri più collegati, community da cui dipende e che la usano |
| `search_symbols` | Ricerca per nome, frammento di id o testo della doc; filtri per kind e path. Ogni risultato dice come corrisponde (`exact`, `prefix`, `fragment`, `id`, `doc`), e una nota avvisa quando nessun simbolo ha il nome cercato |
| `get_symbol` | Dettaglio di un nodo: firma, doc, relazioni in/out per tipo, codice sorgente |
| `get_neighbors` | BFS attorno a un nodo (es. chi chiama X: `direction=in`, `edge_kinds=[calls]`) |
| `find_paths` | Catene di relazioni più brevi tra due nodi ("come sono collegati A e B?") |
| `file_outline` | Simboli di un file (annidati), import, importatori, file più correlati |
| `audit_edges` | Verifica del resolver: archi per tipo e confidenza, poi quelli di un livello (default `ambiguous`) con file e riga |
| `impact_analysis` | Tutto ciò che dipende transitivamente da un simbolo/file: cosa si rompe se cambia. Per un metodo conta anche chi chiama i metodi che sovrascrive (interfacce, classi base) |
| `read_code` | Legge un intervallo di righe di un file del repo |
| `list_projects` | Repository serviti (nome da passare come `project` a ogni tool), quale è il default e quali sono caricati |
| `reindex` | Ricostruisce il grafo |

Tutti i tool che restituiscono dati strutturati rispondono di default in **testo compatto**
(simile a YAML, senza virgolette e parentesi) tagliato a `token_budget` (default 2000, da 200 a
100000). Se l'output viene tagliato, la prima riga lo dice e indica il `token_budget` necessario
per vedere tutto, più un suggerimento per restringere la richiesta. Con `format: "json"` si ottiene
invece il risultato JSON completo, senza budget.

### Più repository

`codenav serve repo-a repo-b …` serve più repository da un solo server: ogni tool accetta
`project` (il nome mostrato da `list_projects`, o il path), e senza `project` risponde sul primo.
I grafi si caricano al primo uso; al massimo `--max-loaded` (default 4) restano in memoria, i meno
usati di recente vengono scaricati e ricaricati dal loro indice quando servono. Con
`--any-project` i tool possono aprire anche altri repository indicandone il path assoluto (solo
cartelle con `.git` o con un indice codenav). `--fresh` e `--hub-percentile` valgono per tutti i
repository.

### Risorse e prompt MCP

Oltre ai tool, il server espone **risorse** di sola lettura, utili come contesto iniziale (testo
compatto, budget di 8000 token): `codenav://overview`, `codenav://modules`, `codenav://insights`,
`codenav://cycles` e `codenav://audit`. Il prompt **`architecture-review`** (argomento opzionale
`focus`: una cartella, un file o un simbolo) guida l'LLM in una revisione dell'architettura con i
tool di codenav: componenti, cicli, punti critici e collegamenti da verificare.

### Community

Durante l'indicizzazione il grafo viene diviso in **community**: gruppi di file e simboli che
dipendono tra loro più che dal resto del repo (algoritmo di Louvain sugli archi `calls`,
`references`, `inherits`, `imports` e `contains`). Mostrano i componenti reali anche quando la
struttura delle cartelle non li riflette. Ogni community ha un numero (0 è la più grande) e
un'etichetta automatica: la cartella dominante, oppure il file o il simbolo dominante quando la
cartella è condivisa. Il calcolo è deterministico, quindi lo stesso codice dà sempre gli stessi
numeri.

Gli archi di dipendenza degli *hub* (i nodi più collegati, sopra il 99° percentile) non contano,
così le utility usate ovunque non uniscono parti che non c'entrano. La soglia si cambia con
`codenav index --hub-percentile N` (o `serve --hub-percentile N`); `100` disattiva l'esclusione.
Il valore viene salvato nell'indice e riusato dai re-index successivi (anche quelli automatici di
`serve`) finché non se ne passa un altro. I pacchetti esterni non hanno community, e gli archi
`ambiguous` pesano la metà; `get_community` li conta a parte (`ambiguous_edges`).

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
