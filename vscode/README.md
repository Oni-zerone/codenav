# CodeNav Graph (VS Code)

Visualizza dentro VS Code il grafo della codebase generato da [codenav](../README.md)
(`.codenav/graph.json`): lo stesso grafo che gli LLM navigano via MCP.

## Viste

| Vista | Cosa mostra |
|---|---|
| **Modules** | Directory (alla profondità scelta) come componenti, con le dipendenze aggregate tra loro. Pacchetti esterni opzionali. |
| **Files** | File raggruppati per directory (gruppi annidati), con import/chiamate/riferimenti aggregati tra file. Doppio clic su una directory per restringere la vista: i file esterni collegati restano visibili, attenuati. |
| **Focus** | L'intorno di un simbolo o di un file: BFS a profondità N, archi entranti/uscenti, simboli annidati nel loro file. |

Interazioni:

- **clic** su un nodo: pannello dettagli (firma, doc, posizione, relazioni per tipo). Clic su
  una relazione (es. *called by 5*) per vedere solo quelle;
- **doppio clic**: scende di livello (directory → file → focus, o ricentra il focus);
- **Open** / clic sul percorso: apre il sorgente alla riga del simbolo;
- ricerca simboli nella toolbar (`Cmd/Ctrl+F`), `Alt+←` per tornare indietro;
- filtri per tipo di arco (`calls`, `imports`, `references`, `inherits`, `contains`) e
  layout (force, albero, concentrico, cerchio, griglia).

Archi tratteggiati = riferimenti `ambiguous` (risolti per nome tra più candidati). Lo spessore
cresce con il numero di occorrenze.

## Comandi

| Comando | |
|---|---|
| `CodeNav: Show Code Graph` | Apre il pannello del grafo (anche dall'icona nella barra dell'editor). |
| `CodeNav: Reveal in Code Graph` | Focus sul simbolo sotto il cursore (menu contestuale dell'editor) o sul file (menu dell'Explorer). |
| `CodeNav: Re-index Repository` | Esegue `codenav index` sulla cartella. |

Il pannello si aggiorna da solo quando `graph.json` cambia (ad esempio quando `codenav serve`
re-indicizza in background).

## Impostazioni

| Impostazione | Default | |
|---|---|---|
| `codenav.executable` | `codenav` | Binario usato da *Re-index*. |
| `codenav.maxNodes` | `400` | Nodi massimi per vista; oltre si tengono i più collegati. |
| `codenav.moduleDepth` | `2` | Profondità iniziale della vista Modules. |

## Sviluppo

```sh
cd vscode
npm install
npm run compile      # tsc + copia cytoscape/fcose in media/vendor
npm test             # test del modello (node:test, senza VS Code)
```

Per provarla: apri la cartella `vscode/` in VS Code e premi `F5` (Extension Development Host),
poi apri una repo indicizzata con `codenav index`. Per creare il pacchetto: `npx @vscode/vsce package`.

Struttura:

- `src/graphModel.ts`: parsing di `graph.json` e costruzione delle viste (puro, testato);
- `src/graphStore.ts`: caricamento per workspace folder e file watcher;
- `src/graphPanel.ts`: webview e protocollo dei messaggi;
- `src/extension.ts`: comandi;
- `media/main.js`, `media/main.css`: UI della webview (Cytoscape.js + layout fcose).
