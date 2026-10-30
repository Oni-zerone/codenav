// Pure model of `.codenav/graph.cnav` (JSON) and the views drawn by the webview.
// No `vscode` import here, so it can be unit-tested with plain Node.

export const SUPPORTED_VERSION = 1;

export type SymKind =
  | 'file' | 'external' | 'module' | 'class' | 'struct' | 'enum'
  | 'interface' | 'type' | 'function' | 'method' | 'constant' | 'macro';

export type EdgeKind = 'contains' | 'imports' | 'calls' | 'inherits' | 'references';

export const EDGE_KINDS: EdgeKind[] = ['contains', 'imports', 'calls', 'inherits', 'references'];

export interface RawNode {
  id: string;
  kind: SymKind;
  name: string;
  file: string;
  lang?: string;
  line: number;
  end_line: number;
  signature?: string;
  doc?: string;
  parent?: number;
}

export interface RawEdge {
  from: number;
  to: number;
  kind: EdgeKind;
  count: number;
  ambiguous?: boolean;
  line: number;
}

export interface RawGraph {
  version: number;
  root: string;
  indexed_at: number;
  nodes: RawNode[];
  edges: RawEdge[];
}

/** Node as drawn by the webview. `kind` may also be `dir` (a directory group or component). */
export interface ViewNode {
  id: string;
  label: string;
  kind: SymKind | 'dir';
  parent?: string;
  /** Repo-relative path of `file` and `dir` nodes (a `dir` label may be shortened). */
  path?: string;
  /** Present when the node maps to a graph node, so it can be opened or inspected. */
  ref?: string;
  /** Number of files (directories) or symbols (files) it stands for. */
  size?: number;
  focus?: boolean;
  /** Shown for context only (outside the drilled-down directory). */
  dim?: boolean;
  tooltip?: string;
}

export interface ViewEdge {
  id: string;
  source: string;
  target: string;
  kind: EdgeKind;
  count: number;
  ambiguous?: boolean;
}

export interface View {
  title: string;
  nodes: ViewNode[];
  edges: ViewEdge[];
  /** Set when nodes were dropped to stay under `maxNodes`. */
  truncated?: { shown: number; total: number };
}

export type Direction = 'in' | 'out' | 'both';

export interface ViewParams {
  mode: 'modules' | 'files' | 'focus';
  edgeKinds: EdgeKind[];
  showExternal: boolean;
  /** modules: directory depth defining a component. focus: BFS depth. */
  depth?: number;
  /** files: only files under this directory (plus their direct neighbours, dimmed). */
  dir?: string;
  /** focus: id of the center node. */
  center?: string;
  direction?: Direction;
}

export interface NodeDetails {
  id: string;
  kind: SymKind;
  name: string;
  file: string;
  lang?: string;
  line: number;
  endLine: number;
  signature?: string;
  doc?: string;
  outgoing: Record<string, number>;
  incoming: Record<string, number>;
}

export class GraphError extends Error {}

export class CodeGraph {
  readonly nodes: RawNode[];
  readonly edges: RawEdge[];
  readonly root: string;
  readonly indexedAt: number;
  private readonly out: number[][];
  private readonly in: number[][];
  private readonly byId = new Map<string, number>();
  private readonly byFile = new Map<string, number>();
  /** Symbols of each file, for cursor lookup. */
  private readonly symbolsByFile = new Map<string, number[]>();

  constructor(raw: RawGraph) {
    this.nodes = raw.nodes;
    this.edges = raw.edges;
    this.root = raw.root;
    this.indexedAt = raw.indexed_at;
    this.out = this.nodes.map(() => []);
    this.in = this.nodes.map(() => []);
    this.nodes.forEach((n, i) => {
      this.byId.set(n.id, i);
      if (n.kind === 'file') {
        this.byFile.set(n.file, i);
      } else if (n.kind !== 'external' && n.file) {
        let list = this.symbolsByFile.get(n.file);
        if (!list) {
          list = [];
          this.symbolsByFile.set(n.file, list);
        }
        list.push(i);
      }
    });
    this.edges.forEach((e, i) => {
      if (this.nodes[e.from] && this.nodes[e.to]) {
        this.out[e.from].push(i);
        this.in[e.to].push(i);
      }
    });
  }

  static parse(text: string): CodeGraph {
    let raw: unknown;
    try {
      raw = JSON.parse(text);
    } catch (err) {
      throw new GraphError(`The code graph is not valid JSON: ${(err as Error).message}`);
    }
    const g = raw as Partial<RawGraph>;
    if (!g || typeof g !== 'object' || !Array.isArray(g.nodes) || !Array.isArray(g.edges)) {
      throw new GraphError('The code graph has no nodes/edges arrays');
    }
    if (g.version !== SUPPORTED_VERSION) {
      throw new GraphError(
        `Code graph format version ${g.version} is not supported (expected ${SUPPORTED_VERSION}). ` +
          'Re-index with a matching codenav version.',
      );
    }
    return new CodeGraph(g as RawGraph);
  }

  get size(): { nodes: number; edges: number; files: number } {
    return { nodes: this.nodes.length, edges: this.edges.length, files: this.byFile.size };
  }

  index(id: string): number | undefined {
    return this.byId.get(id);
  }

  node(id: string): RawNode | undefined {
    const i = this.byId.get(id);
    return i === undefined ? undefined : this.nodes[i];
  }

  /** Index of the file node containing node `i` (itself for files, undefined for externals). */
  fileOf(i: number): number | undefined {
    const n = this.nodes[i];
    if (n.kind === 'file') {
      return i;
    }
    if (n.kind === 'external') {
      return undefined;
    }
    return this.byFile.get(n.file);
  }

  fileNode(path: string): RawNode | undefined {
    const i = this.byFile.get(path);
    return i === undefined ? undefined : this.nodes[i];
  }

  /** Innermost symbol whose range contains `line` (1-based), falling back to the file node. */
  symbolAt(file: string, line: number): RawNode | undefined {
    let best: RawNode | undefined;
    for (const i of this.symbolsByFile.get(file) ?? []) {
      const n = this.nodes[i];
      if (n.line <= line && line <= n.end_line) {
        if (!best || n.end_line - n.line < best.end_line - best.line) {
          best = n;
        }
      }
    }
    return best ?? this.fileNode(file);
  }

  /** Name matches first (exact, prefix, substring), then id substrings; case-insensitive. */
  search(query: string, limit = 50): RawNode[] {
    const q = query.trim().toLowerCase();
    if (!q) {
      return [];
    }
    const scored: { i: number; score: number }[] = [];
    this.nodes.forEach((n, i) => {
      const name = n.name.toLowerCase();
      let score: number;
      if (name === q) {
        score = 0;
      } else if (name.startsWith(q)) {
        score = 1;
      } else if (name.includes(q)) {
        score = 2;
      } else if (n.id.toLowerCase().includes(q)) {
        score = 3;
      } else {
        return;
      }
      scored.push({ i, score });
    });
    scored.sort(
      (a, b) =>
        a.score - b.score ||
        this.degree(b.i) - this.degree(a.i) ||
        this.nodes[a.i].id.localeCompare(this.nodes[b.i].id),
    );
    return scored.slice(0, limit).map((s) => this.nodes[s.i]);
  }

  degree(i: number): number {
    return this.out[i].length + this.in[i].length;
  }

  details(id: string): NodeDetails | undefined {
    const i = this.byId.get(id);
    if (i === undefined) {
      return undefined;
    }
    const n = this.nodes[i];
    const count = (list: number[]) => {
      const acc: Record<string, number> = {};
      for (const e of list) {
        const k = this.edges[e].kind;
        acc[k] = (acc[k] ?? 0) + 1;
      }
      return acc;
    };
    return {
      id: n.id,
      kind: n.kind,
      name: n.name,
      file: n.file,
      lang: n.lang,
      line: n.line,
      endLine: n.end_line,
      signature: n.signature,
      doc: n.doc,
      outgoing: count(this.out[i]),
      incoming: count(this.in[i]),
    };
  }

  build(params: ViewParams, maxNodes: number): View {
    switch (params.mode) {
      case 'modules':
        return this.moduleView(params, maxNodes);
      case 'files':
        return this.fileView(params, maxNodes);
      case 'focus':
        return this.focusView(params, maxNodes);
    }
  }

  /** Directories at `depth` as components, with dependencies aggregated between them. */
  moduleView(params: ViewParams, maxNodes: number): View {
    const depth = Math.max(1, params.depth ?? 2);
    const kinds = new Set<EdgeKind>(params.edgeKinds.filter((k) => k !== 'contains'));
    const nodes = new Map<string, ViewNode>();
    const dirKey = (file: string) => 'dir:' + dirPrefix(file, depth);

    for (const n of this.nodes) {
      if (n.kind !== 'file') {
        continue;
      }
      const key = dirKey(n.file);
      const existing = nodes.get(key);
      if (existing) {
        existing.size = (existing.size ?? 0) + 1;
      } else {
        const dir = dirPrefix(n.file, depth);
        nodes.set(key, { id: key, label: dir, kind: 'dir', path: dir, size: 1, tooltip: dir });
      }
    }

    const agg = new EdgeAggregator();
    for (const e of this.edges) {
      if (!kinds.has(e.kind)) {
        continue;
      }
      const src = this.fileOf(e.from);
      if (src === undefined) {
        continue;
      }
      const from = dirKey(this.nodes[src].file);
      const target = this.nodes[e.to];
      let to: string;
      if (target.kind === 'external') {
        if (!params.showExternal) {
          continue;
        }
        to = target.id;
        if (!nodes.has(to)) {
          nodes.set(to, externalNode(target));
        }
      } else {
        const dst = this.fileOf(e.to);
        if (dst === undefined) {
          continue;
        }
        to = dirKey(this.nodes[dst].file);
      }
      if (from !== to) {
        agg.add(from, to, e);
      }
    }

    const title = `Modules (depth ${depth})`;
    return truncate(title, [...nodes.values()], agg.edges(), maxNodes);
  }

  /** Files grouped by directory, with dependencies aggregated between files. */
  fileView(params: ViewParams, maxNodes: number): View {
    const kinds = new Set<EdgeKind>(params.edgeKinds.filter((k) => k !== 'contains'));
    const dir = params.dir ? params.dir.replace(/\/+$/, '') : undefined;
    const inScope = (file: string) => !dir || dir === '.' || file === dir || file.startsWith(dir + '/');
    const nodes = new Map<string, ViewNode>();

    const addFile = (i: number, dim: boolean) => {
      const n = this.nodes[i];
      if (!nodes.has(n.id)) {
        nodes.set(n.id, {
          id: n.id,
          label: n.name,
          kind: 'file',
          ref: n.id,
          path: n.file,
          size: this.symbolsByFile.get(n.file)?.length ?? 0,
          dim,
          tooltip: n.file,
        });
      }
    };

    this.nodes.forEach((n, i) => {
      if (n.kind === 'file' && inScope(n.file)) {
        addFile(i, false);
      }
    });

    const agg = new EdgeAggregator();
    for (const e of this.edges) {
      if (!kinds.has(e.kind)) {
        continue;
      }
      const src = this.fileOf(e.from);
      if (src === undefined) {
        continue;
      }
      const target = this.nodes[e.to];
      if (target.kind === 'external') {
        if (!params.showExternal || !inScope(this.nodes[src].file)) {
          continue;
        }
        if (!nodes.has(target.id)) {
          nodes.set(target.id, externalNode(target));
        }
        agg.add(this.nodes[src].id, target.id, e);
        continue;
      }
      const dst = this.fileOf(e.to);
      if (dst === undefined || dst === src) {
        continue;
      }
      const srcIn = inScope(this.nodes[src].file);
      const dstIn = inScope(this.nodes[dst].file);
      if (!srcIn && !dstIn) {
        continue;
      }
      // Files outside the drilled-down directory are kept as dimmed context.
      if (!srcIn) {
        addFile(src, true);
      }
      if (!dstIn) {
        addFile(dst, true);
      }
      agg.add(this.nodes[src].id, this.nodes[dst].id, e);
    }

    const title = dir && dir !== '.' ? `Files in ${dir}` : 'Files';
    const view = truncate(title, [...nodes.values()], agg.edges(), maxNodes);
    view.nodes.push(...directoryGroups(view.nodes));
    return view;
  }

  /** Neighbourhood of one node, symbols nested in their files. */
  focusView(params: ViewParams, maxNodes: number): View {
    const center = params.center ? this.byId.get(params.center) : undefined;
    if (center === undefined) {
      return { title: 'Focus', nodes: [], edges: [] };
    }
    const depth = Math.max(1, params.depth ?? 1);
    const direction = params.direction ?? 'both';
    const kinds = new Set(params.edgeKinds);
    const budget = Math.max(1, maxNodes);

    // BFS. `contains` is only followed from the center: following it further would pull in
    // every sibling of every file that was reached.
    const dist = new Map<number, number>([[center, 0]]);
    const usedEdges = new Set<number>();
    const dropped = new Set<number>();
    let frontier = [center];
    for (let d = 0; d < depth && frontier.length > 0; d++) {
      const next: number[] = [];
      for (const i of frontier) {
        const visit = (edgeIdx: number, other: number) => {
          const e = this.edges[edgeIdx];
          if (!kinds.has(e.kind) || (e.kind === 'contains' && i !== center)) {
            return;
          }
          if (this.nodes[other].kind === 'external' && !params.showExternal) {
            return;
          }
          if (!dist.has(other)) {
            if (dist.size >= budget) {
              dropped.add(other);
              return;
            }
            dist.set(other, d + 1);
            next.push(other);
          }
          usedEdges.add(edgeIdx);
        };
        if (direction !== 'in') {
          for (const e of this.out[i]) {
            visit(e, this.edges[e].to);
          }
        }
        if (direction !== 'out') {
          for (const e of this.in[i]) {
            visit(e, this.edges[e].from);
          }
        }
      }
      frontier = next;
    }

    const nodes = new Map<string, ViewNode>();
    for (const i of dist.keys()) {
      const n = this.nodes[i];
      let parent: string | undefined;
      if (n.kind !== 'file' && n.kind !== 'external') {
        const f = this.fileOf(i);
        if (f !== undefined) {
          const fileNode = this.nodes[f];
          parent = fileNode.id;
          if (!nodes.has(parent)) {
            nodes.set(parent, {
              id: parent,
              label: fileNode.file,
              kind: 'file',
              ref: parent,
              tooltip: fileNode.file,
            });
          }
        }
      }
      const existing = nodes.get(n.id);
      nodes.set(n.id, {
        ...existing,
        id: n.id,
        label: n.kind === 'file' ? n.file : n.name,
        kind: n.kind,
        parent: existing?.parent ?? parent,
        ref: n.id,
        focus: i === center,
        tooltip: nodeTooltip(n),
      });
    }

    // Also draw edges among the visible nodes that BFS did not walk (e.g. A→B when both are
    // neighbours of the center), so the picture is not misleadingly sparse.
    for (const i of dist.keys()) {
      for (const e of this.out[i]) {
        const edge = this.edges[e];
        if (kinds.has(edge.kind) && edge.kind !== 'contains' && dist.has(edge.to)) {
          usedEdges.add(e);
        }
      }
    }

    const edges: ViewEdge[] = [];
    for (const e of usedEdges) {
      const edge = this.edges[e];
      const source = this.nodes[edge.from].id;
      const target = this.nodes[edge.to].id;
      // A file→symbol `contains` edge is already shown by nesting.
      if (edge.kind === 'contains' && nodes.get(target)?.parent === source) {
        continue;
      }
      edges.push({
        id: `e${e}`,
        source,
        target,
        kind: edge.kind,
        count: edge.count,
        ambiguous: edge.ambiguous || undefined,
      });
    }

    const view: View = { title: `Focus: ${this.nodes[center].id}`, nodes: [...nodes.values()], edges };
    if (dropped.size > 0) {
      view.truncated = { shown: dist.size, total: dist.size + dropped.size };
    }
    return view;
  }
}

class EdgeAggregator {
  private readonly map = new Map<string, ViewEdge>();

  add(source: string, target: string, e: RawEdge) {
    const key = `${source}\u0000${target}\u0000${e.kind}`;
    const existing = this.map.get(key);
    if (existing) {
      existing.count += e.count;
      if (!e.ambiguous) {
        existing.ambiguous = undefined;
      }
    } else {
      this.map.set(key, {
        id: `a${this.map.size}`,
        source,
        target,
        kind: e.kind,
        count: e.count,
        ambiguous: e.ambiguous || undefined,
      });
    }
  }

  edges(): ViewEdge[] {
    return [...this.map.values()];
  }
}

/** Keeps the `maxNodes` most connected nodes. Expects a flat list (groups are added afterwards). */
function truncate(title: string, nodes: ViewNode[], edges: ViewEdge[], maxNodes: number): View {
  if (nodes.length <= maxNodes) {
    return { title, nodes, edges };
  }
  const weight = new Map<string, number>();
  for (const e of edges) {
    weight.set(e.source, (weight.get(e.source) ?? 0) + e.count);
    weight.set(e.target, (weight.get(e.target) ?? 0) + e.count);
  }
  const keep = new Set(
    [...nodes]
      .sort((a, b) => (weight.get(b.id) ?? 0) - (weight.get(a.id) ?? 0) || a.id.localeCompare(b.id))
      .slice(0, maxNodes)
      .map((n) => n.id),
  );
  return {
    title,
    nodes: nodes.filter((n) => keep.has(n.id)),
    edges: edges.filter((e) => keep.has(e.source) && keep.has(e.target)),
    truncated: { shown: keep.size, total: nodes.length },
  };
}

/**
 * Nested directory groups for file nodes (sets their `parent`). Only directories that directly
 * hold a file get a group; each group nests in its nearest grouped ancestor and is labelled
 * relative to it, so `tests/fixtures/poly/py/app` under `tests` reads `fixtures/poly/py/app`.
 * Top-level files stay ungrouped.
 */
function directoryGroups(nodes: ViewNode[]): ViewNode[] {
  const files = nodes.filter((n) => n.kind === 'file' && n.path);
  const dirs = new Set<string>();
  for (const f of files) {
    const d = dirname(f.path!);
    if (d !== '.') {
      dirs.add(d);
    }
  }
  const nearestGroup = (path: string): string | undefined => {
    for (let p = dirname(path); p !== '.'; p = dirname(p)) {
      if (dirs.has(p)) {
        return p;
      }
    }
    return undefined;
  };

  const groups = new Map<string, ViewNode>();
  for (const d of dirs) {
    const parent = nearestGroup(d);
    groups.set(d, {
      id: 'dir:' + d,
      label: parent ? d.slice(parent.length + 1) : d,
      kind: 'dir',
      path: d,
      parent: parent ? 'dir:' + parent : undefined,
      tooltip: d,
      dim: true,
    });
  }
  for (const f of files) {
    const d = dirname(f.path!);
    if (d === '.') {
      continue;
    }
    f.parent = 'dir:' + d;
    // A group is dimmed only when everything inside it is context.
    if (!f.dim) {
      for (let g: string | undefined = d; g && groups.get(g)!.dim; g = nearestGroup(g)) {
        groups.get(g)!.dim = false;
      }
    }
  }
  return [...groups.values()];
}

function externalNode(n: RawNode): ViewNode {
  return { id: n.id, label: n.name, kind: 'external', ref: n.id, tooltip: `external: ${n.name}` };
}

function nodeTooltip(n: RawNode): string {
  const loc = n.file ? `${n.file}:${n.line}` : '';
  return [n.id, n.signature, loc].filter(Boolean).join('\n');
}

export function dirname(file: string): string {
  const i = file.lastIndexOf('/');
  return i <= 0 ? '.' : file.slice(0, i);
}

/** First `depth` components of the file's directory (`.` for top-level files). */
export function dirPrefix(file: string, depth: number): string {
  const dir = dirname(file);
  if (dir === '.') {
    return '.';
  }
  return dir.split('/').slice(0, depth).join('/');
}
