import * as vscode from 'vscode';
import { ViewParams } from './graphModel';
import { GraphStore } from './graphStore';

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

/** Single webview panel showing the graph of one workspace folder at a time. */
export class GraphPanel implements vscode.Disposable {
  static readonly viewType = 'codenav.graph';
  private static current: GraphPanel | undefined;

  private readonly disposables: vscode.Disposable[] = [];
  private ready = false;
  private pending: Navigate | undefined;

  static show(
    context: vscode.ExtensionContext,
    store: GraphStore,
    folder: vscode.WorkspaceFolder,
    navigate?: Navigate,
  ): GraphPanel {
    const existing = GraphPanel.current;
    if (existing) {
      const folderChanged = existing.folder.uri.toString() !== folder.uri.toString();
      existing.folder = folder;
      existing.panel.reveal(undefined, true);
      if (folderChanged) {
        existing.post({ type: 'graphChanged' });
        void existing.sendInfo();
      }
      if (navigate) {
        existing.navigate(navigate);
      }
      return existing;
    }
    const panel = vscode.window.createWebviewPanel(
      GraphPanel.viewType,
      'Code Graph',
      { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [vscode.Uri.joinPath(context.extensionUri, 'media')],
      },
    );
    GraphPanel.current = new GraphPanel(context, store, folder, panel, navigate);
    return GraphPanel.current;
  }

  private constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly store: GraphStore,
    private folder: vscode.WorkspaceFolder,
    private readonly panel: vscode.WebviewPanel,
    navigate?: Navigate,
  ) {
    this.pending = navigate;
    panel.iconPath = new vscode.ThemeIcon('type-hierarchy');
    panel.webview.html = this.html();
    panel.onDidDispose(() => this.dispose(), null, this.disposables);
    panel.webview.onDidReceiveMessage((m: FromWebview) => this.onMessage(m), null, this.disposables);
    store.onDidChange(
      (f) => {
        if (f.uri.toString() === this.folder.uri.toString()) {
          this.post({ type: 'graphChanged' });
          void this.sendInfo();
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
    GraphPanel.current = undefined;
    this.panel.dispose();
    this.disposables.forEach((d) => d.dispose());
  }

  navigate(nav: Navigate) {
    if (this.ready) {
      this.post({ type: 'navigate', params: nav });
    } else {
      this.pending = nav;
    }
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
          const graph = await this.store.load(this.folder);
          const maxNodes = vscode.workspace.getConfiguration('codenav').get<number>('maxNodes', 400);
          this.post({ type: 'view', params: m.params, view: graph.build(m.params, maxNodes) });
          break;
        }
        case 'details': {
          const graph = await this.store.load(this.folder);
          this.post({ type: 'details', details: graph.details(m.id) });
          break;
        }
        case 'search': {
          const graph = await this.store.load(this.folder);
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
          await vscode.commands.executeCommand('codenav.reindex', this.folder.uri);
          break;
      }
    } catch (err) {
      this.post({ type: 'error', message: (err as Error).message });
    }
  }

  private async sendInfo() {
    try {
      const graph = await this.store.load(this.folder);
      this.post({
        type: 'info',
        folder: this.folder.name,
        indexedAt: graph.indexedAt,
        ...graph.size,
      });
    } catch (err) {
      this.post({ type: 'error', message: (err as Error).message });
    }
  }

  private async openNode(id: string) {
    const graph = await this.store.load(this.folder);
    const node = graph.node(id);
    if (!node || !node.file) {
      return;
    }
    const uri = vscode.Uri.joinPath(this.folder.uri, node.file);
    const line = Math.max(0, node.line - 1);
    const range = new vscode.Range(line, 0, line, 0);
    await vscode.window.showTextDocument(uri, {
      selection: range,
      viewColumn: vscode.ViewColumn.One,
      preserveFocus: false,
    });
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
