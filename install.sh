#!/bin/sh
# travsr installer: downloads, verifies, and installs the travsr CLI binary.
# Usage: curl -fsSL https://travsr.com/install.sh | sh -s -- [--system] [--print-target] [--help]
# Repo:  https://github.com/Travsr-com/travsr
set -eu

REPO="Travsr-com/travsr"
SYSTEM_DIR="/usr/local/bin"
DEFAULT_DIR="${TRAVSR_INSTALL_DIR:-${HOME:-}/.local/bin}"

info() {
  printf 'travsr: %s\n' "$1"
}

warn() {
  printf 'travsr: warning: %s\n' "$1" >&2
}

err() {
  printf 'travsr: error: %s\n' "$1" >&2
  exit 1
}

usage() {
  cat <<EOF
Usage: curl -fsSL https://travsr.com/install.sh | sh -s -- [OPTIONS]

Installs the travsr CLI binary for this platform.

Options:
  --version TAG   Install a specific release instead of the latest stable,
                   e.g. --version v0.11.0. Must be an existing release tag.
  --system        Install to ${SYSTEM_DIR} instead of the default user
                   directory. Escalates with sudo unless already running
                   as root.
  --print-target  Print the detected target triple and exit. Does not
                   touch the network.
  -h, --help      Show this help and exit.

Environment:
  TRAVSR_INSTALL_DIR  Overrides the default install directory
                       (default: \$HOME/.local/bin).

A piped script cannot take flags directly; use the "sh -s --" form to
forward them, for example:
  curl -fsSL https://travsr.com/install.sh | sh -s -- --system
EOF
}

# Normalises uname output and resolves the release artifact triple for this
# platform. Sets $target. // O(1)
detect_target() {
  os=$(uname -s)
  arch=$(uname -m)

  case "$os" in
    Linux) os=linux ;;
    Darwin) os=darwin ;;
  esac

  case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
  esac

  # BEGIN TARGET_MAP
  # POSIX targets only. Must stay in lockstep with the release build matrix
  # (.github/workflows/release.yml) minus the Windows artifact, the npm TARGETS
  # map, and the vscode TARGET_MAP (#691); checked in CI by
  # .github/scripts/check-target-maps.mjs. Never add a *-windows-* triple here.
  case "${os}_${arch}" in
    linux_x86_64) target='x86_64-unknown-linux-gnu' ;;
    linux_aarch64) target='aarch64-unknown-linux-gnu' ;;
    darwin_x86_64) target='x86_64-apple-darwin' ;;
    darwin_aarch64) target='aarch64-apple-darwin' ;;
    *)
      err "Travsr does not yet ship a prebuilt binary for ${os}/${arch}. Build from source: https://github.com/${REPO}"
      ;;
  esac
  # END TARGET_MAP
}

# Resolves $tag: the explicit --version if one was given, otherwise the
# "latest" release tag via the redirect target of the GitHub releases/latest
# URL. Validates it looks like a real tag either way, and rejects any
# character that could escape a path component once $tag is interpolated into
# a URL or an output filename. Sets $tag.
resolve_tag() {
  if [ -n "$version" ]; then
    tag="$version"
    case "$tag" in
      *[!A-Za-z0-9._-]*) err "invalid --version '${tag}': release tags contain only letters, digits, '.', '_' and '-'" ;;
      v[0-9]*) ;;
      *) err "invalid --version '${tag}': expected a release tag like v0.11.0" ;;
    esac
    return
  fi

  releases_url="https://github.com/${REPO}/releases"
  url=$(curl -fsSLI --proto '=https' --proto-redir '=https' -o /dev/null -w '%{url_effective}' "${releases_url}/latest")
  # --proto-redir pins the redirect scheme but not its host, and $tag is parsed
  # out of wherever the chain terminates; assert the host and path explicitly.
  case "$url" in
    "${releases_url}/tag/"*) ;;
    *) err "the latest-release redirect ended at an unexpected URL (${url}); refusing to parse a tag from it" ;;
  esac
  tag=${url##*/}
  case "$tag" in
    *[!A-Za-z0-9._-]*) err "resolved an unusable release tag ('${tag}') from ${url}" ;;
    v[0-9]*) ;;
    *) err "could not resolve the latest release tag (got '${tag}' from ${url}); the repository may have no releases yet" ;;
  esac
}

# Downloads the tarball and SHA256SUMS into $tmp, verifies the checksum
# unconditionally, then verifies the cosign signature when cosign is on
# PATH. Aborts on any verification failure; there is no bypass.
download_and_verify() {
  if ! curl -fsSL --proto '=https' --proto-redir '=https' -o "$tmp/${tarball_name}" "${base_url}/${tarball_name}"; then
    if [ -n "$version" ]; then
      err "could not download ${tarball_name}; check that release ${tag} exists and ships a build for this platform: https://github.com/${REPO}/releases/tag/${tag}"
    else
      err "could not download ${tarball_name} from the latest release (${tag})"
    fi
  fi
  curl -fsSL --proto '=https' --proto-redir '=https' -o "$tmp/SHA256SUMS" "${base_url}/SHA256SUMS" ||
    err "could not download SHA256SUMS for release ${tag}; refusing to install unverified"

  if command -v sha256sum >/dev/null 2>&1; then
    actual_hash=$(sha256sum "$tmp/${tarball_name}" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then
    actual_hash=$(shasum -a 256 "$tmp/${tarball_name}" | awk '{print $1}')
  else
    err "neither sha256sum nor shasum is available; cannot verify the release checksum, refusing to install"
  fi

  # SHA256SUMS path fields differ between the publish and promote jobs
  # ("dist/travsr-..." vs "travsr-..."), and Windows lines are prefixed with
  # "*". Normalise by stripping a leading '*' and everything up to the last
  # '/', then require an exact basename match.
  expected_hash=$(awk -v want="${tarball_name}" '
    {
      hash = $1
      p = $2
      sub(/^\*/, "", p)
      n = split(p, parts, "/")
      base = parts[n]
      if (base == want) { print hash; exit }
    }
  ' "$tmp/SHA256SUMS")

  [ -n "$expected_hash" ] || err "no SHA256SUMS entry found for ${tarball_name}; refusing to install"
  [ "$actual_hash" = "$expected_hash" ] || err "SHA256 mismatch for ${tarball_name}: expected ${expected_hash}, got ${actual_hash}"

  if command -v cosign >/dev/null 2>&1; then
    info "cosign found, verifying signature"
    bundle_name="${tarball_name}.bundle"
    curl -fsSL --proto '=https' --proto-redir '=https' -o "$tmp/${bundle_name}" "${base_url}/${bundle_name}" ||
      err "cosign is installed but ${bundle_name} could not be downloaded; refusing to install unverified"
    # The identity is pinned all the way through the ref, and the ref pattern
    # mirrors release.yml's `on.push.tags` exactly. Only the tag-triggered build
    # job ever signs (promote reuses those bundles), so a signature from a
    # branch, PR, or workflow_dispatch run of this same workflow must not pass.
    if ! cosign verify-blob --bundle "$tmp/${bundle_name}" \
      --certificate-oidc-issuer https://token.actions.githubusercontent.com \
      --certificate-identity-regexp '^https://github\.com/Travsr-com/travsr/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+(-(beta|rc)\.[0-9]+)?$' \
      "$tmp/${tarball_name}"; then
      err "cosign signature verification failed for ${tarball_name}; aborting install"
    fi
    info "cosign signature verified"
  else
    warn "cosign not found, skipping signature verification. Install cosign for stronger guarantees: https://docs.sigstore.dev/cosign/system_config/installation/"
  fi
}

# Copies the extracted, already-verified binary from $tmp into $dir via a
# staging name plus atomic rename, so an in-place upgrade never leaves a
# truncated binary on PATH. Escalates with sudo only for --system on a
# non-root user, and only after printing the exact commands being run.
# The mode is set on the staging file rather than on the $tmp copy: cp without
# -p applies the caller's umask to the destination, so a chmod in $tmp does not
# carry through and a hardened umask would otherwise install 0700 system-wide.
install_binary() {
  staging="${dir}/.travsr.install.$$"

  if [ "$use_system" = yes ] && [ "$(id -u)" != 0 ]; then
    command -v sudo >/dev/null 2>&1 || err "--system requires root or sudo, and sudo was not found on PATH"
    info "--system requested, elevating with sudo to run:"
    info "  sudo mkdir -p ${dir}"
    info "  sudo cp ${tmp}/travsr ${staging}"
    info "  sudo chmod 755 ${staging}"
    info "  sudo mv -f ${staging} ${dir}/travsr"
    sudo mkdir -p "$dir"
    sudo cp "$tmp/travsr" "$staging"
    sudo chmod 755 "$staging"
    sudo mv -f "$staging" "${dir}/travsr"
  else
    mkdir -p "$dir"
    cp "$tmp/travsr" "$staging"
    chmod 755 "$staging"
    mv -f "$staging" "${dir}/travsr"
  fi
}

main() {
  use_system=no
  print_target_only=no
  version=""

  while [ $# -gt 0 ]; do
    case "$1" in
      --version)
        [ $# -ge 2 ] || err "--version requires an argument, e.g. --version v0.11.0"
        # An empty value must fail rather than fall through to the latest-release
        # path: the realistic source is --version "$VAR" in automation with VAR
        # unset, and silently turning a pinned install into a floating one is the
        # exact failure mode pinning exists to prevent.
        [ -n "$2" ] || err "--version requires a non-empty release tag, e.g. --version v0.11.0"
        version="$2"
        shift
        ;;
      --system) use_system=yes ;;
      --print-target) print_target_only=yes ;;
      -h | --help)
        usage
        exit 0
        ;;
      *) err "unrecognised option: $1 (see --help)" ;;
    esac
    shift
  done

  detect_target

  if [ "$print_target_only" = yes ]; then
    printf '%s\n' "$target"
    exit 0
  fi

  if [ "$use_system" = yes ]; then
    dir="$SYSTEM_DIR"
  else
    dir="$DEFAULT_DIR"
    [ "$dir" != "/.local/bin" ] || err "HOME is not set and TRAVSR_INSTALL_DIR was not provided; cannot determine an install directory"
  fi

  # Preflight every external tool the install path needs, so a minimal image
  # fails here with a travsr: error rather than mid-run with a bare
  # "tar: not found" after the download and both verifications have happened.
  for tool in curl tar mktemp; do
    command -v "$tool" >/dev/null 2>&1 || err "${tool} is required to install travsr but was not found on PATH"
  done

  resolve_tag
  tarball_name="travsr-${tag}-${target}.tar.gz"
  base_url="https://github.com/${REPO}/releases/download/${tag}"

  tmp=$(mktemp -d)
  # A trap handler that does not exit resumes the script after the interrupted
  # command, so INT/TERM/HUP set an explicit status instead of falling through.
  trap 'rm -rf "$tmp"' EXIT
  trap 'rm -rf "$tmp"; exit 130' INT
  trap 'rm -rf "$tmp"; exit 143' TERM HUP

  download_and_verify

  tar -xzf "$tmp/${tarball_name}" -C "$tmp" travsr

  install_binary

  case ":${PATH}:" in
    *":${dir}:"*) ;;
    *) warn "${dir} is not on your PATH. Add this to your shell profile: export PATH=\"${dir}:\$PATH\"" ;;
  esac

  info "installed travsr to ${dir}/travsr"
}

main "$@"
