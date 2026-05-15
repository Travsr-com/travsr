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

## Scope

- `travsr` CLI binary and all crates in this workspace
- The npm wrapper (`packages/travsr-npm`)
- The git hook installed by `travsr init`

## Out of Scope

- Vulnerabilities in user repositories indexed by Travsr
- Issues in third-party dependencies (report those upstream)
