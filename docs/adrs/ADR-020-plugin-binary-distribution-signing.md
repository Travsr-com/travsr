# ADR-020: Plugin Binary Distribution & Signing

**Date:** 2026-06-04
**Status:** Proposed
**Phase:** Cross-cutting (RFC-013 Phase 5 — gates migrating any language to a shipped sidecar)
**Author:** Principal Security Engineer (trust root) + DevOps Engineer (delivery)
**Extends:** ADR-017 (sandboxes *execution*; explicitly defers plugin *authenticity* to "a Phase 5 ADR" — **this is that ADR**)
**Related:** RFC-013 (runtime language pluggability — D2/D8), ADR-019 (signature verified during AUTHORIZE), RFC-011 (two-transport architecture), CLAUDE.md §Non-Negotiable Principles (#3 local-first, #5 OCI free tier, #6 ARM64), DevOps release/signing bar (sigstore/cosign, SLSA)

---

## Context

RFC-013 ships every non-builtin language as a standalone `travsr-lang-*` binary
(Direction A) that links its own Tree-sitter grammar and runs as a sandboxed
sidecar. These binaries are distributed **independently of the core `travsr`
release** and execute against the user's private source code.

ADR-017 deliberately scoped itself to sandboxing *execution* and left plugin
**authenticity** open:

> "This ADR sandboxes *execution*. Plugin *authenticity* (signing, a registry trust
> root for community plugins) is **out of scope** and deferred to a Phase 5 ADR …
> Until then, `--command` plugins are a local, explicit, per-corpus opt-in only."

RFC-013's subscription flow (ADR-019) introduces an **AUTHORIZE** step that must
decide whether a binary is allowed to run *at all*, before the verification probe
executes it. AUTHORIZE needs an authenticity signal — "is this the binary the
language's maintainer actually published, or a tampered/substituted one?" — that
the sandbox (which only contains the blast radius) does not provide.

The threat is concrete (RFC-013 D8 / global T11 lineage): a compromised or
malicious `travsr-lang-x` indexes the user's private code, or exfiltrates it the
moment a sandbox rule has a gap. Sandboxing reduces blast radius; it does not
establish that the binary is the legitimate one.

The core release bar already mandates sigstore/cosign signatures + SLSA provenance
(DevOps skill; global T3/T10). This ADR extends that bar to plugin binaries and
defines how AUTHORIZE consumes it.

This ADR decides:

1. The signing requirement for `travsr-lang-*` binaries.
2. Where the expected signing identity is pinned, and how AUTHORIZE verifies it.
3. The distribution matrix (channels, platforms — including ARM64).
4. The version / `PROTOCOL_VERSION` compatibility policy across the boundary.
5. The scope line between first-party plugins (now) and a community registry (deferred).

---

## Decision

### Rule 1 — Every `travsr-lang-*` binary is signed (no exceptions)

Every plugin binary release is **cosign-signed** with **SLSA build provenance**,
produced by CI (OIDC keyless signing), never on a developer laptop — identical to
the core release bar. An unsigned plugin binary is **not eligible** for AUTHORIZE
and is therefore never executed (fail-closed, inheriting ADR-017 Rule 2's posture).

The signed artifact set per release:
```
travsr-lang-<x>-<target>(.exe)        the binary
travsr-lang-<x>-<target>.sig          cosign signature
travsr-lang-<x>-<target>.intoto.jsonl SLSA provenance attestation
```

### Rule 2 — The expected signing identity is pinned in the catalog; AUTHORIZE verifies it

The Phase B catalog entry (`travsr-plugin-host/src/phase_b/catalog.rs`,
`PhaseBEntry`) — which RFC-013 D6 promotes into the runtime handshake declaration —
is extended with the **expected signing identity** (the OIDC subject + issuer the
binary must be signed by, e.g. the `travsr-lang` repo's GitHub Actions identity).

During **AUTHORIZE** (ADR-019 Rule 1), before any spawn:

1. Compute `sha256(binary)` (the same hash ADR-019 keys its cache on).
2. Verify the cosign signature against the **pinned** identity for that language.
3. Verify the SLSA provenance attests the expected source repo + builder.
4. On any failure → `Quarantined{Untrusted}`; the binary is **not executed** (so the
   ADR-019 probe never runs an unauthenticated binary).

This makes signature verification a **precondition of execution**, not a
post-hoc check — consistent with ADR-019's "untrusted code is not run to test it."
The verdict cache (ADR-019 T17) is advisory: a cached `Active` does **not** skip
this gate; signature + hash are re-checked at spawn.

### Rule 3 — Distribution matrix (channels & platforms)

Plugin binaries follow the core distribution matrix (DevOps), built in CI for every
supported target — **ARM64 is mandatory** (CLAUDE.md #6; OCI A1 is aarch64, and
Apple Silicon developers are first-class):

| Channel | Use | Notes |
|---|---|---|
| **GitHub Releases** (`travsr-lang` repo) | canonical source of signed binaries + `.sig` + provenance | per-target tarballs, mirrors the core release layout |
| **npm** (`@travsr-plugin/<x>`) | `travsr lang install <x>` path (already referenced by catalog `npm_package` / `scip_install`) | `postinstall` downloads the platform binary from GitHub Releases and **verifies the cosign signature before extraction** |
| **OCIR / object storage** | cloud-tier / OCI deployments | within the 500 MB OCIR / 20 GB object-storage free-tier limits; lean binaries |

Targets: `x86_64`/`aarch64` × `linux-gnu`/`apple-darwin`, plus `x86_64-pc-windows-msvc`
(Windows sidecars post-MVP, per ADR-018). Pushing an x86 image/binary to an A1
(aarch64) host runs under QEMU — forbidden by the ARM64 rule.

### Rule 4 — Version & `PROTOCOL_VERSION` compatibility policy

- **`PROTOCOL_VERSION` is the hard contract.** A plugin whose `protocol_version`
  the daemon does not support is refused at registration (ADR-017 Rule 5 / RFC-011
  §4) — a wire-contract mismatch must never be driven, lest it mis-decode into
  forged nodes/edges.
- **Bumping `PROTOCOL_VERSION` is a coordinated release** across `travsr-core` /
  `travsr-plugin-protocol` (the published boundary, RFC-013 D1) and every
  `travsr-lang-*` plugin, with a deprecation note (DevOps SemVer: a breaking
  protocol change is a MAJOR event with a 2-week deprecation notice).
- **`plugin_version`** is an ADR-019 cache-invalidation component and is recorded in
  graph metadata (ADR-017 Rule 5) so version drift is detectable via `travsr status`.
- The `BOUNDARY-CHANGE:` CI guard (RFC-013 D1) ensures protocol bumps are explicit
  and reviewed.

### Rule 5 — First-party now; community registry deferred (scope line)

This ADR governs **first-party** `travsr-lang-*` binaries published by the Travsr
org — pinned to the org's signing identity (Rule 2). A **community** plugin
ecosystem (third-party publishers, a registry trust root, identity delegation) is
**out of scope** and remains, per ADR-017, a local explicit per-corpus `--command`
opt-in only, gated behind a future ADR and the CTO's deferral of the community
SDK/registry. Until then:

- only org-signed `travsr-lang-*` binaries pass AUTHORIZE via the pinned identity;
- a user-supplied `--command` binary still runs (ADR-017 Rule 3 trust + sandbox) but
  carries **no authenticity guarantee** and is surfaced as such in `travsr languages`.

### Rule 6 — Phase B analyzers are shared, often third-party, and hash-pinned

RFC-013 D6a makes a Phase B **analyzer** (e.g. `scip-java`) a first-class shared
entity serving several languages. Analyzers split into two authenticity classes:

- **Org-republished analyzers** (a Travsr-built wrapper/redistribution) → treated
  exactly like a `travsr-lang-*` binary: cosign-signed, identity pinned (Rules 1–2).
- **Third-party analyzers** (`scip-java`, `rust-analyzer`, `scip-python`, … installed
  via their own ecosystems) → Travsr does **not** publish or sign these, so their
  authenticity rests on: a **catalog-pinned expected `sha256` (and/or upstream
  publisher identity where available)**, the ADR-019 verification probe, and the
  ADR-017 sandbox. Verified at AUTHORIZE against the pinned hash before spawn.

In **both** classes the analyzer is **hash-pinned and re-verified at spawn** — never
trusted by path — because RFC-013 D6a shares one analyzer across languages, so a
single tampered analyzer is a **concentrated blast radius** (poisons every language
it serves). A user-pointed analyzer path (the D6a manual override) is a
`--command`-class decision: it runs sandboxed but carries no authenticity guarantee
unless it matches a pinned hash/identity, and is surfaced as such.

---

## Threat model — rows touched / added (T-table)

> Numbering continues the global table; ADR-017 occupies T11–T13, ADR-018 T14–T16,
> ADR-019 T17–T19/T22. This ADR is the home of **T11**'s plugin-binary supply-chain
> facet (ADR-017 framed T11 as the `--command` case; here it gains the *signed
> distribution* mitigation for first-party plugins). T20/T21/T23 are this ADR's.

| Row | Asset | Threat | Likelihood | Impact | Mitigation | Δ |
|---|---|---|---|---|---|---|
| **T11 (extended)** | Developer machine / private source | A malicious or compromised `travsr-lang-*` binary runs in the user's authority and indexes/exfiltrates private code | Med (supply chain) | Critical | Cosign signature + SLSA provenance (Rule 1); identity pinned in catalog, verified at AUTHORIZE before spawn (Rule 2); ADR-017 sandbox contains residual blast radius | extended (signed distribution) |
| **T20 (new)** | Plugin binary in transit | Binary tampered/substituted between CI build and the user's machine (MITM, registry compromise) | Low | Critical | Signature verified post-download before extraction (Rule 3 npm `postinstall`) and again at AUTHORIZE (Rule 2); hash-pinned distribution | new |
| **T21 (new)** | Wire contract | A `protocol_version`-mismatched plugin is driven and mis-decodes into forged graph data | Low | High | Fail-fast registration refusal (Rule 4 / ADR-017 Rule 5); coordinated protocol-bump releases | new |
| **T23 (new)** | Multiple languages' graphs | A tampered/substituted **shared analyzer** (`scip-java`) poisons every language it serves at once (concentrated blast radius, RFC-013 D6a) | Low | High | Analyzer hash-pinned in catalog + verdict, re-verified at spawn (Rule 6); org-republished analyzers signed; user-pointed path runs sandboxed + surfaced as unauthenticated | new |

---

## Tests QA must add

- **Unsigned refused:** a `travsr-lang-x` binary with no/invalid signature is
  `Quarantined{Untrusted}` at AUTHORIZE and is **never spawned**.
- **Wrong identity refused:** a validly-signed binary signed by an identity other
  than the catalog-pinned one is refused.
- **Tamper detected (T20):** flipping a byte after signing causes signature
  verification to fail both at `postinstall` and at AUTHORIZE.
- **Cached-Active does not bypass (T17 × this ADR):** a cached `Active` verdict does
  **not** skip signature/hash re-check at spawn.
- **Provenance:** the SLSA attestation must name the expected source repo + builder;
  a missing/foreign provenance fails.
- **Shared-analyzer tamper (T23):** a `scip-java` whose `sha256` differs from the
  catalog-pinned hash is refused at AUTHORIZE for **all** languages it serves (Java,
  Kotlin, Scala), not just one; a user-pointed path not matching the pin is surfaced
  as unauthenticated.
- **Protocol mismatch (T21):** a plugin advertising an unsupported `protocol_version`
  is refused at registration, not driven.
- **ARM64 delivery:** the install path selects the `aarch64` artifact on Apple
  Silicon / OCI A1; no x86-under-QEMU fallback.

---

## Consequences

**Positive:**
- Establishes the authenticity layer ADR-017 explicitly deferred — first-party
  plugin binaries now carry the same cryptographic trust as the core release.
- AUTHORIZE has a real signal: signature + provenance gate execution *before* the
  probe, so an unauthenticated binary is never run.
- Independent plugin shipping (RFC-013's decoupling win) does not weaken the supply
  chain — each plugin inherits the core signing bar.

**Negative / accepted:**
- Release/CI burden: every `travsr-lang-*` gains a signed, multi-target build matrix
  (incl. ARM64). Mitigated by templating the plugin CI (RFC-013 Phase 5 task) so a
  new language inherits signing rather than re-implementing it.
- Key/identity management: the pinned signing identity must be rotated carefully —
  an identity rotation invalidates pinned catalog entries until updated (a
  `BOUNDARY-CHANGE:`-class coordination, surfaced to users on mismatch).
- Community plugins gain **no** authenticity guarantee under this ADR (Rule 5) —
  accepted, deferred with the community registry; `--command` stays explicit + local.

---

## Escalations

- **Signing identity rotation or compromise** → PSE + DevOps incident path (SEV-2
  class: signed-release tampering); coordinated catalog re-pin + plugin re-release.
- **A community-registry / third-party-publisher request** → CTO (re-opens the
  deferred ecosystem decision) + PSE for the registry trust-root design; not grantable
  under this ADR.
- **A signing requirement that would force off the OCI free tier or slip a public
  commitment > 1 sprint** → CTO + PM.

---

## Verdict

**APPROVED_WITH_MITIGATIONS** — Rules 1 (mandatory signing) and 2 (verify the pinned
identity at AUTHORIZE, before spawn) are blocking for RFC-013 Phase 5: no language
may be migrated to a shipped, runtime-subscribed sidecar until its binary is signed
and AUTHORIZE enforces the signature. The community-plugin authenticity gap (Rule 5)
is accepted and deferred with the registry decision.

---

## References

- RFC-013 — Open Language Identity & Runtime Language Pluggability (D2/D8; this ADR gates Phase 5)
- ADR-017 — Unified Plugin Sandbox & Trust Model (sandboxes execution; defers authenticity to "a Phase 5 ADR" — this one)
- ADR-019 — Language Subscription & Verdict Cache (AUTHORIZE consumes the signature; cache is advisory, not a bypass)
- ADR-018 — Plugin Resource Governance (Windows sidecar timing; per-platform build matrix)
- RFC-011 — Two-Transport Language Plugin Architecture (registration fail-fast on protocol mismatch)
- CLAUDE.md — Non-Negotiable Principles (#3 local-first, #5 OCI free tier, #6 ARM64)
