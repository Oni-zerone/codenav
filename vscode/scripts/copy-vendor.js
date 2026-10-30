// Copies the browser builds of cytoscape and the fcose layout next to the webview assets,
// so the packaged extension does not need node_modules at runtime.
const fs = require('fs');
const path = require('path');

const files = [
  'cytoscape/dist/cytoscape.min.js',
  // fcose's UMD build expects layout-base and cose-base as globals, loaded in this order.
  'layout-base/layout-base.js',
  'cose-base/cose-base.js',
  'cytoscape-fcose/cytoscape-fcose.js',
];
const destDir = path.join(__dirname, '..', 'media', 'vendor');
fs.mkdirSync(destDir, { recursive: true });
for (const file of files) {
  fs.copyFileSync(require.resolve(file), path.join(destDir, path.basename(file)));
}
