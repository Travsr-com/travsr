// Put the two grammar/runtime .wasm files beside the compiled emitter.
//
// walker.ts loads them from its own directory, which is the one layout that
// holds in both places the emitter runs: dist/ in this checkout, and
// travsr-lib/ beside the installed binary, where there is no node_modules to
// resolve through.
//
// This script is the only consumer of tree-sitter-python, and it only reads a
// file out of the package at build time. Nothing requires it at runtime, so it
// is a devDependency: as a regular dependency it put node-gyp-build,
// node-addon-api and six platforms' worth of prebuilt .node binaries into the
// production tree, which is what the web-tree-sitter switch existed to get rid
// of. CI asserts the --omit=dev tree stays addon-free, so moving this back
// fails the emitter bundle job rather than quietly undoing that.
import { copyFileSync } from 'node:fs';
import { basename, join } from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
for (const spec of ['web-tree-sitter/tree-sitter.wasm', 'tree-sitter-python/tree-sitter-python.wasm']) {
  copyFileSync(require.resolve(spec), join('dist', basename(spec)));
}
