# Security Policy

## Reporting a Vulnerability

**Please do not file a public GitHub issue for security vulnerabilities.**

Email **security@travsr.com** with:

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
- Kùzu (C++ native dependency) is included in the scan

### npm Dependencies
- `npm audit --audit-level=high` runs on every PR for `packages/travsr-lsif-ts`

## Scope

- `travsr` CLI binary and all crates in this workspace
- The npm wrapper (`packages/travsr-npm`)
- The git hook installed by `travsr init`

## Out of Scope

- Vulnerabilities in user repositories indexed by Travsr
- Issues in third-party dependencies (report those upstream)
