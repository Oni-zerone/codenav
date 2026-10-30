import { execFile } from 'child_process';
import * as vscode from 'vscode';
import { GraphEditorProvider, GraphPanel } from './graphPanel';
import { folderSource, GRAPH_FILE, GraphStore, relativePath } from './graphStore';

let output: vscode.OutputChannel | undefined;

export function activate(context: vscode.ExtensionContext) {
  const store = new GraphStore();
  context.subscriptions.push(store, GraphEditorProvider.register(context, store));

  context.subscriptions.push(
    vscode.commands.registerCommand('codenav.showGraph', async () => {
      const folder = await pickFolder(store);
      if (folder) {
        GraphPanel.show(context, store, folder);
      }
    }),

    vscode.commands.registerCommand('codenav.revealInGraph', async (uri?: vscode.Uri) => {
      const editor = vscode.window.activeTextEditor;
      const target = uri ?? editor?.document.uri;
      if (!target) {
        return vscode.commands.executeCommand('codenav.showGraph');
      }
      const folder = vscode.workspace.getWorkspaceFolder(target);
      if (!folder || !(await ensureIndexed(store, folder))) {
        return;
      }
      const graph = await store.load(folderSource(folder));
      const file = relativePath(folder.uri, target);
      // From the explorer (or another file) reveal the file; from the editor, the symbol under the cursor.
      const fromEditor = editor && editor.document.uri.toString() === target.toString();
      const node = fromEditor
        ? graph.symbolAt(file, editor.selection.active.line + 1)
        : graph.fileNode(file);
      if (!node) {
        void vscode.window.showInformationMessage(
          `${file} is not in the code graph. Re-index if the file is new or not a supported language.`,
        );
        return;
      }
      GraphPanel.show(context, store, folder, { mode: 'focus', center: node.id });
    }),

    vscode.commands.registerCommand('codenav.reindex', async (uri?: vscode.Uri) => {
      // `uri` is a repository root (from the graph view); it may be outside the workspace.
      const root = uri ?? (await pickFolder(store, true))?.uri;
      if (root) {
        await reindex(root);
      }
    }),
  );
}

export function deactivate() {
  output?.dispose();
}

/** Picks the workspace folder to show: the active editor's, the only indexed one, or ask. */
async function pickFolder(store: GraphStore, includeUnindexed = false): Promise<vscode.WorkspaceFolder | undefined> {
  const folders = vscode.workspace.workspaceFolders ?? [];
  if (folders.length === 0) {
    void vscode.window.showWarningMessage('CodeNav: open a folder first.');
    return undefined;
  }
  const active = vscode.window.activeTextEditor?.document.uri;
  const activeFolder = active ? vscode.workspace.getWorkspaceFolder(active) : undefined;
  if (activeFolder && (includeUnindexed || (await store.hasGraph(activeFolder)))) {
    return activeFolder;
  }
  const candidates = includeUnindexed ? folders : await store.indexedFolders();
  if (candidates.length === 1) {
    return candidates[0];
  }
  if (candidates.length === 0) {
    return (await ensureIndexed(store, folders[0])) ? folders[0] : undefined;
  }
  return vscode.window.showWorkspaceFolderPick({ placeHolder: 'Workspace folder' });
}

/** Offers to run `codenav index` when the folder has no graph yet. */
async function ensureIndexed(store: GraphStore, folder: vscode.WorkspaceFolder): Promise<boolean> {
  if (await store.hasGraph(folder)) {
    return true;
  }
  const choice = await vscode.window.showInformationMessage(
    `No code graph found in ${folder.name} (${GRAPH_FILE}).`,
    'Index now',
  );
  return choice === 'Index now' && (await reindex(folder.uri));
}

async function reindex(root: vscode.Uri): Promise<boolean> {
  const name = root.path.replace(/\/+$/, '').split('/').pop();
  const exe = vscode.workspace.getConfiguration('codenav').get<string>('executable', 'codenav') || 'codenav';
  output ??= vscode.window.createOutputChannel('CodeNav');
  return vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title: `CodeNav: indexing ${name}…` },
    () =>
      new Promise<boolean>((resolve) => {
        execFile(
          exe,
          ['index', root.fsPath],
          { cwd: root.fsPath, maxBuffer: 16 * 1024 * 1024 },
          (err, stdout, stderr) => {
            output!.appendLine(`$ ${exe} index ${root.fsPath}`);
            if (stdout) {
              output!.append(stdout);
            }
            if (stderr) {
              output!.append(stderr);
            }
            if (err) {
              const notFound = (err as NodeJS.ErrnoException).code === 'ENOENT';
              const message = notFound
                ? `CodeNav: "${exe}" not found. Install it with \`cargo install --path .\` or set codenav.executable.`
                : `CodeNav: indexing failed (${err.message.split('\n')[0]}).`;
              void vscode.window.showErrorMessage(message, 'Show output').then((c) => {
                if (c) {
                  output!.show();
                }
              });
              resolve(false);
            } else {
              // The graph store's file watchers pick up the new graph file.
              resolve(true);
            }
          },
        );
      }),
  );
}
