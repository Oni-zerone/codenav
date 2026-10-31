import * as vscode from 'vscode';
import { cnavToJson } from './cnavFile';
import { CodeGraph } from './graphModel';

export const GRAPH_FILE = '.codenav/graph.cnav';

/**
 * A graph file and the repository it describes. Paths in the graph are relative to `root`.
 * `key` identifies the graph file and is the cache key.
 */
export interface GraphSource {
  key: string;
  name: string;
  graphUri: vscode.Uri;
  root: vscode.Uri;
}

/** Source of a workspace folder's own index (`<folder>/.codenav/graph.cnav`). */
export function folderSource(folder: vscode.WorkspaceFolder): GraphSource {
  const graphUri = vscode.Uri.joinPath(folder.uri, GRAPH_FILE);
  return { key: graphUri.toString(), name: folder.name, graphUri, root: folder.uri };
}

/**
 * Source of any `.cnav` file. Inside a `.codenav` directory the repository is its parent;
 * otherwise (a copied or renamed graph) it is the root recorded in the graph at index time.
 */
export function fileSource(graphUri: vscode.Uri, graph: CodeGraph): GraphSource {
  const dir = vscode.Uri.joinPath(graphUri, '..');
  const root = basename(dir) === '.codenav' ? vscode.Uri.joinPath(dir, '..') : vscode.Uri.file(graph.root);
  return { key: graphUri.toString(), name: basename(root) || graph.root, graphUri, root };
}

/** Loads graph files, caches them, and reloads them when codenav rewrites them. */
export class GraphStore implements vscode.Disposable {
  private readonly cache = new Map<string, Promise<CodeGraph>>();
  private readonly changed = new vscode.EventEmitter<string>();
  private readonly watcher: vscode.FileSystemWatcher;
  private reloadTimers = new Map<string, NodeJS.Timeout>();

  /** Fires with the `key` of a graph file that changed on disk. */
  readonly onDidChange = this.changed.event;

  constructor() {
    this.watcher = vscode.workspace.createFileSystemWatcher('**/' + GRAPH_FILE);
    this.listen(this.watcher);
    void this.updateContext();
  }

  dispose() {
    this.watcher.dispose();
    this.changed.dispose();
    this.reloadTimers.forEach((t) => clearTimeout(t));
  }

  /** Watches one graph file, e.g. a `.cnav` opened from outside the workspace. */
  watchFile(uri: vscode.Uri): vscode.Disposable {
    const dir = vscode.Uri.joinPath(uri, '..');
    const watcher = vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(dir, basename(uri)));
    this.listen(watcher);
    return watcher;
  }

  load(source: GraphSource | vscode.Uri): Promise<CodeGraph> {
    const uri = source instanceof vscode.Uri ? source : source.graphUri;
    const key = uri.toString();
    let p = this.cache.get(key);
    if (!p) {
      p = (async () => {
        const bytes = await vscode.workspace.fs.readFile(uri);
        return CodeGraph.parse(cnavToJson(bytes));
      })();
      this.cache.set(key, p);
      p.catch(() => this.cache.delete(key));
    }
    return p;
  }

  async hasGraph(folder: vscode.WorkspaceFolder): Promise<boolean> {
    try {
      await vscode.workspace.fs.stat(folderSource(folder).graphUri);
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

  private listen(watcher: vscode.FileSystemWatcher) {
    const onFs = (uri: vscode.Uri) => {
      // codenav may write the file in several chunks; wait for it to settle.
      const key = uri.toString();
      clearTimeout(this.reloadTimers.get(key));
      this.reloadTimers.set(
        key,
        setTimeout(() => {
          this.reloadTimers.delete(key);
          this.cache.delete(key);
          this.changed.fire(key);
          void this.updateContext();
        }, 300),
      );
    };
    watcher.onDidCreate(onFs);
    watcher.onDidChange(onFs);
    watcher.onDidDelete(onFs);
  }

  private async updateContext() {
    const indexed = await this.indexedFolders();
    await vscode.commands.executeCommand('setContext', 'codenav.hasGraph', indexed.length > 0);
  }
}

/** Path of `uri` relative to `root`, with `/` separators, as stored in the graph. */
export function relativePath(root: vscode.Uri, uri: vscode.Uri): string {
  const base = root.path.replace(/\/+$/, '');
  return uri.path.startsWith(base + '/') ? uri.path.slice(base.length + 1) : uri.path;
}

function basename(uri: vscode.Uri): string {
  return uri.path.replace(/\/+$/, '').split('/').pop() ?? '';
}
