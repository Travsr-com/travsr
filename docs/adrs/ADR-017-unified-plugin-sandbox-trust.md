# ADR-017: Unified Plugin Sandbox & Trust Model

**Date:** 2026-05-30
**Status:** Proposed
**Phase:** 5 (P5-S1 — structural prerequisite for P5-S2–P5-S5)
**Author:** Principal Security Engineer
**Supersedes:** ADR-006 (rust-analyzer subprocess trust model) — generalised here
**Obviates:** the planned per-tool ADR chain referenced by RFC-008 §7 — ADR-011 (`scip-java` trust), ADR-012 (`scip-typescript` trust), ADR-013 (`scip-kotlin` trust), ADR-015 (bridge plugin panic-isolation), ADR-016 (TOML descriptor trust). These are **not written**; their concerns fold into this single ADR.
**Related:** RFC-011 (two-transport plugin architecture — companion; merges same sprint), RFC-008, RFC-009, ADR-005 (per-language corpus naming — trust keyed per canonical corpus), ADR-006 (the precedent this generalises), CLAUDE.md §Non-Negotiable Principles

---

## Context

RFC-011 replaces the hardcoded indexer + per-tool-trust-ADR model with one `Plugin` contract behind two transports (in-process for first-party Phase A; sidecar + sandbox for Phase B and community plugins). That refactor moves the trust boundary in one direction (every untrusted invoker now sits behind a single transport) and opens a new surface in another (community `--command` binaries; an internal IPC channel; a content-addressed parse cache).

RFC-008 §7 would have required a fresh subprocess-trust ADR per language (ADR-006, then ADR-011/012/013, …). The threat each would describe is identical: **a SCIP/LSIF toolchain executes the indexed repository's `build.rs`, proc-macros, Gradle/Maven scripts, or `setup.py` — arbitrary code the indexer did not author and cannot audit.** Re-arguing that per language is process cost without a security benefit.

This ADR defines **one** sandbox policy and **one** trust-gating model for **every** plugin subprocess — Phase B invokers and community Phase A plugins alike — and the compensating controls that make the in-process transport safe for first-party grammars.

This ADR decides:

1. What sandbox every plugin subprocess runs under, and what happens when the sandbox is unavailable.
2. How trust is granted before a subprocess that executes repo-related code is spawned.
3. Why and under what conditions in-process (un-sandboxed) execution is acceptable for first-party Phase A grammars.
4. How the parse cache and the internal IPC channel are protected.

---

## Decision

### Rule 1 — One sandbox policy for every plugin subprocess

Every Sidecar-transport spawn (RFC-011 §2) — whether a Phase B SCIP/LSIF invoker or a community Phase A plugin — runs under a single `SandboxPolicy::Standard`:

```
SandboxPolicy::Standard
  network:    ALLOW             (intentional — see Amendment A1 below)
  filesystem: repo root         → READ-ONLY
              scratch tmpdir     → READ-WRITE (per-invocation, removed after)
              everything else    → DENY
  resources:  CPU / RAM / wall-clock caps enforced
  env:        scrubbed allowlist — permitted set:
                PATH    (toolchain discovery)
                LANG    (locale)
                LC_ALL  (locale)
                TMPDIR  (set to the per-invocation scratch dir; NOT the user's $TMPDIR)
              Any variable beyond this list requires explicit justification recorded here.
              No HOME, no CARGO_HOME, no GIT_*, no SSH_*, no AWS_* / GCP_* / AZURE_*,
              no CI, no GITHUB_TOKEN, no NPM_TOKEN, and no other credential-carrying vars.
```

> **Amendment A1 — Network allow for Standard/NativeIpc (2026-06-11)**
>
> Network egress is **intentionally allowed** in `Standard` and `NativeIpc` sandbox policies.
>
> **Rationale:** Every language build tool that Phase B invokes (`go mod download`, `npm install`,
> `pip install`, `pub get`, Maven/Gradle for scip-java) requires outbound network access to
> resolve and fetch dependencies at analysis time. Blocking network caused 0 edges on
> multi-package repos (e.g. Kubernetes: 2255 packages, 0 LSIF edges → 426,636 after fix).
>
> **Compensating controls:**
> - Filesystem confinement via bwrap/sandbox-exec still fully applies — the plugin cannot
>   write outside its scratch dir or read outside the repo root.
> - `Elevated` policy continues to exist for documenting host-allowlists; host-level egress
>   controls (firewall / egress proxy) remain the network boundary.
> - The sentence in Rule 1 below ("disable the network-deny rule entirely, it cannot be
>   granted under Elevated — escalate to CTO") referred to ad-hoc Elevated exceptions that
>   bypass documented allowlists. Standard's unconditional network-allow is not an Elevated
>   exception; it is an explicit policy change recorded here and enforced uniformly.
>
> **Approved by:** Principal Security Engineer (PSE review 2026-06-11, branch
> `feature/travsr-daemon-init-at-scale-295`)

> **Amendment A2 — Windows sandbox FFI unsafe-code sanction (2026-08-05)**
>
> The Windows sandbox mechanism (AppContainer + Job Object, the Windows row of the
> table below) is implemented against raw Win32 APIs — `CreateAppContainerProfile`,
> `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`,
> `SetNamedSecurityInfoW`/`GetAce`, `CreateJobObjectW`, `OpenProcess` — none of which
> have safe standard-library equivalents. This amendment sanctions `unsafe` code in
> the `travsr-plugin-host` workspace under the following invariants:
>
> 1. **Confinement:** every `unsafe` block lives in exactly one file,
>    `crates/travsr-plugin-host/src/sandbox/windows/ffi.rs`. The crate root carries
>    `#![deny(unsafe_code)]`; only that file carries the `#![allow(unsafe_code)]`
>    override. Any second override site re-opens this amendment.
> 2. **Encapsulation:** the file exposes only safe wrappers; OS handles, SIDs, ACLs,
>    and attribute lists are owned by RAII types (`OwnedHandle`, `AppContainerSid`,
>    `AttrList`, `OwnedSecurityCapabilities`) so no raw resource outlives its owner.
> 3. **Verification:** invariants with a history of violation are pinned by unit
>    tests running against real OS objects (capability-pointer stability after moves,
>    Job Object limit read-back, DACL ACE round-trips, process liveness).
>
> This corrects a stale citation: the override previously cited RFC-014, which covers
> Phase B symbol unification and says nothing about unsafe or FFI. ADR-017 is the
> governing document for the sandbox mechanism, so the sanction is recorded here.
>
> **Approved by:** _pending Tech Lead sign-off (raised in PR #577 review; drafted
> 2026-08-05)_

> **Amendment A3 — per-language toolchain env forwarding (2026-08-05)**
>
> Rule 1's scrubbed env allowlist (PATH / LANG / LC_ALL / TMPDIR) is extended by the
> **daemon-computed** per-language toolchain variables in
> `crates/travsr-plugin-host/src/sandbox/toolchain.rs` (`ToolchainAccess::env`):
> `HOME`, `GOPATH`, `GOCACHE`, `GOMODCACHE`, `GOROOT`, `JAVA_HOME`,
> `GRADLE_USER_HOME`, `SBT_OPTS`, `COMPOSER_HOME`, `NUGET_PACKAGES`, `DOTNET_ROOT`,
> `GEM_HOME`, `GEM_PATH`, `CARGO_HOME`, `RUSTUP_HOME`, `NVM_DIR`, `PYENV_ROOT`,
> and `TRAVSR_DART_EMITTER`.
>
> **Rationale:** Phase B analyzers drive the language's real build tool, which
> resolves module/build caches through these variables; without them the analyzer
> resolves zero packages and emits an empty index (the scip-go 0-of-244 case that
> created `toolchain.rs`). These are location pointers computed by the daemon from
> the same paths that receive the sandbox's filesystem grants — they are **not** an
> ambient-environment passthrough, and the filesystem confinement (not the env)
> remains the enforcement boundary: a forwarded `HOME` value does not grant access
> to anything the FS rules deny. The Rule 1 exclusion of credential-carrying
> variables (`GIT_*`, `SSH_*`, `AWS_*`/`GCP_*`/`AZURE_*`, `CI`, `GITHUB_TOKEN`,
> `NPM_TOKEN`, …) is unchanged and still absolute; `HOME` and `CARGO_HOME` move
> from the "never" list to this justified set.
>
> Linux (`sandbox/linux.rs`) and macOS (`sandbox/macos.rs`) have forwarded this set
> since `toolchain.rs` was introduced; Windows (`sandbox/windows/ffi.rs`) matches as
> of #501. This amendment makes that shipped behavior the recorded policy instead of
> an undocumented divergence (raised in PR #577 review).
>
> **Approved by:** _pending Tech Lead sign-off (raised in PR #577 review; drafted
> 2026-08-05)_

Mechanism by platform (DevOps owns the implementation, Security owns the policy):

| Platform | Primary mechanism | Fallback |
|---|---|---|
| Linux (incl. OCI A1 / aarch64) | **bubblewrap** (outer namespace + seccomp-bpf container) + **Landlock** FS rules (additive, if kernel ≥ 5.13) | bubblewrap without Landlock if kernel < 5.13; plugin **disabled** (fail-closed) if bubblewrap is absent |
| macOS | `sandbox-exec` (Seatbelt) profile | **plugin disabled (fail-closed per Rule 2)** — if `sandbox-exec` is absent or returns non-zero at spawn time, treat identically to a missing sandbox: disable the plugin, emit `tracing::warn!`, surface as `disabled (sandbox unavailable)` in `travsr language list` |

The policy is defined **once**, reviewed **once**, and applied at every spawn. Adding a language does not re-open the policy. A language whose toolchain needs an exception (e.g. legitimate network access to fetch a toolchain component) does not get a new ADR — it gets a reviewed, named exception recorded in `travsr.toml` and surfaced to the user; the **default** is always `Standard`.

`SandboxPolicy` is defined normatively as an enum; `Elevated` is the exception variant:

```rust
pub enum SandboxPolicy {
    /// The default — applied to every Sidecar spawn unless an explicit exception is approved.
    Standard,
    /// Exception variant — requires PSE sign-off before any implementation PR merges.
    Elevated {
        /// Explicit allowlist of hosts the plugin may reach. No wildcards. No CIDR ranges.
        /// Example: vec!["repo1.maven.org".to_string(), "plugins.gradle.org".to_string()]
        permitted_hosts: Vec<String>,
        /// One-sentence human-readable justification recorded in travsr.toml and shown in
        /// `travsr language list`. Required; empty string is rejected at parse time.
        reason: String,
        /// GitHub username/handle of the Security reviewer who approved this exception.
        approved_by: String,
        /// ISO-8601 date the approval was recorded (e.g. "2026-06-01"). Approvals older
        /// than 12 months require re-review.
        approved_date: String,
    },
}
```

Approval requirement: any use of `SandboxPolicy::Elevated` must be reviewed and signed off by the Principal Security Engineer before the implementation PR merges. Self-approval is forbidden. If an exception would require a wildcard host (e.g. `*.gradle.org`) or disable the network-deny rule entirely, it cannot be granted under `Elevated` — escalate to CTO.

### Rule 2 — Fail-closed (non-negotiable)

If the sandbox mechanism is unavailable on the host (missing `bwrap`, kernel without seccomp, `sandbox-exec` failure), the affected plugin is **disabled** and its files are **not indexed**. There is **no path** that runs a plugin subprocess un-sandboxed as a fallback.

> The "trust the user's local toolchain because the sandbox tool is missing" fallback **is** the vulnerability. It is forbidden. (Security hard rule; CLAUDE.md #3 local-first.)

The daemon emits a `tracing::warn!` naming the plugin and the missing mechanism, and `travsr language list` shows the language as `disabled (sandbox unavailable)`. Phase A for *first-party* languages is unaffected because it runs in-process (Rule 4), not in a sandbox — the structural graph is always available.

### Rule 3 — Trust is granted per canonical corpus, before spawn

A subprocess that executes repo-related code is spawned **only** after an explicit, persistent trust grant, keyed per canonical corpus (ARCH-102), reusing the ADR-006 primitive:

```
travsr config set plugins.trust.<canonical-corpus> true     # Phase B invokers
travsr language add <lang> --command <binary>               # community plugin: explicit opt-in
```

- **The daemon never enables a code-executing subprocess based on content found inside the repo itself.** A repo cannot opt *itself* into Phase B or into a community plugin. (Direct inheritance of ADR-006 Rule 1.)
- Trust is **per corpus**, not global: trusting `github.com/me/my-repo` does not trust a dependency checkout.
- Community `--command` binaries are **never** auto-discovered. The user names the binary explicitly; the daemon records the grant; the binary then runs under `SandboxPolicy::Standard` like any Phase B invoker.

### Rule 4 — In-process is permitted only for first-party, fuzzed grammars

The in-process transport (RFC-011 §10) runs first-party Tree-sitter grammars (C, via FFI) in the daemon's address space with **no** sandbox. This is accepted because Phase A executes **no untrusted code** — it parses bytes — and because the residual risk (a memory bug in a C grammar triggered by crafted source) is contained by mandatory compensating controls:

In-process eligibility requires **all** of:

1. **First-party** — the grammar crate is a workspace dependency carried in the monorepo (never a `--command` plugin).
2. **Pinned** — exact patch version (e.g. `=0.23.4`; `x` is not valid Cargo semver after `=`), `cargo-deny` advisory gate in CI (Tree-sitter grammar CVEs have shipped historically — RFC-003 §7). The pinned version must be updated in `deny.toml` on every grammar bump and reviewed in the PR that changes it.
3. **Fuzzed** — a `cargo-fuzz` target exists for the grammar under `fuzz/` and runs in the nightly fuzz workflow.
4. **Fixture-gated** — golden fixtures (RFC-003 §6) gate every change.

Any grammar that cannot meet all four runs under the **Sidecar** transport instead (fault-isolated, §10 RFC-011), trading IPC cost for crash isolation. The choice is per-grammar and reversible at the transport boundary.

### Rule 5 — Cache and IPC integrity

- **Cache keys are daemon-computed.** The parse cache (RFC-011 §6) is keyed by `(plugin_version, sha256(file))` where the `sha256` is computed by the **daemon**, never reported by the plugin. A plugin cannot select or forge a cache slot. `plugin_version` (from the handshake) is a hard invalidation component; a plugin-logic change that fails to bump it is a freshness bug, not a security bypass, but the daemon additionally records the running `plugin_version` in graph metadata so drift is detectable (`travsr status`).

  **CI enforcement gate (normative):** The CI pipeline MUST compute a content hash of each plugin crate's `src/` tree (e.g. `sha256sum $(find crates/travsr-plugin-<lang>/src -type f | sort)`) and store it alongside `plugin_version` in a `plugin-hashes.lock` file committed to the repo. On every CI run the pipeline re-hashes the source tree and asserts it matches the recorded hash if and only if `plugin_version` is unchanged. A source-tree change with no `plugin_version` bump fails CI with a mandatory error message naming the affected plugin. This makes the "forgot to bump" failure mode a build error rather than a silent stale-cache bug.
- **Protocol version is fail-fast.** A plugin whose `protocol_version` the daemon does not support is refused at registration (RFC-011 §4) — never driven with a mismatched contract that could mis-decode into forged nodes/edges.
- **The IPC channel is not externally reachable.** stdin/stdout to a spawned child; no listening socket; carries no client data. It does not widen the network attack surface and does not breach MCP-only (RFC-011 §9).

---

## Threat model — rows touched / added (T-table)

| Row | Asset | Threat | Likelihood | Impact | Mitigation | Δ |
|---|---|---|---|---|---|---|
| **T4** | Developer machine | Untrusted code execution via indexed repo's build scripts/proc-macros (Phase B) | Med | High | `SandboxPolicy::Standard` (Rule 1), fail-closed (Rule 2), per-corpus trust before spawn (Rule 3) | **improved** — now structural, not per-tool |
| **T11 (new)** | Developer machine | Malicious community plugin binary (`--command`) running in the daemon's authority | Med (supply chain) | High | Same sandbox as Phase B (Rule 1); explicit per-corpus opt-in (Rule 3); never in-process (Rule 4); fail-closed (Rule 2) | new |
| **T12 (new)** | Graph integrity | Parse-cache poisoning — a forged/stale `(plugin_version, sha256)` entry serving fabricated nodes/edges | Low | Med | Daemon-computed hash, plugin never supplies keys; mandatory version bump on logic change (Rule 5) | new |
| **T13 (new)** | Graph integrity / availability | Malformed source triggers a memory bug in an in-process C grammar → daemon RCE/crash | Low | High | In-process restricted to first-party + pinned + fuzzed + fixture-gated (Rule 4); else run sidecar | new |

---

## Findings carried from the RFC-011 design review

| # | Severity | Finding | Required mitigation | Blocks merge |
|---|---|---|---|---|
| 1 | SEV-1 | Any "sandbox missing ⇒ run plugin anyway" path | Fail-closed; disable the plugin (Rule 2) | **yes** |
| 2 | SEV-2 | In-process grammar runs attacker-controlled source in-daemon (C/FFI) | Restrict in-process to first-party fuzzed grammars (Rule 4); document crash domain (RFC-011 §10) | **yes** |
| 3 | SEV-2 | `--command` community binary is arbitrary local code | Per-corpus trust opt-in + `SandboxPolicy::Standard` (Rules 1, 3) | **yes** |

---

## Tests Security requires QA to add

- **Egress allowed (Standard/NativeIpc):** a plugin running under `Standard` or `NativeIpc` policy CAN reach the network — this is intentional per Amendment A1. The test `sandbox_standard_allows_network` verifies this. `Elevated` policy follows the same allow rule; host-level egress controls enforce the permitted-hosts list.
- **Fail-closed:** with the sandbox mechanism forcibly unavailable, the plugin is disabled and indexes **zero** of its files — assert no fallback path runs the subprocess.
- **Trust gate:** a `--command` plugin with no `plugins.trust.<corpus>` / `--command` opt-in is **refused**; a repo cannot opt itself into Phase B via committed config.
- **FS confinement:** a plugin attempting to write inside the repo root (outside the scratch tmpdir) fails; repo is read-only.
- **Resource caps:** a plugin exceeding the wall-clock cap is killed and the file marked failed without hanging the index run.
- **Cache integrity:** a plugin-supplied cache key is ignored; only the daemon-computed `(plugin_version, sha256)` selects a slot.

(These are merge-gating for any sprint landing a Sidecar plugin — P5-S1 onward.)

---

## Supply-chain check (applies per added language/plugin)

- [ ] `cargo-deny` passes (advisories, licenses, bans) — including each pinned `tree-sitter-<lang>` grammar.
- [ ] New grammar/invoker deps fit the MIT/Apache-2.0/BSD-3-Clause/ISC allowlist.
- [ ] `cargo-fuzz` target present for any in-process grammar (Rule 4.3).
- [ ] Release artifact sigstore-signed + SLSA provenance (unchanged release bar).

---

## Consequences

**Positive:**
- One sandbox review instead of N. Adding a language is no longer gated on a bespoke trust ADR — it inherits `SandboxPolicy::Standard`, and only a genuine exception (network, elevated FS) triggers a (named, recorded) review. This unblocks RFC-011's P5-S3–P5-S5 language additions without per-language Security re-litigation.
- Net posture **improvement** over RFC-008: the sandbox boundary is structural (enforced at the transport layer for *every* untrusted spawn) rather than per-tool and easy to forget.
- The fail-closed rule removes the most common real-world sandbox bypass (the "tool missing, run anyway" fallback).

**Negative / accepted:**
- The in-process transport keeps a shared crash domain for first-party grammars (T13). Accepted under Rule 4's compensating controls; the escape hatch (run that grammar sidecar) is always available at the transport boundary.
- A genuinely new threat surface (community `--command` plugins) exists, but is gated (Rule 3) and deferred to Phase 5 for the *ecosystem* (publishing/registry); Phase 4 ships first-party only, so the surface is dormant until the trust root is designed.

---

## Escalations

- **Authenticity vs. execution.** This ADR sandboxes *execution*. Plugin *authenticity* (signing, a registry trust root for community plugins) is **out of scope** and deferred to a Phase 5 ADR, gated by the CTO's decision (2026-05-30) to defer the community SDK/registry. Until then, `--command` plugins are a local, explicit, per-corpus opt-in only.
- **Elevated-sandbox exceptions.** Any language requiring `SandboxPolicy::Elevated` returns to Security for the *exception* (not a full per-language ADR). If an exception would force off the OCI free tier or slip a public commitment > 1 sprint, escalate to CTO + PM.

---

## Verdict

**APPROVED_WITH_MITIGATIONS** — the three SEV-1/SEV-2 findings above are blocking and must land with the S12 transport work. With them in place, the two-transport architecture (RFC-011) is a net security improvement over the per-tool model it replaces.

---

## References

- RFC-011 — Two-Transport Language Plugin Architecture (companion)
- ADR-006 — rust-analyzer Subprocess Trust Model (generalised here)
- RFC-008 — Multi-Language Extension Architecture (per-tool ADR chain obviated)
- RFC-009 — Cross-Language Bridge Plugin System (bridge panic-isolation folded into Rule 4 / RFC-011 §10)
- ADR-005 — Per-Language Corpus Naming (trust keyed per canonical corpus)
- ARCH-102 — Kythe Corpus Naming Convention (canonical-corpus identity for trust grants)
- CLAUDE.md — Non-Negotiable Principles (#2 always-fresh, #3 local-first, #7 no-unsafe)
