import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { test } from 'node:test';
import { cnavToJson, WASM_FILE } from '../src/cnavFile';
import { CodeGraph, GraphError } from '../src/graphModel';

const repo = path.join(__dirname, '..', '..', '..');
// Needs the decoder (`npm run build:wasm`, which needs Rust) and cargo to write a graph.
const noDecoder = fs.existsSync(WASM_FILE) ? false : 'no wasm decoder: run `npm run build:wasm`';

test('decodes a graph.cnav written by codenav', { skip: noDecoder }, () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'codenav-ext-'));
  try {
    fs.writeFileSync(path.join(dir, 'a.py'), 'def f():\n    g()\n\ndef g():\n    pass\n');
    execFileSync('cargo', ['run', '--quiet', '--', 'index', dir], { cwd: repo, stdio: 'ignore' });
    const bytes = fs.readFileSync(path.join(dir, '.codenav', 'graph.cnav'));
    assert.equal(bytes.subarray(0, 4).toString(), 'CNAV');
    const graph = CodeGraph.parse(cnavToJson(bytes));
    assert.ok(graph.nodes.some((n) => n.id === 'a.py::g'));
    assert.ok(graph.edges.some((e) => e.kind === 'calls'));
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('passes older JSON files through', () => {
  const json = '{"version":3,"root":"/r","indexed_at":0,"nodes":[],"edges":[]}';
  assert.equal(cnavToJson(Buffer.from(json)), json);
  assert.equal(cnavToJson(Buffer.from('\uFEFF\n  ' + json)), '\n  ' + json);
});

test('reports unreadable files', { skip: noDecoder }, () => {
  assert.throws(() => cnavToJson(Buffer.from('CNAV\u0004\u0000\u0000\u0000garbage')), GraphError);
  assert.throws(() => cnavToJson(Buffer.from('not a graph')), /not a codenav graph/);
});
