# ADR-019: Language Subscription & Verdict Cache

**Date:** 2026-06-04
**Status:** Proposed
**Phase:** Cross-cutting (RFC-013 Phase 2 — the first non-boundary engine RFC-013 needs)
**Author:** Tech Lead (state machine) + Principal Security Engineer (admission gate)
**Extends:** ADR-017 (trust precedes admission — this ADR is the runtime admission machine ADR-017 Rule 3 implies)
**Related:** RFC-013 (runtime language pluggability — D3/D4/D5), ADR-018 (resource kills produce SOFT verdicts), ADR-020 (signature verified during AUTHORIZE), ADR-005 (per-corpus trust), CLAUDE.md §Non-Negotiable Principles (#2 always-fresh, #3 local-first)

---

## Context

RFC-013 opens language identity so a plugin can declare a `LanguageId` the core
never enumerated. Open identity **without** a runtime gate would admit a broken or
malicious plugin's nodes straight into the graph. The gate is a runtime
**subscription + verification** flow that replaces the compile-time
`register_builtins` enumeration (`travsr-plugin-host/src/registry.rs:93`).

That flow needs a **persistent, machine-local record** of what was verified, for
three reasons:

1. **Performance** — verification (spawn + sandboxed conformance probe) is too
   expensive to run on every daemon start or index; a verified binary must be
   remembered.
2. **Correctness under transient failure** — a *correct* binary can fail
   verification because of a momentary system error (memory pressure, full disk,
   saturated CPU, a transiently-unavailable sandbox). The record must distinguish a
   **binary fault** (stays rejected) from a **machine fault** (must remain
   subscribable), or it will permanently punish good plugins for bad luck.
3. **Identity** — "the same binary" must be defined precisely so that deleting and
   restoring a binary, or disabling and re-enabling a language, does not force a
   needless re-probe.

The substrate exists: `travsr-plugin-host/src/trust.rs` already reads
`~/.travsr/lang.toml` (`from_disk`, `registered_languages_from_disk`) and holds
per-corpus trust; `PluginHealth { Ok, Disabled(String) }`
(`travsr-plugin-host/src/transport.rs:12`) is a binary verdict set only on crash.
This ADR enriches the verdict and persists it.

This ADR decides:

1. The subscription lifecycle state machine.
2. The verdict taxonomy and the rule that classifies a failure as sticky vs transient.
3. The verdict cache: location, key, contents, and invalidation.
4. Delete / disable / re-verify semantics ("re-subscribe the same binary?").

---

## Decision

### Rule 1 — The lifecycle: SUBSCRIBE → AUTHORIZE → VERIFY → ADMIT / QUARANTINE

```
SUBSCRIBE   plugin handshake declares:
            { LanguageId, extensions, capabilities, protocol_version,
              binary_hash, plugin_version, phase_b_prerequisites }
   │
AUTHORIZE   ADR-017 trust + ADR-020 signature: is this binary allowed to RUN AT ALL?
   │        ✗ → Quarantined{Untrusted}.  (Untrusted code is NOT executed to "test it".)
   │            The probe in VERIFY runs INSIDE the sandbox, only after this passes.
VERIFY      conformance probe (Rule 2), bounded by ADR-018 probe_deadline/limits
   │        ✗ → Quarantined{reason} (HARD or SOFT per Rule 3)
ADMIT       register extensions → transport; write verdict to cache (Rule 4)
```

AUTHORIZE strictly precedes VERIFY (PSE): "check if the binary works" means
*executing a third-party binary*, which is only permissible once trust + signature
clear it. This ADR's "pass/fail" therefore applies **only to binaries already
decided eligible to run**.

The 4 builtins (ts/js/rust/python, RFC-013 D3) subscribe via the in-process fast
path — they skip AUTHORIZE's spawn gate but **still run the conformance probe once
in CI** so the probe kit itself is exercised against known-good output.

### Rule 2 — Verification tiers (what "usable" means, and its ceiling)

The probe verifies what protects **our** invariants, not the plugin's semantics:

| Tier | Checks | Protects |
|---|---|---|
| Liveness | spawns, handshakes, `protocol_version` matches, declares a `LanguageId` | wiring |
| **Structural conformance** | well-formed `Node`/`Edge`; every VName a valid 5-tuple built via `travsr_core::VName::new`; edges reference emitted nodes; `node.language == declared LanguageId` | **VName uniqueness** (CLAUDE.md / Principal Architect invariant) |
| **Determinism** | parse the *same bytes twice* → byte-identical `ParseResponse` | **incremental correctness** (full reindex == incremental) |

**Selected gate: structural + determinism.**

**Honest ceiling (no oracle):** for a net-new language we cannot verify the grammar
parses the language *correctly* — only that output is structurally valid,
deterministic, and self-consistent. The plugin **ships its own golden fixture**; we
assert validity + determinism over it, never content. Semantic correctness remains
the plugin author's test suite, exactly as grammar correctness already is.

### Rule 3 — Verdict taxonomy and the reproducibility rule

| Verdict | Persisted & hash-keyed? | Blocks re-subscribe of same binary? | Reasons |
|---|---|---|---|
| `Active(tier)` | yes | — | passed; `tier ∈ {A+B, A-only}` (RFC-013 D6) |
| **`Rejected` (HARD)** | yes | yes — needs a *changed* binary or `--force` | `ConformanceFailed`, `NonDeterministic`, `ProtocolSkew` — **reproducible binary properties** |
| **`Unverified` (SOFT)** | **no** — not attributed to the binary | **no — auto re-probed** | `Timeout`, `ResourceExceeded`, `SandboxUnavailable`, `ProbeIoError`, `SpawnFailed` — **machine properties at probe time** |
| `Unavailable` | n/a (operational) | n/a | binary missing on disk, or Phase-B prerequisite absent (RFC-013 D6) |
| `Disabled` | policy override | yes until re-enabled | explicit `travsr languages disable <lang>` |

**The reproducibility rule (disambiguates ambiguous failures such as a crash):**

> Run the probe with in-attempt retry + backoff (N attempts; N and backoff tuned per
> ADR-018 deadlines). A failure is recorded **HARD `Rejected`** only if it reproduces
> across all attempts. A failure that clears on retry is **SOFT `Unverified`** and is
> not cached against the binary. Reproducible = binary-intrinsic = sticky.

This is the mirror of the determinism check (parse-twice → identical; here,
fail-twice → confirm). A binary that segfaults every attempt is rejected; a one-off
OOM-kill is absorbed and retried.

**Pre-flight environment gate:** before judging the binary, verify preconditions we
own (sandbox available, scratch creatable, resource headroom per ADR-018). If those
fail, the verdict is against the **environment** (`SandboxUnavailable` /
`ProbeIoError`), surfaced as "cannot verify `<lang>`: …", never as a binary fault.

### Rule 4 — The verdict cache: location, key, contents

Machine-local, **never** committed to a repo (a verdict is binary- and
arch-specific; a committed verdict is an admission-bypass vector — see T17):

```
~/.travsr/lang.toml          ← exists: corpus trust + registered langs
~/.travsr/subscriptions.lock ← THIS ADR: verdict cache
                                (honours TRAVSR_LANG_FILE-style override dir)
```

**Key (the tuple that makes a verdict valid):**
```
(LanguageId, binary_path, sha256(binary), plugin_version,
 protocol_version, probe_kit_version)
```

**Value:**
```
{ verdict, golden_fixture_hash, sandbox_policy, verified_at }
```

This cleanly separates two questions that must not be conflated:
- **"Is this binary structurally sound?"** → this cache, global, hash-keyed.
- **"Is *this repo* allowed to load it?"** → per-corpus trust (`trust.rs`, ADR-005/017).

The cache is **advisory, not authoritative** (T17): the binary hash is re-checked
at spawn, and AUTHORIZE (Rule 1) re-runs regardless of a cached `Active`. A tampered
cache cannot admit a binary that fails the live trust/signature gate.

### Rule 5 — Delete / disable / re-verify semantics

**"If the user deletes or disables the binary, must they re-subscribe the same
binary on the same machine?" → No, not if it is byte-identical.** Because the
verdict is keyed by `sha256(binary)`:

- **Deleted binary** → spawn fails → `Unavailable{BinaryMissing}`; the verdict is
  **retained**, not purged. A byte-identical binary returning at any path → hash hit
  → cached `Active` applies immediately, **zero re-probe**. A *different* binary at
  the same path → hash miss → full re-subscribe. (Re-hash is gated on `mtime+size`
  to avoid hashing an unchanged binary every spawn.)
- **User-disabled** (`travsr languages disable <lang>`) → a separate **policy
  override** that supersedes the verdict, orthogonal to verification. Re-enable
  flips it back with **no re-probe** if the hash is unchanged.

**Re-verification trigger matrix (complete):** binary hash change · `plugin_version`
change · `protocol_version` bump · `probe_kit_version` bump. **Nothing else** —
verdicts do not expire by time; a verified binary stays verified.

**Manual escape hatch:** `travsr languages verify <lang> --force` forces a fresh
probe regardless of any cached verdict — including overriding a HARD `Rejected` —
for the residual misclassification / "I fixed my environment" case.

---

## Threat model — rows touched / added (T-table)

> Numbering continues the global table; ADR-017 occupies T11–T13, ADR-018 T14–T16.

| Row | Asset | Threat | Likelihood | Impact | Mitigation | Δ |
|---|---|---|---|---|---|---|
| **T17 (new)** | Graph integrity | A tampered `subscriptions.lock` marks a malicious binary `Active` to bypass verification | Low | High | Cache is advisory only (Rule 4): hash re-checked at spawn; AUTHORIZE (trust + signature) re-runs regardless of cached `Active`; cache never committed to a repo | new |
| **T18 (new)** | Graph integrity | A plugin emits nodes under a spoofed `LanguageId` to poison another language's graph slice | Low | Med | Conformance probe asserts `node.language == declared LanguageId` (Rule 2); per-corpus trust scopes admissible languages (ADR-005/017) | new |
| **T19 (new)** | Availability / DoS | A flaky plugin is re-probed in a tight loop, spawning untrusted code repeatedly | Med | Med | SOFT failures re-probe only on a trigger (Rule 5), not in a loop; HARD failures never auto-retry; AUTHORIZE precedes every probe | new |

---

## Tests QA must add

- **Garbage rejected:** a plugin emitting malformed VNames / dangling edges / a
  wrong `node.language` is `Rejected{ConformanceFailed}` and admits **zero** nodes.
- **Non-determinism caught:** a plugin whose two parses of identical bytes differ is
  `Rejected{NonDeterministic}`.
- **Transient survives:** a correct binary failing once with a simulated
  resource/sandbox error is `Unverified` (SOFT), **not** `Rejected`, and subscribes
  on retry with **no binary change**.
- **Reproducibility rule:** a binary that fails every attempt → HARD; a binary that
  fails once then passes → SOFT.
- **Hash identity:** deleting then restoring the byte-identical binary → cached
  `Active` reused, no re-probe; a modified binary at the same path → re-probe.
- **Disable/enable:** disabling then re-enabling an unchanged binary → no re-probe.
- **Cache tamper (T17):** a hand-edited `Active` for a binary that fails the live
  trust/signature gate is **ignored**; the binary is not admitted.
- **`--force`:** overrides a cached HARD `Rejected` and re-runs the probe.

---

## Consequences

**Positive:**
- Open identity becomes **safe**: a broken or malicious plugin is quarantined with a
  diagnostic, never silently admitted.
- Correct binaries are robust to transient system errors — the SOFT/HARD split plus
  the reproducibility rule means a good plugin is never permanently punished for a
  machine hiccup.
- Deterministic, auditable invalidation (the trigger matrix) and a clean separation
  of "binary sound?" from "repo allowed?".
- `register_builtins` stops being a compile-time list; adding a language is a
  runtime subscription with no boundary edit.

**Negative / accepted:**
- A first-subscribe probe cost (spawn + two parses) per new binary/version. Bounded:
  it runs once per `(hash, versions)`, cached thereafter.
- The "no oracle" ceiling: we cannot certify a third-party grammar is *semantically*
  correct, only structurally valid + deterministic. Documented; mirrors the existing
  trust posture for first-party grammars.
- A new on-disk artifact (`subscriptions.lock`) to version and migrate across
  `probe_kit_version` changes; migration invalidates and re-probes, which is correct
  by Rule 5.

---

## Escalations

- **Probe retry count / backoff (N)** materially affecting flake rate or spawn churn
  → Tech Lead + DevOps to tune against observed data.
- **A proposal to make the cache authoritative** (skip AUTHORIZE on cached `Active`
  for speed) → **REJECTED** at this layer; escalate to PSE if revisited, as it
  reintroduces T17.

---

## Verdict

**APPROVED_WITH_MITIGATIONS** — Rules 1 (authorize-before-execute) and 4 (cache is
advisory, never authoritative) are blocking: they are what keep open identity from
becoming an admission bypass. Must land with RFC-013 Phase 2, before any
non-builtin language is migrated to a runtime-subscribed sidecar.

---

## References

- RFC-013 — Open Language Identity & Runtime Language Pluggability (D3/D4/D5; this ADR is its subscription/verdict dependency)
- ADR-017 — Unified Plugin Sandbox & Trust Model (trust precedes admission; the probe runs inside its sandbox)
- ADR-018 — Plugin Resource Governance (probe is bounded by its deadlines; resource kills produce SOFT verdicts)
- ADR-020 — Plugin Binary Distribution & Signing (signature verified during AUTHORIZE)
- ADR-005 — Per-Language Corpus Naming (per-corpus trust scopes admissible languages)
- CLAUDE.md — Non-Negotiable Principles (#2 always-fresh, #3 local-first)
