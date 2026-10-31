// Reads `.cnav` files. Since format v4 they are binary (bincode): they are decoded by
// codenav's own Rust code compiled to WebAssembly (`../wasm`, copied to `wasm/` by
// `npm run build:wasm`), so the extension never has to mirror the Rust types' layout.
// Files from older codenav versions are JSON and are passed through.
import * as fs from 'fs';
import * as path from 'path';
import { GraphError } from './graphModel';

// The parts of the WebAssembly API used here (the project's `lib` has no declarations).
declare const WebAssembly: {
  Module: new (bytes: Uint8Array) => object;
  Instance: new (module: object, imports: object) => { exports: Decoder };
};

interface Decoder {
  memory: { buffer: ArrayBuffer };
  alloc(len: number): number;
  dealloc(ptr: number, len: number): void;
  decode(ptr: number, len: number): bigint;
}

export const WASM_FILE = path.join(__dirname, '..', '..', 'wasm', 'codenav_wasm.wasm');

let compiled: object | undefined;

/** A fresh instance per file: its memory (input and JSON together) is freed with it. */
function instantiate(): Decoder {
  if (!compiled) {
    let bytes: Buffer;
    try {
      bytes = fs.readFileSync(WASM_FILE);
    } catch {
      throw new GraphError(`The .cnav decoder is missing (${WASM_FILE}); build it with \`npm run build:wasm\`.`);
    }
    compiled = new WebAssembly.Module(bytes);
  }
  return new WebAssembly.Instance(compiled, {}).exports as Decoder;
}

/** JSON from older codenav versions: `{` first, after whitespace or a byte order mark. */
function isJson(bytes: Uint8Array): boolean {
  let i = bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf ? 3 : 0;
  while (i < bytes.length && [0x20, 0x09, 0x0a, 0x0d].includes(bytes[i])) {
    i++;
  }
  return bytes[i] === 0x7b;
}

/** The graph in the contents of a `.cnav` file, as JSON text (for `CodeGraph.parse`). */
export function cnavToJson(bytes: Uint8Array): string {
  if (isJson(bytes)) {
    return Buffer.from(bytes).toString('utf8').replace(/^\uFEFF/, '');
  }
  let text: string;
  try {
    const wasm = instantiate();
    const input = wasm.alloc(bytes.length);
    new Uint8Array(wasm.memory.buffer, input, bytes.length).set(bytes);
    // `decode` frees the input; the result is (address << 32) | length.
    const packed = BigInt.asUintN(64, wasm.decode(input, bytes.length));
    const ptr = Number(packed >> 32n);
    const len = Number(packed & 0xffff_ffffn);
    text = Buffer.from(new Uint8Array(wasm.memory.buffer, ptr, len)).toString('utf8');
  } catch (err) {
    if (err instanceof GraphError) {
      throw err;
    }
    throw new GraphError(`Cannot read the code graph: the decoder failed (${(err as Error).message})`);
  }
  if (text.startsWith('{"error":')) {
    throw new GraphError(`Cannot read the code graph: ${(JSON.parse(text) as { error: string }).error}`);
  }
  return text;
}
