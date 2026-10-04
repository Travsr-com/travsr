#!/usr/bin/env bash
# Build the Node LSIF emitters into the layout the release tarball ships.
#
# TypeScript, JavaScript and Python declare native_phase_b in the Phase B
# catalog, but their emitters (travsr-lsif-ts, travsr-lsif-py) were never in a
# published artifact: the tarball held one file, and neither emitter is on npm.
# resolve_lsif_emitter/resolve_lsif_py_emitter therefore fell through to the
# bare-PATH step on every install that was not a monorepo checkout, so those
# three languages produced no cross-file edges while `lang list` reported them
# active. This script produces the payload that closes that gap.
#
# Output layout, staged under $OUT_DIR and unpacked beside the binary:
#
#   travsr-lib/travsr-lsif-ts          single-file bundle
#   travsr-lib/travsr-lsif-py          single-file bundle
#   travsr-lib/tree-sitter.wasm        tree-sitter runtime, loaded by the above
#   travsr-lib/tree-sitter-python.wasm Python grammar
#
# Nothing here is platform specific. The Python emitter parses through
# web-tree-sitter rather than the native tree-sitter addon, so the payload is
# the same bytes on every target: no prebuild to select, no libstdc++ or libc
# floor to clear, and it runs under a musl node as happily as a glibc one.
# (tree-sitter's own linux prebuilds need GLIBCXX_3.4.31, which Ubuntu 22.04,
# Debian 12 and RHEL 9 do not have, and it ships no musl build at all.)
#
# Staged payload is ~10 MB, 1.7 MB gzipped. 9.5 MB of that is travsr-lsif-ts,
# which inlines the TypeScript compiler; the Python side is 866 KB all in.
#
# Usage: scripts/bundle-emitters.sh <out-dir>
set -euo pipefail

OUT_DIR="${1:?usage: bundle-emitters.sh <out-dir>}"

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
# build_one cds into each package, so every path handed to esbuild has to be
# absolute; release.yml passes a relative "dist".
mkdir -p "${OUT_DIR}"
OUT_DIR="$(cd "${OUT_DIR}" && pwd)"
lib_dir="${OUT_DIR}/travsr-lib"
mkdir -p "${lib_dir}"

# The emitters are external programs the indexer spawns, so each needs its own
# shebang and exec bit. Both dist/index.js files already carry the shebang;
# esbuild preserves it, which is why no --banner is passed (a second one would
# land on line 2 and is a syntax error).
build_one() {
  pkg="$1"
  out_name="$2"
  shift 2

  echo "==> ${pkg}"
  npm ci --prefix "${repo_root}/packages/${pkg}"
  npm run build --prefix "${repo_root}/packages/${pkg}"
  (
    cd "${repo_root}/packages/${pkg}"
    npx --no-install esbuild dist/index.js \
      --bundle \
      --platform=node \
      --target=node18 \
      --outfile="${lib_dir}/${out_name}" \
      --log-level=warning \
      "$@"
  )
  chmod +x "${lib_dir}/${out_name}"
}

build_one travsr-lsif-ts travsr-lsif-ts
build_one travsr-lsif-py travsr-lsif-py

# The Python bundle loads both .wasm files from its own directory, so they ride
# beside it. `npm run build` already placed them in dist/ for the same reason.
for wasm in tree-sitter.wasm tree-sitter-python.wasm; do
  cp "${repo_root}/packages/travsr-lsif-py/dist/${wasm}" "${lib_dir}/${wasm}"
done

# Smoke the bundles from the staging directory, which is the relocated layout a
# user gets: no node_modules to fall back through. A bundle that builds but
# cannot load is exactly the failure this script exists to prevent, and it is
# the one thing the release job's `test -f` cannot tell you. Each emitter's own
# fixture is reused rather than a new one invented here.
dump="$(mktemp)"
trap 'rm -f "${dump}"' EXIT

smoke() {
  name="$1"
  shift
  if ! node "${lib_dir}/${name}" "$@" > "${dump}" 2>&1; then
    echo "ERROR: ${name} failed to run from ${lib_dir}:" >&2
    head -5 "${dump}" >&2
    exit 1
  fi
  # Cross-file edges are the whole point of the emitter; a dump carrying only
  # the metaData and project vertices means it loaded but resolved nothing.
  if ! grep -q '"label":"referenceResult"' "${dump}"; then
    echo "ERROR: ${name} ran but emitted no reference edges" >&2
    exit 1
  fi
}
smoke travsr-lsif-py --root "${repo_root}/packages/travsr-lsif-py/fixtures/simple"
smoke travsr-lsif-ts --project "${repo_root}/packages/travsr-lsif-ts/fixtures/tsconfig.json"

echo "==> staged $(du -sh "${lib_dir}" | cut -f1) in ${lib_dir}"
