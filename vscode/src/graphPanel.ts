import * as vscode from 'vscode';
import { ViewParams } from './graphModel';
import { fileSource, folderSource, GraphSource, GraphStore } from './graphStore';

/** Messages sent by the webview (media/main.js). */
type FromWebview =
  | { type: 'ready' }
  | { type: 'requestView'; params: ViewParams }
  | { type: 'details'; id: string }
  | { type: 'open'; id: string }
  | { type: 'search'; query: string }
  | { type: 'reindex' };

/** Navigation pushed from the extension, e.g. "reveal the symbol under the cursor". */
export type Navigate = Partial<ViewParams>;

/** Single panel opened by the commands, showing one workspace folder's graph at a time. */
export class GraphPanel {
  static readonly viewType = 'codenav.graph';
  private static current: GraphView | undefined;

  static show(
    context: vscode.ExtensionContext,
    store: GraphStore,
    folder: vscode.WorkspaceFolder,
    navigate?: Navigate,
  ): GraphView {
    const source = folderSource(folder);
    const existing = GraphPanel.current;
    if (existing) {
      existing.setSource(source);
      existing.panel.reveal(undefined, true);
      if (navigate) {
        existing.navigate(navigate);
      }
      return existing;
    }
    const panel = vscode.window.createWebviewPanel(
      GraphPanel.viewType,
      'Code Graph',
      { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
      { retainContextWhenHidden: true },
    );
    const view = new GraphView(context, store, source, panel, navigate);
    panel.onDidDispose(() => {
      if (GraphPanel.current === view) {
        GraphPanel.current = undefined;
      }
    });
    GraphPanel.current = view;
    return view;
  }
}

/** Opens `.cnav` files as a graph instead of text. */
export class GraphEditorProvider implements vscode.CustomReadonlyEditorProvider {
  static readonly viewType = 'codenav.graphFile';

  static register(context: vscode.ExtensionContext, store: GraphStore): vscode.Disposable {
    return vscode.window.registerCustomEditorProvider(
      GraphEditorProvider.viewType,
      new GraphEditorProvider(context, store),
      { webviewOptions: { retainContextWhenHidden: true }, supportsMultipleEditorsPerDocument: true },
    );
  }

  private constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly store: GraphStore,
  ) {}

  openCustomDocument(uri: vscode.Uri): vscode.CustomDocument {
    return { uri, dispose: () => undefined };
  }

  async resolveCustomEditor(document: vscode.CustomDocument, panel: vscode.WebviewPanel): Promise<void> {
    const uri = document.uri;
    let source: GraphSource;
    try {
      source = fileSource(uri, await this.store.load(uri));
    } catch {
      // Unreadable graph: the view reports the error; source files can't be opened anyway.
      source = { key: uri.toString(), name: uri.path.split('/').pop() ?? '', graphUri: uri, root: vscode.Uri.joinPath(uri, '..') };
    }
    const view = new GraphView(this.context, this.store, source, panel);
    view.track(this.store.watchFile(uri));
  }
}

/** The graph webview: renders a graph source and answers the webview's requests. */
export class GraphView implements vscode.Disposable {
  private readonly disposables: vscode.Disposable[] = [];
  private ready = false;
  private pending: Navigate | undefined;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly store: GraphStore,
    private source: GraphSource,
    readonly panel: vscode.WebviewPanel,
    navigate?: Navigate,
  ) {
    this.pending = navigate;
    panel.iconPath = new vscode.ThemeIcon('type-hierarchy');
    panel.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(context.extensionUri, 'media')],
    };
    panel.webview.html = this.html();
    panel.onDidDispose(() => this.dispose(), null, this.disposables);
    panel.webview.onDidReceiveMessage((m: FromWebview) => this.onMessage(m), null, this.disposables);
    store.onDidChange(
      (key) => {
        if (key === this.source.key) {
          void this.refreshSource();
        }
      },
      null,
      this.disposables,
    );
    vscode.workspace.onDidChangeConfiguration(
      (e) => {
        if (e.affectsConfiguration('codenav.maxNodes')) {
          this.post({ type: 'graphChanged' });
        }
      },
      null,
      this.disposables,
    );
  }

  dispose() {
    this.disposables.forEach((d) => d.dispose());
    this.disposables.length = 0;
  }

  /** Disposes `d` together with the view. */
  track(d: vscode.Disposable) {
    this.disposables.push(d);
  }

  setSource(source: GraphSource) {
    if (source.key !== this.source.key) {
      this.source = source;
      this.post({ type: 'graphChanged' });
      void this.sendInfo();
    }
  }

  navigate(nav: Navigate) {
    if (this.ready) {
      this.post({ type: 'navigate', params: nav });
    } else {
      this.pending = nav;
    }
  }

  /** The graph file was rewritten: redraw, and for a standalone `.cnav` re-derive its root. */
  private async refreshSource() {
    if (!this.isFolderIndex()) {
      try {
        this.source = fileSource(this.source.graphUri, await this.store.load(this.source));
      } catch {
        // sendInfo reports the error.
      }
    }
    this.post({ type: 'graphChanged' });
    await this.sendInfo();
  }

  private isFolderIndex(): boolean {
    return this.source.graphUri.path.endsWith('/.codenav/graph.cnav');
  }

  private post(message: unknown) {
    void this.panel.webview.postMessage(message);
  }

  private async onMessage(m: FromWebview) {
    try {
      switch (m.type) {
        case 'ready': {
          this.ready = true;
          const config = vscode.workspace.getConfiguration('codenav');
          this.post({ type: 'config', moduleDepth: config.get<number>('moduleDepth', 2) });
          await this.sendInfo();
          if (this.pending) {
            this.post({ type: 'navigate', params: this.pending });
            this.pending = undefined;
          } else {
            this.post({ type: 'graphChanged' });
          }
          break;
        }
        case 'requestView': {
          const graph = await this.store.load(this.source);
          const maxNodes = vscode.workspace.getConfiguration('codenav').get<number>('maxNodes', 400);
          this.post({ type: 'view', params: m.params, view: graph.build(m.params, maxNodes) });
          break;
        }
        case 'details': {
          const graph = await this.store.load(this.source);
          this.post({ type: 'details', details: graph.details(m.id) });
          break;
        }
        case 'search': {
          const graph = await this.store.load(this.source);
          const results = graph.search(m.query, 30).map((n) => ({
            id: n.id,
            name: n.name,
            kind: n.kind,
            file: n.file,
            line: n.line,
          }));
          this.post({ type: 'searchResults', query: m.query, results });
          break;
        }
        case 'open':
          await this.openNode(m.id);
          break;
        case 'reindex':
          await vscode.commands.executeCommand('codenav.reindex', this.source.root);
          break;
      }
    } catch (err) {
      this.post({ type: 'error', message: (err as Error).message });
    }
  }

  private async sendInfo() {
    try {
      const graph = await this.store.load(this.source);
      this.post({
        type: 'info',
        folder: this.source.name,
        indexedAt: graph.indexedAt,
        ...graph.size,
      });
    } catch (err) {
      this.post({ type: 'error', message: (err as Error).message });
    }
  }

  private async openNode(id: string) {
    const graph = await this.store.load(this.source);
    const node = graph.node(id);
    if (!node || !node.file) {
      return;
    }
    const uri = vscode.Uri.joinPath(this.source.root, node.file);
    const line = Math.max(0, node.line - 1);
    const range = new vscode.Range(line, 0, line, 0);
    try {
      await vscode.window.showTextDocument(uri, {
        selection: range,
        viewColumn: vscode.ViewColumn.One,
        preserveFocus: false,
      });
    } catch {
      void vscode.window.showWarningMessage(`CodeNav: cannot open ${uri.fsPath}.`);
    }
  }

  private html(): string {
    const webview = this.panel.webview;
    const media = (file: string) => webview.asWebviewUri(vscode.Uri.joinPath(this.context.extensionUri, 'media', file));
    const nonce = makeNonce();
    // Cytoscape writes inline styles on its container, hence 'unsafe-inline' for styles only.
    const csp = [
      "default-src 'none'",
      `img-src ${webview.cspSource} data:`,
      `style-src ${webview.cspSource} 'unsafe-inline'`,
      `script-src 'nonce-${nonce}'`,
      `font-src ${webview.cspSource}`,
    ].join('; ');
    return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta http-equiv="Content-Security-Policy" content="${csp}">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <link rel="stylesheet" href="${media('main.css')}">
  <title>Code Graph</title>
</head>
<body>
  <header class="toolbar">
    <button id="back" title="Back" disabled>&#8592;</button>
    <div class="segmented" id="modes" role="tablist">
      <button data-mode="modules" title="Directories as components">Modules</button>
      <button data-mode="files" title="Files grouped by directory">Files</button>
      <button data-mode="focus" title="Neighbourhood of one symbol">Focus</button>
    </div>
    <div class="search">
      <input id="search" type="search" placeholder="Search symbols…" autocomplete="off" spellcheck="false">
      <ul id="results" hidden></ul>
    </div>
    <label class="field" id="depthField" title="Directory depth (Modules) or hops from the center (Focus)">
      Depth <input id="depth" type="number" min="1" max="6" value="2">
    </label>
    <label class="field" id="directionField" title="Which relations to follow from the center">
      <select id="direction">
        <option value="both">In + out</option>
        <option value="out">Outgoing</option>
        <option value="in">Incoming</option>
      </select>
    </label>
    <label class="field" title="Layout algorithm">
      <select id="layout">
        <option value="fcose">Force</option>
        <option value="cose">Force (classic)</option>
        <option value="breadthfirst">Tree</option>
        <option value="concentric">Concentric</option>
        <option value="circle">Circle</option>
        <option value="grid">Grid</option>
      </select>
    </label>
    <label class="field" id="colorField" title="Node colour: structural community or symbol kind (the shape always shows the kind)">
      Color <select id="colorBy">
        <option value="community">Community</option>
        <option value="kind">Kind</option>
      </select>
    </label>
    <button id="fit" title="Fit to screen">Fit</button>
    <button id="reindex" title="Run codenav index">Re-index</button>
  </header>
  <div class="filters" id="edgeKinds">
    <span class="muted">Edges:</span>
    <label><input type="checkbox" value="calls" checked><i class="swatch e-calls"></i>calls</label>
    <label><input type="checkbox" value="imports" checked><i class="swatch e-imports"></i>imports</label>
    <label><input type="checkbox" value="references" checked><i class="swatch e-references"></i>references</label>
    <label><input type="checkbox" value="inherits" checked><i class="swatch e-inherits"></i>inherits</label>
    <label><input type="checkbox" value="contains" checked><i class="swatch e-contains"></i>contains</label>
    <label class="sep"><input type="checkbox" id="showExternal">external packages</label>
    <span class="spacer"></span>
    <span id="status" class="muted"></span>
  </div>
  <main>
    <div id="cy"></div>
    <div id="empty" class="empty" hidden></div>
    <aside id="details" hidden></aside>
    <div id="legend" hidden></div>
    <div id="tooltip" hidden></div>
  </main>
  <script nonce="${nonce}" src="${media('vendor/cytoscape.min.js')}"></script>
  <script nonce="${nonce}" src="${media('vendor/layout-base.js')}"></script>
  <script nonce="${nonce}" src="${media('vendor/cose-base.js')}"></script>
  <script nonce="${nonce}" src="${media('vendor/cytoscape-fcose.js')}"></script>
  <script nonce="${nonce}" src="${media('main.js')}"></script>
</body>
</html>`;
  }
}

function makeNonce(): string {
  const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
  let s = '';
  for (let i = 0; i < 32; i++) {
    s += chars.charAt(Math.floor(Math.random() * chars.length));
  }
  return s;
}
