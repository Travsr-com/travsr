# Security Policy

## Reporting a Vulnerability

**Please do not file a public GitHub issue for security vulnerabilities.**

Email **connect@travsr.com** with:

- A description of the vulnerability
- Steps to reproduce
- Potential impact
- Any suggested mitigations (optional)

You will receive an acknowledgement within 48 hours and a timeline for the fix within 7 days.

## Disclosure Timeline

We follow a **90-day responsible disclosure** policy:

1. You report the vulnerability privately.
2. We confirm receipt within 48 hours.
3. We investigate and develop a fix (target: ≤ 30 days for critical issues).
4. We coordinate a release date with you.
5. We publish a security advisory after the fix is released.
6. At day 90, you may disclose publicly regardless of fix status.

## CVE Response SLA

| Severity | CVSS Score | Patch SLA    |
|----------|-----------|--------------|
| Critical | >= 9.0    | **7 days**   |
| High     | 7.0–8.9   | **30 days**  |
| Medium   | 4.0–6.9   | Next release |
| Low      | < 4.0     | Best effort  |

## Supply Chain Security

### Rust Dependencies
- `cargo-deny check advisories licenses bans` runs on every PR via CI
- Denies packages with known CVEs (`vulnerability = "deny"` in `deny.toml`)
- Denies copyleft licenses (GPL-2.0, GPL-3.0, AGPL-3.0, LGPL-2.0, LGPL-2.1)
- See `deny.toml` at the repository root for the full policy

### CVE Monitoring
- [OSV Scanner](https://google.github.io/osv-scanner/) runs nightly against `Cargo.lock`
  and `packages/travsr-lsif-ts/package-lock.json` (`.github/workflows/osv-scan.yml`)

### npm Dependencies
- `npm audit --audit-level=high` runs on every PR for `packages/travsr-lsif-ts`

## Scope

- `travsr` CLI binary and all crates in this workspace
- The npm wrapper (`packages/travsr-npm`)
- The git hook installed by `travsr init`

## Out of Scope

- Vulnerabilities in user repositories indexed by Travsr
- Issues in third-party dependencies (report those upstream)

## Release Artifact Signing

Every release tarball is signed using **cosign keyless signing** via the
[Sigstore](https://sigstore.dev) public-good transparency log. The signer
identity is the GitHub Actions OIDC token issued for this repository's
`release.yml` workflow.

### Verify a release tarball

```sh
# Install cosign: https://docs.sigstore.dev/cosign/system_config/installation/
# Works the same for any channel: stable (1.0.0), rc (1.0.0-rc.1), or beta (1.0.0-beta.2).
VERSION=1.0.0
TARGET=x86_64-unknown-linux-gnu

# Download tarball and bundle from the GitHub release
curl -LO "https://github.com/Travsr-com/travsr/releases/download/v${VERSION}/travsr-v${VERSION}-${TARGET}.tar.gz"
curl -LO "https://github.com/Travsr-com/travsr/releases/download/v${VERSION}/travsr-v${VERSION}-${TARGET}.tar.gz.bundle"

# Verify
cosign verify-blob \
  --bundle "travsr-v${VERSION}-${TARGET}.tar.gz.bundle" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  --certificate-identity-regexp '^https://github\.com/Travsr-com/travsr/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+(-(beta|rc)\.[0-9]+)?$' \
  "travsr-v${VERSION}-${TARGET}.tar.gz"
```

### SLSA Provenance

Each release also carries a **SLSA v1.0 build provenance attestation** attached
to the GitHub release via `actions/attest-build-provenance`. Verify with:

```sh
gh attestation verify "travsr-v${VERSION}-${TARGET}.tar.gz" \
  --repo Travsr-com/travsr
```

### npm postinstall

The `@travsr.com/travsr` npm package automatically verifies the cosign signature
during `npm install` if `cosign` is installed on the host. If cosign is not
present, installation falls back to SHA256 verification only with a warning.
This applies to every channel — `npm i -g @travsr.com/travsr@beta` and `@rc`
verify identically to `@latest`, since beta/rc/stable reuse the same signed
artifact rather than each being signed separately.

### Shell installer (install.sh)

`curl -fsSL https://travsr.com/install.sh | sh` verifies the downloaded
tarball's SHA256 against the release `SHA256SUMS` unconditionally; this check
has no bypass flag or environment variable and a mismatch aborts the install
with nothing written. If `cosign` is on `PATH`, the script also verifies the
release's cosign signature, and unlike the npm postinstall behavior described
above, a verification failure here aborts the install rather than falling
back to SHA256-only. `install.sh` itself is served over TLS and its integrity
relies on GitHub release integrity, but it is not part of `SHA256SUMS` and is
not cosign-signed, so anyone who wants to inspect it before running should
`curl -fsSL <url> | less` first. `--system` is the only flag that escalates
privileges, and it does so via `sudo` only after printing each privileged
command verbatim, exactly as it is about to run them.
