import assert from 'node:assert/strict';
import { test } from 'node:test';
import { CodeGraph, dirPrefix, EDGE_KINDS, GraphError, RawGraph, ViewParams } from '../src/graphModel';

// Two directories: src/api (server.rs -> handler) and src/db (store.rs), plus an external crate.
function sample(): RawGraph {
  return {
    version: 1,
    root: '/repo',
    indexed_at: 1_700_000_000,
    nodes: [
      { id: 'src/api/server.rs', kind: 'file', name: 'server.rs', file: 'src/api/server.rs', line: 1, end_line: 50 },
      { id: 'src/api/server.rs::Server', kind: 'struct', name: 'Server', file: 'src/api/server.rs', line: 3, end_line: 40, parent: 0 },
      { id: 'src/api/server.rs::Server::run', kind: 'method', name: 'run', file: 'src/api/server.rs', line: 10, end_line: 20, parent: 1 },
      { id: 'src/db/store.rs', kind: 'file', name: 'store.rs', file: 'src/db/store.rs', line: 1, end_line: 30 },
      { id: 'src/db/store.rs::Store', kind: 'struct', name: 'Store', file: 'src/db/store.rs', line: 2, end_line: 25, parent: 3 },
      { id: 'src/db/store.rs::Store::get', kind: 'method', name: 'get', file: 'src/db/store.rs', line: 5, end_line: 9, parent: 4 },
      { id: 'ext:serde', kind: 'external', name: 'serde', file: '', line: 0, end_line: 0 },
      { id: 'main.rs', kind: 'file', name: 'main.rs', file: 'main.rs', line: 1, end_line: 5 },
    ],
    edges: [
      { from: 0, to: 1, kind: 'contains', count: 1, line: 3 },
      { from: 1, to: 2, kind: 'contains', count: 1, line: 10 },
      { from: 3, to: 4, kind: 'contains', count: 1, line: 2 },
      { from: 4, to: 5, kind: 'contains', count: 1, line: 5 },
      { from: 0, to: 3, kind: 'imports', count: 1, line: 1 },
      { from: 2, to: 5, kind: 'calls', count: 3, line: 12 },
      { from: 1, to: 4, kind: 'references', count: 1, line: 4, ambiguous: true },
      { from: 3, to: 6, kind: 'imports', count: 1, line: 1 },
      { from: 7, to: 0, kind: 'imports', count: 1, line: 1 },
    ],
  };
}

const all = (over: Partial<ViewParams>): ViewParams => ({
  mode: 'modules',
  edgeKinds: [...EDGE_KINDS],
  showExternal: false,
  ...over,
});

test('parse rejects bad input and unsupported versions', () => {
  assert.throws(() => CodeGraph.parse('{'), GraphError);
  assert.throws(() => CodeGraph.parse('{"version":1}'), GraphError);
  assert.throws(() => CodeGraph.parse(JSON.stringify({ ...sample(), version: 3 })), /version 3/);
  assert.doesNotThrow(() => CodeGraph.parse(JSON.stringify({ ...sample(), version: 2 })));
  assert.equal(CodeGraph.parse(JSON.stringify(sample())).size.files, 3);
});

test('dirPrefix', () => {
  assert.equal(dirPrefix('main.rs', 2), '.');
  assert.equal(dirPrefix('src/a/b/c.rs', 2), 'src/a');
  assert.equal(dirPrefix('src/a/b/c.rs', 1), 'src');
  assert.equal(dirPrefix('src/c.rs', 3), 'src');
});

test('symbolAt picks the innermost symbol, then the file', () => {
  const g = new CodeGraph(sample());
  assert.equal(g.symbolAt('src/api/server.rs', 12)?.id, 'src/api/server.rs::Server::run');
  assert.equal(g.symbolAt('src/api/server.rs', 30)?.id, 'src/api/server.rs::Server');
  assert.equal(g.symbolAt('src/api/server.rs', 45)?.id, 'src/api/server.rs');
  assert.equal(g.symbolAt('nope.rs', 1), undefined);
});

test('search ranks exact, prefix, substring, id', () => {
  const g = new CodeGraph(sample());
  // Name prefix matches come before matches on the id only (Server::run).
  const names = g.search('serv').map((n) => n.name);
  assert.deepEqual(names.slice(0, 2).sort(), ['Server', 'server.rs']);
  assert.equal(names[names.length - 1], 'run');
  assert.equal(g.search('store')[0].id, 'src/db/store.rs::Store');
  assert.deepEqual(g.search('  '), []);
});

test('module view aggregates edges between directories', () => {
  const g = new CodeGraph(sample());
  const v = g.build(all({ mode: 'modules', depth: 2 }), 100);
  assert.deepEqual(v.nodes.map((n) => n.id).sort(), ['dir:.', 'dir:src/api', 'dir:src/db']);
  const pairs = v.edges.map((e) => `${e.source}>${e.target}:${e.kind}:${e.count}`).sort();
  assert.deepEqual(pairs, [
    'dir:.>dir:src/api:imports:1',
    'dir:src/api>dir:src/db:calls:3',
    'dir:src/api>dir:src/db:imports:1',
    'dir:src/api>dir:src/db:references:1',
  ]);
  // The references edge is ambiguous, the others are not.
  assert.equal(v.edges.find((e) => e.kind === 'references')?.ambiguous, true);

  const withExt = g.build(all({ mode: 'modules', depth: 2, showExternal: true }), 100);
  assert.ok(withExt.edges.some((e) => e.source === 'dir:src/db' && e.target === 'ext:serde'));
});

test('file view nests files in directories and dims out-of-scope neighbours', () => {
  const g = new CodeGraph(sample());
  const v = g.build(all({ mode: 'files', dir: 'src/db' }), 100);
  const byId = new Map(v.nodes.map((n) => [n.id, n]));
  assert.equal(byId.get('src/db/store.rs')?.parent, 'dir:src/db');
  assert.equal(byId.get('src/db/store.rs')?.dim, false);
  assert.equal(byId.get('src/api/server.rs')?.dim, true);
  assert.equal(byId.has('main.rs'), false, 'main.rs is not a neighbour of src/db');
  assert.equal(v.title, 'Files in src/db');
});

test('file view nests directory groups and labels them relative to the parent group', () => {
  const raw = sample();
  raw.nodes.push(
    { id: 'src/a.rs', kind: 'file', name: 'a.rs', file: 'src/a.rs', line: 1, end_line: 1 },
    { id: 'src/x/y/z/deep.rs', kind: 'file', name: 'deep.rs', file: 'src/x/y/z/deep.rs', line: 1, end_line: 1 },
  );
  const v = new CodeGraph(raw).build(all({ mode: 'files' }), 100);
  const byId = new Map(v.nodes.map((n) => [n.id, n]));
  assert.equal(byId.get('dir:src')?.parent, undefined);
  assert.equal(byId.get('dir:src/api')?.parent, 'dir:src');
  assert.equal(byId.get('dir:src/api')?.label, 'api');
  // Intermediate directories without files get no group.
  assert.equal(byId.has('dir:src/x'), false);
  assert.equal(byId.get('dir:src/x/y/z')?.label, 'x/y/z');
  assert.equal(byId.get('dir:src/x/y/z')?.path, 'src/x/y/z');
  // Top-level files are not grouped.
  assert.equal(byId.get('main.rs')?.parent, undefined);
});

test('focus view walks the neighbourhood and nests symbols in files', () => {
  const g = new CodeGraph(sample());
  const v = g.build(all({ mode: 'focus', center: 'src/api/server.rs::Server::run', depth: 1 }), 100);
  const ids = v.nodes.map((n) => n.id).sort();
  // run: contained by Server, calls Store::get. Files appear as groups.
  assert.deepEqual(ids, [
    'src/api/server.rs',
    'src/api/server.rs::Server',
    'src/api/server.rs::Server::run',
    'src/db/store.rs',
    'src/db/store.rs::Store::get',
  ]);
  assert.equal(v.nodes.find((n) => n.focus)?.id, 'src/api/server.rs::Server::run');
  assert.equal(v.nodes.find((n) => n.id === 'src/db/store.rs::Store::get')?.parent, 'src/db/store.rs');

  const incoming = g.build(
    all({ mode: 'focus', center: 'src/db/store.rs::Store::get', direction: 'in', edgeKinds: ['calls'] }),
    100,
  );
  assert.deepEqual(incoming.edges.map((e) => `${e.source}>${e.target}`), [
    'src/api/server.rs::Server::run>src/db/store.rs::Store::get',
  ]);
});

test('focus view does not follow contains beyond the center', () => {
  const g = new CodeGraph(sample());
  // From Server::run at depth 2: Server is reached via contains, but its other children are not
  // pulled in through it, and the file is only a group.
  const v = g.build(all({ mode: 'focus', center: 'src/api/server.rs::Server::run', depth: 2 }), 100);
  assert.ok(!v.nodes.some((n) => n.id === 'ext:serde'));
  // Store is reached from Server via references at depth 2.
  assert.ok(v.nodes.some((n) => n.id === 'src/db/store.rs::Store'));
});

test('focus view truncates at maxNodes and reports it', () => {
  const g = new CodeGraph(sample());
  const v = g.build(all({ mode: 'focus', center: 'src/api/server.rs', depth: 3 }), 2);
  assert.ok(v.truncated);
  assert.equal(v.truncated!.shown, 2);
  assert.ok(v.truncated!.total > 2);
});

test('module view truncation keeps the most connected nodes', () => {
  const g = new CodeGraph(sample());
  const v = g.build(all({ mode: 'modules', depth: 2 }), 2);
  assert.deepEqual(v.nodes.map((n) => n.id).sort(), ['dir:src/api', 'dir:src/db']);
  assert.deepEqual(v.truncated, { shown: 2, total: 3 });
});

test('unknown focus center gives an empty view', () => {
  const g = new CodeGraph(sample());
  assert.equal(g.build(all({ mode: 'focus', center: 'missing' }), 10).nodes.length, 0);
});

test('details counts relations by kind', () => {
  const g = new CodeGraph(sample());
  const d = g.details('src/db/store.rs::Store::get')!;
  assert.deepEqual(d.incoming, { contains: 1, calls: 1 });
  assert.deepEqual(d.outgoing, {});
});
