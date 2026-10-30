import * as vscode from 'vscode';
import { CodeGraph } from './graphModel';

const GRAPH_GLOB = '.codenav/graph.json';

/** Loaded graph of one workspace folder. Paths in the graph are relative to `folder`. */
export interface LoadedGraph {
  folder: vscode.WorkspaceFolder;
  graph: CodeGraph;
}

/**
 * Loads `.codenav/graph.json` per workspace folder, caches it, and reloads it when codenav
 * rewrites the file.
 */
export class GraphStore implements vscode.Disposable {
  private readonly cache = new Map<string, Promise<CodeGraph>>();
  private readonly changed = new vscode.EventEmitter<vscode.WorkspaceFolder>();
  private readonly watcher: vscode.FileSystemWatcher;
  private reloadTimers = new Map<string, NodeJS.Timeout>();

  readonly onDidChange = this.changed.event;

  constructor() {
    this.watcher = vscode.workspace.createFileSystemWatcher('**/' + GRAPH_GLOB);
    const onFs = (uri: vscode.Uri) => {
      const folder = vscode.workspace.getWorkspaceFolder(uri);
      if (!folder || uri.toString() !== graphUri(folder).toString()) {
        return;
      }
      // codenav may write the file in several chunks; wait for it to settle.
      const key = folder.uri.toString();
      clearTimeout(this.reloadTimers.get(key));
      this.reloadTimers.set(
        key,
        setTimeout(() => {
          this.cache.delete(key);
          this.changed.fire(folder);
          void this.updateContext();
        }, 300),
      );
    };
    this.watcher.onDidCreate(onFs);
    this.watcher.onDidChange(onFs);
    this.watcher.onDidDelete(onFs);
    void this.updateContext();
  }

  dispose() {
    this.watcher.dispose();
    this.changed.dispose();
    this.reloadTimers.forEach((t) => clearTimeout(t));
  }

  async load(folder: vscode.WorkspaceFolder): Promise<CodeGraph> {
    const key = folder.uri.toString();
    let p = this.cache.get(key);
    if (!p) {
      p = (async () => {
        const bytes = await vscode.workspace.fs.readFile(graphUri(folder));
        return CodeGraph.parse(Buffer.from(bytes).toString('utf8'));
      })();
      this.cache.set(key, p);
      p.catch(() => this.cache.delete(key));
    }
    return p;
  }

  async hasGraph(folder: vscode.WorkspaceFolder): Promise<boolean> {
    try {
      await vscode.workspace.fs.stat(graphUri(folder));
      return true;
    } catch {
      return false;
    }
  }

  /** Folders that already have an index. */
  async indexedFolders(): Promise<vscode.WorkspaceFolder[]> {
    const folders = vscode.workspace.workspaceFolders ?? [];
    const flags = await Promise.all(folders.map((f) => this.hasGraph(f)));
    return folders.filter((_, i) => flags[i]);
  }

  private async updateContext() {
    const indexed = await this.indexedFolders();
    await vscode.commands.executeCommand('setContext', 'codenav.hasGraph', indexed.length > 0);
  }
}

export function graphUri(folder: vscode.WorkspaceFolder): vscode.Uri {
  return vscode.Uri.joinPath(folder.uri, GRAPH_GLOB);
}

/** Path of `uri` relative to `folder`, with `/` separators, as stored in the graph. */
export function relativePath(folder: vscode.WorkspaceFolder, uri: vscode.Uri): string {
  const base = folder.uri.path.replace(/\/+$/, '');
  return uri.path.startsWith(base + '/') ? uri.path.slice(base.length + 1) : uri.path;
}
