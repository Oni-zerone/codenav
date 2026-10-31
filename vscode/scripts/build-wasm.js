// Builds the `.cnav` decoder (the Rust crate in ../wasm, codenav's own graph code) for
// WebAssembly and copies it to wasm/, which is packaged with the extension.
const { execFileSync } = require('child_process');
const fs = require('fs');
const path = require('path');

const repo = path.join(__dirname, '..', '..');
execFileSync('cargo', ['build', '-p', 'codenav-wasm', '--release', '--target', 'wasm32-unknown-unknown'], {
  cwd: repo,
  stdio: 'inherit',
});
const built = path.join(repo, 'target', 'wasm32-unknown-unknown', 'release', 'codenav_wasm.wasm');
const destDir = path.join(__dirname, '..', 'wasm');
fs.mkdirSync(destDir, { recursive: true });
fs.copyFileSync(built, path.join(destDir, 'codenav_wasm.wasm'));
