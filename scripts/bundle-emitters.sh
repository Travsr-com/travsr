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
#   travsr-lib/travsr-lsif-ts        single-file bundle, no runtime deps
#   travsr-lib/travsr-lsif-py        single-file bundle, native deps external
#   travsr-lib/node_modules/...      tree-sitter + tree-sitter-python prebuilds
#
# The Python emitter keeps its two native addons external because node-gyp-build
# resolves the prebuild at runtime from the package directory, which a bundler
# cannot see. Only the prebuild matching $NODE_PLATFORM is kept, so the payload
# stays near 1 MB rather than carrying all six.
#
# Usage: scripts/bundle-emitters.sh <node-platform> <out-dir>
#   node-platform: prebuild directory name, e.g. darwin-arm64, linux-x64
set -euo pipefail

NODE_PLATFORM="${1:?usage: bundle-emitters.sh <node-platform> <out-dir>}"
OUT_DIR="${2:?usage: bundle-emitters.sh <node-platform> <out-dir>}"

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
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
build_one travsr-lsif-py travsr-lsif-py \
  --external:tree-sitter --external:tree-sitter-python

# Node resolves these from travsr-lib/node_modules because the bundle sits in
# travsr-lib/. Only the four packages the Python emitter loads at runtime are
# copied; the rest of its node_modules is build-time only.
py_modules="${repo_root}/packages/travsr-lsif-py/node_modules"
mkdir -p "${lib_dir}/node_modules"
for dep in tree-sitter tree-sitter-python node-gyp-build; do
  cp -R "${py_modules}/${dep}" "${lib_dir}/node_modules/${dep}"
done

# Drop every prebuild except this target's, and the sources node-gyp would need
# only if it had to compile, which it never does when the prebuild is present.
for dep in tree-sitter tree-sitter-python; do
  prebuilds="${lib_dir}/node_modules/${dep}/prebuilds"
  [ -d "${prebuilds}" ] || continue
  for d in "${prebuilds}"/*; do
    [ "$(basename "$d")" = "${NODE_PLATFORM}" ] || rm -rf "$d"
  done
  if [ ! -d "${prebuilds}/${NODE_PLATFORM}" ]; then
    echo "ERROR: ${dep} ships no prebuild for ${NODE_PLATFORM}" >&2
    exit 1
  fi
  rm -rf "${lib_dir}/node_modules/${dep}/src" \
         "${lib_dir}/node_modules/${dep}/build" \
         "${lib_dir}/node_modules/${dep}/vendor"
done

echo "==> staged $(du -sh "${lib_dir}" | cut -f1) in ${lib_dir}"
