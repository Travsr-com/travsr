# RFC-013: Open Language Identity & Runtime Language Pluggability

**Status:** Draft
**Author:** Abhishek
**Date:** 2026-06-04
**Phase:** Cross-cutting (architecture gate; phased implementation)
**Crate(s) affected:** `travsr-core`, `travsr-plugin-protocol`, `travsr-plugin-sdk`, `travsr-plugin-host`, `travsr-indexer`, `travsr-cli`
**Related:** RFC-003 (multi-language indexer), RFC-008 (config-driven Phase A — the promise this RFC reconciles with reality), RFC-011 (two-transport plugin architecture — the implementation that left the gap), ADR-005 (per-language corpus naming), ADR-009 (SCIP vs LSIF wire format), ADR-017 (unified plugin sandbox & trust model). **New ADRs this RFC requires:** ADR-018 (plugin resource governance), ADR-019 (language subscription & verdict cache), ADR-020 (plugin binary distribution & signing).
**External repo affected:** `travsr-lang` (consumes published `travsr-core` + `travsr-plugin-sdk`)

---

## ⚠️ Living Document Notice

This RFC drives a multi-sprint implementation across the published boundary, the
plugin host, and the `travsr-lang` repo. **The design below is the intended
shape, not a frozen contract.** Real implementation will surface things the
design did not anticipate — a protocol field that needs to change, a sandbox
knob the OS does not expose, a verdict state we missed.

**When implementation deviates from this design, the deviation is recorded in
[§13 Change Log & Implementation Deviations](#13-change-log--implementation-deviations),
not silently absorbed.** Each deviation gets a dated row: what changed, why, and
which section it supersedes. The body of the RFC is updated in place *and* the
change is logged, so the document stays the single source of truth while
preserving the trail of why it drifted. Do not treat a mismatch between this RFC
and the code as "the RFC is stale" — treat it as a missing Change Log row.

---

## Summary

Adding a **genuinely new language** to Travsr today requires editing the
**published boundary crates** and recompiling the `travsr` binary, which forces a
`travsr-lang` re-publish. This contradicts the additive-extension promise of
RFC-008 ("adding a new language no longer requires modifying source code").

This RFC does three things:

1. **Opens language identity** at the boundary so the closed `enum Language` (and
   the `Plugin` trait that returns it) stops being the gate.
2. **Adopts out-of-process grammars (Direction A):** a new language ships as a
   self-contained `travsr-lang-*` plugin binary that links *its own* Tree-sitter
   grammar and answers the existing parse protocol. The host links **zero**
   grammars for non-builtin languages. This reuses the sidecar mechanism RFC-011
   *already built* rather than inventing a new one.
3. **Introduces a runtime subscription + verification model:** a language plugin
   **subscribes** to the host; the host **authorizes** (ADR-017 trust), **probes**
   it for correctness, and admits it with a **tiered verdict** — or quarantines it
   with a diagnostic. Open identity without this gate would let a broken plugin
   inject malformed nodes; the probe is what makes open identity *safe*.

After this lands, adding a Phase-B-tooled / generic-Phase-A language — and any
language with its own grammar plugin — is a `travsr-lang` drop plus a catalog
entry: **zero boundary edits, zero core rebuild, zero `travsr-lang` re-publish.**

---

## Motivation

### The promise vs. the implementation

- **RFC-008** generalised Phase A to be config-driven and declared new languages
  "slot in additively" with no `travsr-indexer` source edits.
- **RFC-011** shipped the two-transport `Plugin` contract (`travsr-plugin-protocol`
  / `-sdk` / `-host`) — including a `Sidecar` transport that spawns a sandboxed
  child and speaks the codec.
- **Reality:** the language *identity* remained a **closed set** baked into the
  published boundary, and the **grammar** stayed statically linked into the host.
  A plugin cannot introduce a language core does not already enumerate; its nodes
  are rejected at handshake.

### The closed-boundary chain (verified, with file:line)

The RFC's original diagnosis found the *symptom*. The verified root cause is one
level deeper — in the published `Plugin` trait itself:

1. **`travsr-plugin-protocol/src/plugin.rs:7`** — `fn language(&self) -> Language;`
   The published trait third parties build against **forces the closed enum**. A
   plugin author physically cannot *name* a language outside the 12 before any
   dispatcher logic runs. → **the deep lock** (boundary crate).
2. `travsr-core/src/lib.rs:116` — `enum Language` is a fixed 12-variant set;
   `from_extension`/`as_str`/`from_str` are exhaustive matches. `#[non_exhaustive]`
   stops *external* exhaustive matches only; inside the workspace it is closed.
   → **boundary crate**.
3. `travsr-plugin-protocol/src/language_map.rs:5` — `language_from_proto_str` is
   `match … _ => None`. → **boundary crate**.
4. `travsr-plugin-host/src/dispatcher.rs:39` — a handshake whose language maps to
   `None` returns `IndexError::UnknownLanguage`; the plugin's nodes are refused
   outright. → the *symptom* the original RFC found.
5. `travsr-plugin-sdk/src/runner.rs:31` — `plugin.language().as_str()` fills the
   handshake `language` field, propagating the closed enum onto the wire.

Because #1–#3 live in the published boundary, a new language **bumps
`travsr-core` + `travsr-plugin-protocol`**, which `travsr-lang` pins at `^0.7.0`.
That forces `travsr-lang` to update pins, rebuild, and re-publish — the
separation-of-concerns violation this RFC fixes.

### The Tree-sitter constraint (why it is more than the enum)

`travsr-plugin-host/Cargo.toml` statically links `tree-sitter-<lang>` C grammars
(java, kotlin, ruby, c#, php, scala, cpp, c). Opening the enum alone does **not**
make a new language a runtime drop-in — the structural layer still needs the
grammar in the binary, a core rebuild. This RFC resolves that by **moving the
grammar out of the host and into the plugin binary** (Direction A, §4), not by
keeping it compile-time.

### What already exists (so this is "finish & unbind," not greenfield)

- `ParseResponse { nodes: Vec<Node>, edges: Vec<Edge> }` (`protocol/types.rs:23`) —
  **plugins already own node/edge creation.**
- `VName::new(corpus, root, path, language, signature)` (`core/lib.rs:211`) is
  published with signature versioning (RFC-002) — the **invariant-preserving VName
  constructor is already on the boundary**, and `language` is stringly-typed, so
  the store is unaffected.
- `Sidecar` transport (`host/transport.rs:98` `spawn`, `:224` `Transport::parse`)
  spawns a sandboxed child, handshakes, and speaks the codec. (Note: a legacy
  `Sidecar::stub` at `:80` still carries a `"not yet implemented (P5-S3)"` health
  string — superseded by `spawn`; to be removed during implementation, see §11.)
- `run_plugin<P: Plugin>()` (`sdk/runner.rs:9`) is the event loop a plugin binary
  runs.
- `trust.rs` already reads `~/.travsr/lang.toml` (`from_disk`,
  `registered_languages_from_disk`) and holds corpus trust (`TrustConfig::is_trusted`).
- `phase_b/catalog.rs` already models per-language Phase B tooling (`command`,
  `sandbox: RequiresElevated`, `elevated_hosts`, `install_hint`, `scip_install`,
  `builtin`) — the host-prerequisite declaration this RFC promotes to runtime.

The heavy machinery shipped. What is missing is: open identity, runtime
subscription/verification, a verdict cache, and resource governance.

---

## The two layers (the frame for every option)

| Layer | What it produces | Pluggability after this RFC |
|---|---|---|
| **Phase A (structural)** | Tree-sitter AST → nodes/edges | **Runtime — grammar lives in the plugin binary** (Direction A). Host links zero non-builtin grammars. |
| **Phase B (semantic)** | SCIP/LSIF tool output over the protocol | **Runtime.** External analyzer the plugin orchestrates; its toolchain (JDK, build tool) is a declared host prerequisite, never bundled. |

A language is one of:
- **Builtin** — Phase A grammar + adapter compiled in-host for the hot path
  (ts/js, rust, python). Stays InProcess.
- **External plugin** — Phase A grammar in its own `travsr-lang-*` binary; Phase B
  via an external analyzer. Subscribes at runtime. (go, java, kotlin, …)

---

## Options (history — see Decision for the chosen path)

- **Option 1 — "new language = core release," made honest.** Open identity, keep
  grammars compile-time. Smallest, but net-new grammar languages still ship with
  core.
- **Option 2 — runtime-loadable grammars in-host (WASM/`dlopen`).** Full
  pluggability, but `dlopen` is forbidden-`unsafe` (CLAUDE.md), WASM adds a second
  plugin mechanism + wasmtime weight + perf cost.
- **Option 3 — open identity now, grammars later.** The original recommendation.

**Superseded.** Discussion established that the sidecar mechanism RFC-011 already
built makes a *third* path the cheapest *and* the most complete: run the grammar
**out-of-process inside the plugin** (below). This is neither "keep it
compile-time" (Option 1/3) nor "load grammars into the host" (Option 2) — it
moves grammar linkage into the per-language binary we already spawn.

---

## Decision

**Adopt Direction A — out-of-process grammars via the existing sidecar — plus
open identity and a runtime subscribe/verify model.**

Concretely, the agreed sub-decisions:

| # | Decision | Rationale |
|---|---|---|
| D-1 | **Open language identity** at the boundary (`LanguageId`; `Plugin::language() -> LanguageId`). | The one-time boundary fix; root cause is the trait, not just the dispatcher. |
| D-2 | **Out-of-process grammar (Direction A).** Non-builtin grammars link into `travsr-lang-*` binaries; host links none of them. | Reuses the shipped sidecar; no `unsafe`; crash + memory isolation; independent shipping. WASM (Option 2) kept only as a future escape hatch if IPC cost proves unacceptable. |
| D-3 | **Keep 4 builtin, rest become plugins.** ts/js/rust/python stay InProcess; go/java/kotlin/ruby/c#/php/scala/cpp/c migrate to Sidecar plugins. | Hot path stays in-process; everything else gains isolation + independent cadence. |
| D-4 | **Runtime subscription + verification** with a tiered verdict, replacing compile-time `register_builtins`. | Makes open identity safe — a broken/garbage-emitting plugin is quarantined, not admitted. |
| D-5 | **Verdict cache** in `~/.travsr`, keyed by binary hash; environmental failures never poison a binary's reputation. | Correct binaries stay subscribable across transient system errors; reproducible binary faults stick. |
| D-6 | **Host-prerequisite declaration + graceful degradation.** Phase B's toolchain (JDK, build tool) is declared and discovered, never bundled; missing toolchain → Phase-A-only, not failure. | Java with no JVM still yields a correct structural graph. |
| D-7 | **Shared analyzers as a first-class entity, with auto-onboarding.** A Phase B analyzer (e.g. `scip-java`) is stored once and *referenced* by languages, not replicated per language. A language whose declared analyzer is already verified **auto-onboards silently** (Phase B verified in the background); trust to execute is keyed **per (corpus, analyzer)** and covers every language that analyzer serves. | `scip-java` indexes the whole JVM build in one run; Java/Kotlin/Scala share that one invocation. De-dups install + execution, no second trust prompt. (Resolves former Open Question #4.) |

---

## Detailed Design

### D1 — The boundary break (one-time)

1. **`travsr-core`** — introduce `LanguageId`: a validated, lowercase string
   newtype (`Cow<'static, str>` to start; interning is a later optimization, see
   Open Questions). Keep `enum Language` as a **builtin fast-path / grammar
   selector only**, with `Language → LanguageId` and `LanguageId → Option<Language>`.
   `nodes.language` already stores a string — **store unaffected**.
2. **`travsr-plugin-protocol`** —
   - `Plugin::language(&self) -> LanguageId` (was `-> Language`). **This is the
     load-bearing change**: it frees the published trait so a plugin can name a
     language core never enumerated.
   - `language_from_proto_str(&str) -> LanguageId` (validate format; do not reject
     unknowns). `Language` lookup becomes a secondary, non-gating step.
   - Bump `PROTOCOL_VERSION` (this is a wire/trait break; see §8 supply chain).
3. **`travsr-plugin-host/dispatcher.rs`** — `register()` accepts any well-formed
   `LanguageId` from a **trusted, verified** plugin instead of returning
   `UnknownLanguage`. Eligibility is enforced by subscription (D3) + ADR-017 trust,
   **not** enum membership.
4. **`travsr-plugin-sdk/runner.rs:31`** — emit the plugin's declared `LanguageId`,
   not `Language::as_str()`.

**Boundary stability policy (the separation guarantee):**
`travsr-core`, `travsr-plugin-protocol`, `travsr-plugin-sdk` are the **published
boundary**; their public API + `PROTOCOL_VERSION` are the `travsr-lang`
compatibility contract. Feature work lands in non-boundary crates
(`travsr-plugin-host`, `travsr-indexer`, `travsr-cli`). **CI guard:** a PR
diffing a boundary crate fails unless it carries an explicit `BOUNDARY-CHANGE:`
note justifying the version implication. After D1 lands once, language additions
require **no** boundary edits.

### D2 — Out-of-process grammar (Direction A)

A `travsr-lang-<x>` plugin binary is **self-contained for Phase A + protocol +
Phase B orchestration**:

- Links its own `tree-sitter-<x>` grammar (the plugin author's crate, not core —
  `unsafe`-free as far as core is concerned).
- `impl Plugin { fn language() -> LanguageId("x"); fn parse() -> ParseResponse }`,
  building `Node`/`Edge` via the **published `travsr_core::VName::new`** (never a
  hand-rolled VName — see invariant guard below).
- `fn main() { travsr_plugin_sdk::run_plugin(XPlugin) }`.

The host spawns it as a sandboxed `Sidecar` (already implemented), receives
`ParseResponse`, and merges nodes/edges. **The host links zero grammars for
non-builtin languages.**

**Invariant guards (Principal Architect, non-negotiable):**
- **VName uniqueness** — plugins MUST mint VNames via `travsr_core::VName::new`.
  A conformance test asserts a plugin's VNames match the in-host reference for a
  golden file.
- **Incremental correctness** — an out-of-process parser MUST be **deterministic**
  given identical bytes (no map-iteration order in emitted edges, no timestamps).
  Enforced by the determinism probe (D4).

### D3 — Subscription lifecycle (replaces compile-time `register_builtins`)

`registry.rs:93 register_builtins()` is subscription, hardcoded. This promotes it
to a runtime state machine per language:

```
SUBSCRIBE  →  AUTHORIZE  →  VERIFY  →  ADMIT / QUARANTINE
(handshake)   (trust)       (probe)    (register / diagnostic)
```

1. **SUBSCRIBE** — handshake declares `{ LanguageId, extensions, capabilities,
   protocol_version, binary_hash, plugin_version, phase_b_prerequisites }`.
2. **AUTHORIZE (precedes execution)** — ADR-017: is this binary allowed to run at
   all? Catalog entry + trust + (future) signature. **A binary is not executed to
   "test if it works" unless trust already cleared it** — running untrusted code
   to probe it *is* the vulnerability (PSE). The probe runs *inside* the sandbox.
3. **VERIFY** — the conformance probe (D4), inside the sandbox, bounded.
4. **ADMIT / QUARANTINE** — register extensions → transport and cache the verdict
   (D5), or quarantine with a surfaced diagnostic.

The 4 builtins subscribe via the InProcess fast path (skip spawn; still run the
conformance probe once in CI).

### D4 — Verdict taxonomy & the reproducibility rule

`PluginHealth { Ok, Disabled(String) }` (`transport.rs:12`) is replaced by a
richer verdict. **A failure caused by the binary and a failure caused by the
machine are cached differently** — this is what keeps a correct binary subscribable
through a transient system error:

| Verdict | Cached & hash-keyed? | Blocks re-subscribe of same binary? | Reasons |
|---|---|---|---|
| `Active(tier)` | yes | — | passed (tier = A+B or A-only, see D6) |
| **`Rejected` (HARD)** | yes | yes — needs a *changed* binary or `--force` | `ConformanceFailed`, `NonDeterministic`, `ProtocolSkew` — **reproducible binary properties** |
| **`Unverified` (SOFT)** | **no** — not attributed to the binary | **no — auto re-probed** | `Timeout`, `ResourceExceeded`, `SandboxUnavailable`, `ProbeIoError`, `SpawnFailed` — **machine properties at probe time** |

**Probe tiers (what "usable" means):**
- *Liveness* — spawns, handshakes, `PROTOCOL_VERSION` matches, declares a `LanguageId`.
- *Structural conformance* — well-formed `Node`/`Edge`; every VName a valid 5-tuple
  via `VName::new`; edges reference emitted nodes; `node.language == declared
  LanguageId`. **Protects VName uniqueness.**
- *Determinism* — parse same bytes twice → byte-identical `ParseResponse`.
  **Protects incremental correctness.** *(Selected gate: structural + determinism.)*

**Honest ceiling:** for a net-new language we have **no oracle** — we cannot verify
the grammar parses the language *correctly*, only that output is structurally
valid, deterministic, and self-consistent. Semantic correctness stays the plugin
author's test suite. The plugin **ships its own golden fixture**; we assert
validity + determinism, not content.

**The reproducibility rule (disambiguates ambiguous failures like a crash):**

> Run the probe with in-attempt retry + backoff (N attempts). A failure is
> recorded **HARD `Rejected`** only if it reproduces across all attempts. A
> failure that clears on retry is **SOFT `Unverified`** and not cached against the
> binary. Reproducible = binary-intrinsic = sticky.

This is the mirror of the determinism check (parse-twice → identical; here,
fail-twice → confirm). A binary that segfaults every time is rejected; a one-off
OOM-kill is absorbed and retried.

**Pre-flight environment gate:** before judging the binary, check preconditions we
own (sandbox available, scratch creatable, resource headroom). If *those* fail,
the verdict is against the **environment** ("cannot verify `x`: sandbox
unavailable"), never the binary.

**Manual escape hatch:** `travsr languages verify <lang> --force` forces a fresh
probe regardless of any cached verdict (including overriding a HARD `Rejected`),
for the residual misclassification / "I fixed my environment" case.

### D5 — Verdict cache (where & invalidation)

Machine-local, **not** in the repo (a verdict is binary- and arch-specific; a
committed verdict is an attack vector):

```
~/.travsr/lang.toml          ← exists: corpus trust + registered langs
~/.travsr/subscriptions.lock ← NEW: verdict cache  (TRAVSR_LANG_FILE-aware)
```

Each entry keyed by the tuple that makes a verdict valid:
```
(LanguageId, binary_path, sha256(binary), plugin_version,
 protocol_version, probe_kit_version)
  → { verdict, golden_fixture_hash, sandbox_policy, verified_at }
```

Clean separation of two questions that must not be conflated:
- **"Is this binary structurally sound?"** → global, hash-keyed verdict cache.
- **"Is *this repo* allowed to load it?"** → per-corpus trust (`trust.rs`, exists).

**Delete / disable semantics (answers "re-subscribe same binary?"):**
- *Deleted binary* → spawn fails → `Unavailable{BinaryMissing}`, **verdict
  retained**. A byte-identical binary returning at any path → hash hit → cached
  `Active` applies immediately, **zero re-probe**. A different binary → hash miss
  → full re-subscribe. (Re-hash gated on `mtime+size` to stay cheap.)
- *User-disabled* (`travsr languages disable <lang>`) → a separate **policy
  override** superseding the verdict; re-enable flips it back with no re-probe if
  hash unchanged.

**Re-verification triggers (complete matrix):** binary hash change · plugin_version
change · `PROTOCOL_VERSION` bump · `probe_kit_version` bump. Verdicts do **not**
expire by time — a verified binary stays verified.

### D6 — Host prerequisites & tiered / graceful-degradation verdicts

A language needs three things in three places; only the first is bundled:

| Tier | Example (Java) | In the plugin binary? | Lives where |
|---|---|---|---|
| Phase A grammar | `tree-sitter-java` | ✅ yes | the plugin binary |
| Phase B analyzer | `scip-java` | ❌ no — declared, installed, on PATH | user's machine |
| Language runtime + build env | JDK + Gradle/Maven/sbt + resolved classpath + Maven network | ❌ **never** — per-project, gigabytes | user's machine + project |

`scip-java` *drives the project's actual build* to resolve the classpath — that is
per-project, network-resolved, version-pinned, and cannot be bundled. So the
plugin **declares** its analyzer + runtime prerequisites (the catalog's `command`
/ `sandbox` / `elevated_hosts` / `install_hint` fields, promoted into the
handshake), and VERIFY yields a **tiered verdict**:

- `Active(A+B)` — grammar verified **and** analyzer + toolchain present.
- `Active(A only) + PhaseB-Unavailable{prereq}` — **structural graph still works**;
  deep semantic edges deferred until the toolchain appears, with the exact install
  hint surfaced. *Graceful degradation: Java with no JVM is degraded, not broken —
  the graph is never wrong, only less deep.*
- `Rejected` — reserved for Phase A / the binary itself failing.

A missing toolchain is **environmental** (D4 SOFT class): Phase B sits in
`Unavailable{prereq}` and **auto-upgrades to `Active(A+B)` when the toolchain is
installed — zero change to the plugin binary.** Phase B is verified at the
**protocol level** (emits valid SCIP/LSIF, VNames valid) + prerequisite
satisfiability — not semantic completeness (build-dependent, no oracle).

### D6a — Shared analyzers & auto-onboarding (resolves former Q4)

One analyzer serves several languages: `scip-java` indexes the **whole JVM build**
in one run, covering Java, Kotlin, and Scala together. So the analyzer is **not** a
per-language thing — it is a **first-class, shared entity** that languages
*reference*, never replicate. (Today the catalog already duplicates
`command: "scip-java"` across the java and kotlin rows — this de-duplicates it.)

**Phase A is per-language; Phase B is per-analyzer/build, shared:**
- Each language keeps its own grammar plugin, `LanguageId`, and structural graph.
- One analyzer is installed once (shared store, `~/.travsr/analyzers/…` — on-disk
  layout is an open question; the **verdict keys on `sha256(analyzer)` regardless**)
  and invoked **once per build root**, producing semantic edges for every
  `LanguageId` it covers.

**"Verified" is two facts, only one of which is reused (the invariant guard):**

| Fact | Scope | Reused across languages? |
|---|---|---|
| Analyzer binary is authentic + sound | per `sha256(analyzer)`, global | ✅ yes — the de-dup |
| Analyzer emits valid SCIP **for this language** | per `(analyzer-hash, LanguageId)`, global | ❌ no — first-time pairing check per language |

scip-java being verified for Java does **not** prove it handles Scala; the
first-time-per-language pairing check (valid SCIP + VNames over the language's
fixture) is what keeps auto-onboarding honest.

**Trust to execute is keyed per `(corpus, analyzer)` (D-7).** Running scip-java
*executes the project build* — code execution, gated per-corpus by ADR-017 Rule 3.
But because Java/Kotlin/Scala are **one** scip-java invocation, trusting the
analyzer for a corpus once **covers every language it serves** — no second prompt.

**Auto-onboard silently, verify Phase B in the background — without admitting
unverified output:**
- A language whose declared analyzer is already verified + corpus-trusted
  **auto-onboards with no prompt.** Phase A activates **immediately** (always safe).
- The `(analyzer-hash, LanguageId)` pairing check runs in the **background**;
  **Phase B semantic edges are held out of the graph until it passes.** "Silent"
  means no user prompt — **not** "merge unverified nodes." On pass → Phase B
  activates; on fail → Phase B stays off with a surfaced diagnostic; Phase A is
  unaffected throughout (graceful degradation, D6).
- **Manual override is the escape hatch:** the user may rebind a language's analyzer
  (id / version / path) per-language (global default) or per-corpus. A user-pointed
  arbitrary binary is a `--command`-class trust decision (ADR-017/020) and carries
  **no authenticity guarantee** unless org-signed. A rebind re-verifies Phase B.

**Concentrated blast radius (security note):** a shared analyzer means one tampered
`scip-java` poisons three languages — so the analyzer is **hash-pinned in the
verdict and re-verified at spawn**, never trusted by path alone (ADR-019/020).

This is owned in detail by **ADR-019** (analyzer verdict facets, pairing check,
auto-onboard, per-`(corpus, analyzer)` trust) and **ADR-020** (analyzer signing,
user-pointed-analyzer caveat).

### D7 — Resource governance (DevOps; ADR-018)

`supervisor.rs` today is crash-count only (`record_crash`, `is_disabled`);
`SandboxPolicy` (`Standard | Elevated{…}`) has **no** memory/CPU/time knobs. This
RFC adds:

- **Per-plugin memory ceiling** — Linux `RLIMIT_AS` / cgroup `memory.max`. Breach →
  kill → `ResourceExceeded`.
- **Wall-clock deadline** on probe and on each parse — kill on hang → `Timeout`.
- **Recycling** — restart the sidecar after *N files* or *M MB RSS* (PHP-FPM/
  Gunicorn `max_requests` pattern); bounds any leak, including in third-party C
  grammar code we cannot fix.
- **Idle spin-down** — no in-flight files for a TTL → kill, release memory; respawn
  on demand (cheap; verdict cached).
- **macOS gap (explicit):** `sandbox-exec` cannot cap memory. A **supervisor
  watchdog** samples child RSS and kills on breach. Net-new supervisor work,
  tracked in ADR-018.

The daemon is structurally insulated: a plugin leak lives in the **plugin's
address space**, reclaimed on respawn — the key advantage of Direction A over
WASM-in-host (Option 2), where a leak would grow the daemon's heap.

### D8 — Security model (PSE; threat rows)

- **Authorize before execute** (D3) — ADR-017 trust precedes any spawn; the probe
  runs inside the sandbox. Fail-closed if the sandbox tool is missing — **never**
  "run it anyway."
- **Plugin binary distribution & signing (ADR-020):** plugin binaries are a
  supply-chain target (a malicious `travsr-lang-x` indexes the user's private
  code). Require: cosign signature on every `travsr-lang-*` release; the catalog
  entry pins an expected signing identity; AUTHORIZE verifies the signature before
  spawn. SLSA provenance as for the core binary.
- **`PROTOCOL_VERSION` bump** is a breaking change → coordinated `travsr-lang`
  release + deprecation note (DevOps SemVer policy).
- **Determinism + sandboxing** already enforce no-network/no-FS-escape during
  parse; `Elevated` (network for build tools) stays PSE-gated per existing
  `SandboxPolicy::Elevated{ approved_by, … }`.

**Threat-model rows (the detailed rows live in the ADRs that own each mechanism;
global numbering continues past ADR-017's T11–T13):**

| # | Asset | Threat | Owning ADR |
|---|---|---|---|
| T11 (extended) | Plugin binary | Malicious/compromised `travsr-lang-*` indexes private code or escapes sandbox | **ADR-020** (signing) + ADR-017 (sandbox) |
| T14–T16 | Daemon availability / memory | Plugin leak / OOM / hang / giant output starves the daemon | **ADR-018** (resource governance) |
| T17 | Verdict cache | Tampered `subscriptions.lock` admits an unverified binary | **ADR-019** (cache advisory only) |
| T18 | Open `LanguageId` | A plugin emits nodes under a spoofed/colliding language | **ADR-019** (probe asserts `node.language`) |
| T19 | Availability / DoS | A flaky plugin is re-probed in a tight loop, repeatedly spawning untrusted code | **ADR-019** (SOFT re-probe on trigger only) |
| T20–T21 | Binary in transit / wire contract | Tampered binary between build and machine; `protocol_version` mismatch mis-decodes into forged nodes | **ADR-020** |

### D9 — Java before/after (concrete walkthrough)

| Aspect | Today | After |
|---|---|---|
| Identity | `Language::Java` (closed gate) | `LanguageId("java")` (runtime) |
| Phase A grammar | linked into **host binary** | linked into **`travsr-lang-java`** |
| Execution | InProcess (`registry.rs:127`) | sandboxed Sidecar |
| Registration | compile-time macro | runtime subscribe + verify |
| Verification | none — assumed correct | structural + determinism, cached by hash |
| Phase B (scip-java + JDK) | external, static catalog row | external, **declared in handshake** (deps unchanged) |
| Missing JDK | Phase B silently skipped | explicit `Active(A only)` + install hint |
| Fix Java grammar | **rebuild core, re-publish travsr-lang** | **ship new `travsr-lang-java`, nothing else** |
| Grammar segfault | can crash the daemon | isolated to the sidecar |

The graph output for Java is **identical** (same `Node`/`Edge`/VNames). What
changes is decoupling, isolation, and an honest degraded state — not the deps and
not the graph.

---

## Implementation Plan (Tech Lead)

Branch convention: `feature/<crate>-rfc013-<slice>`. Each slice is independently
mergeable, CI-green, with the `BOUNDARY-CHANGE:` note where it touches the
boundary. Estimates use the standard scale (S=½d, M=1–2d, L=3–4d, XL=full sprint).

### Phase 0 — ADRs & gates (pre-work)
```
[ ] TL/PA:  ADR-018 plugin resource governance               (M)
[ ] TL/SEC: ADR-019 subscription & verdict cache              (M)
[ ] SEC:    ADR-020 plugin binary distribution & signing      (M)
[ ] DevOps: CI BOUNDARY-CHANGE: guard (fail PR diffing boundary w/o note)  (M)
```

### Phase 1 — Open identity (the one-time boundary break)  [Sprint A]
```
[ ] SWE:  travsr-core: LanguageId newtype + Language<->LanguageId  (M)  BOUNDARY
[ ] SWE:  protocol: Plugin::language() -> LanguageId               (M)  BOUNDARY
[ ] SWE:  protocol: language_from_proto_str -> LanguageId (validate, not reject) (S) BOUNDARY
[ ] SWE:  sdk/runner.rs: emit declared LanguageId                 (S)  BOUNDARY
[ ] SWE:  host/dispatcher.rs: accept well-formed LanguageId from trusted plugin (M)
[ ] SWE:  bump PROTOCOL_VERSION; update travsr-lang pins          (S)  BOUNDARY
[ ] QA:   identity round-trip + unknown-language-accepted tests   (M)
[ ] SEC:  review boundary diff (NEEDS_THREAT_MODEL → T13)         (review)
```
*Exit:* a plugin can declare a non-enumerated language and reach the dispatcher.

### Phase 2 — Subscription + verification engine  [Sprint B]
```
[ ] SWE:  Verdict enum (Active(tier) | Rejected | Unverified) replacing PluginHealth (M)
[ ] SWE:  Subscription state machine: SUBSCRIBE→AUTHORIZE→VERIFY→ADMIT (L)
[ ] SWE:  Conformance probe kit (structural + determinism) + probe_kit_version (L)
[ ] SWE:  Reproducibility rule (retry+backoff; HARD vs SOFT classification) (M)
[ ] SWE:  Pre-flight environment gate                            (M)
[ ] SWE:  Verdict cache ~/.travsr/subscriptions.lock (hash-keyed, mtime+size) (L)
[ ] SWE:  Delete/disable/re-verify-trigger handling + `--force`  (M)
[ ] SWE:  SDK conformance test harness (plugin authors run it)   (M)
[ ] QA:   transient-vs-sticky failure matrix; cache invalidation; golden-fixture (L)
[ ] SEC:  authorize-before-execute review; T12/T14 mitigations   (review)
```
*Exit:* `register_builtins` is replaced by runtime subscription; a garbage-emitting
plugin is quarantined; a correct binary survives a simulated transient failure.

### Phase 3 — Resource governance  [Sprint B/C, ADR-018]
```
[ ] SWE/DevOps: per-plugin memory ceiling (RLIMIT_AS / cgroup)   (L)
[ ] SWE/DevOps: wall-clock deadline on probe + parse             (M)
[ ] SWE:        recycling (N files / M MB RSS) + idle spin-down  (L)
[ ] SWE/DevOps: macOS RSS watchdog in supervisor                 (L)
[ ] QA:         leak/OOM/hang simulation; recycle correctness    (M)
```

### Phase 4 — Host prerequisites & tiered verdicts  [Sprint C]
```
[ ] SWE:  promote catalog (command/sandbox/elevated_hosts/install_hint) into handshake (M)
[ ] SWE:  tiered verdict Active(A+B) / Active(A only) + auto-upgrade on toolchain appear (L)
[ ] SWE:  Phase B protocol-level conformance + prereq satisfiability (M)
[ ] SWE:  shared analyzer entity + store; analyzer verdict keyed by sha256 (D6a) (L)
[ ] SWE:  (analyzer-hash, LanguageId) pairing check; silent auto-onboard, Phase B held until pass (L)
[ ] SWE:  trust per (corpus, analyzer); manual rebind override (per-lang/per-corpus) (M)
[ ] QA:   missing-JDK degradation; toolchain-appears upgrade path (M)
[ ] QA:   auto-onboard: Kotlin/Scala ride Java's verified scip-java; Phase B not merged pre-pairing (L)
```

### Phase 5 — Builtin re-split (4 builtin + rest plugins)  [Sprint D, incremental]
```
[ ] DevOps: scaffold travsr-lang-* binary template + CI matrix (ARM64+x86) (L)
[ ] SEC:    cosign signing of travsr-lang-* releases (ADR-020)   (M)
[ ] SWE:    pilot: migrate ONE language (go) host→travsr-lang-go sidecar; remove its grammar from host Cargo.toml (L)
[ ] QA:     graph-equivalence: in-host vs sidecar produce identical Node/Edge/VName for golden repo (L)
[ ] SWE:    migrate remaining: java, kotlin, ruby, c#, php, scala, cpp, c (XL — break per language)
[ ] SWE:    remove migrated tree-sitter-* from host Cargo.toml; delete Sidecar::stub legacy path (M)
[ ] DevOps: travsr languages CLI/view surfacing verdicts (ties VSCODE-247 follow-up) (M)
```
*Exit:* host links only ts/js/rust/python grammars; the other 8 ship as signed,
subscribed sidecar plugins; `travsr languages` shows per-language tiered verdicts.

### Sequencing notes
- Phase 1 unblocks everything and is the only mandatory boundary change.
- Phases 2–4 are host-internal (no boundary edits) and can overlap.
- Phase 5 is incremental and demand-driven — pilot with `go` (Standard sandbox,
  self-contained tool) before the `RequiresElevated` JVM languages.

---

## Non-goals

- **Runtime-loadable grammars *in the host*** (WASM/`dlopen`, the old Option 2).
  Kept only as a future escape hatch if Direction-A IPC cost proves unacceptable
  under benchmark; would be its own RFC.
- Changing the Phase B wire format (ADR-009) or the core sandbox/trust model
  (ADR-017) — this RFC *extends* ADR-017 via ADR-018/019/020, it does not replace it.
- Re-architecting the 4 builtins away from InProcess.

---

## Open questions

1. `LanguageId` representation: start `Cow<'static, str>`; intern hot-path strings
   later? (Decision: `Cow` first, measure, intern if the parse loop shows cost.)
2. Phase-B-only languages (no Phase A grammar, no generic fallback): minimum
   structural graph such a language yields? Do we admit them?
3. Trust for a non-enumerated `LanguageId`: is catalog entry + ADR-017 corpus trust
   sufficient, or do we want a separate per-language allowlist? (Lean: catalog
   entry *is* the allowlist.)
4. ~~Cross-language analyzer sharing~~ — **RESOLVED (D-7 / D6a):** shared
   first-class analyzer, one invocation per build root, trust per `(corpus,
   analyzer)`, silent auto-onboard with background Phase-B verification.
   *Still open within this:* the on-disk analyzer store layout — content-hash dir
   vs `name@version` dir vs hybrid (the verdict keys on `sha256` either way).
5. Probe retry count N and backoff schedule for the reproducibility rule — tune
   against real flake rates.
6. macOS memory enforcement fidelity — is an RSS-sampling watchdog tight enough,
   or do we need a helper (e.g., a launchd-managed cgroup-equivalent)?

---

## Decision Record

_Pending sign-off._

- **Principal Architect:** open identity + Direction A + invariant guards (VName,
  determinism) — _pending_.
- **Principal Security Engineer:** authorize-before-execute, signing (ADR-020),
  threat rows T11 (ext)/T14–T21 — _NEEDS_THREAT_MODEL on Phase 1 boundary diff_.
- **Tech Lead:** phasing + boundary-stability CI guard — _pending_.
- **DevOps:** resource governance (ADR-018) + plugin distribution matrix — _pending_.

Commit target: master after PR #271 merges (per project plan), as
`[docs] RFC-013: open language identity + runtime language pluggability`.

---

## 13. Change Log & Implementation Deviations

> Append a dated row whenever implementation diverges from the design above.
> Format: **date — section affected — what changed — why.** Update the relevant
> section in place *and* log here. An undocumented mismatch between this RFC and
> the code is a missing row, not a stale RFC.

| Date | Section | Change | Why |
|---|---|---|---|
| 2026-06-04 | — | RFC reframed from "Option 3, defer Option 2" to "Direction A: out-of-process grammars + subscribe/verify." Root cause refined to `Plugin::language() -> Language` (`plugin.rs:7`), deeper than the dispatcher symptom. | Discussion established the shipped sidecar makes Direction A the cheapest *and* most complete path; the trait — not just the dispatcher — is the gate. |
| 2026-06-04 | D8 | Threat rows renumbered: RFC's draft T11–T14 collided with ADR-017's existing global T11–T13. Detailed rows moved into the owning ADRs; RFC now references canonical numbers T11 (ext), T14–T16 (ADR-018), T17–T19 (ADR-019), T20–T21 (ADR-020). | The T-table is a global, project-wide sequence; ADR-017 already claimed T11–T13. |
| 2026-06-04 | refs | ADR-018 / ADR-019 / ADR-020 drafted (`docs/adrs/`) on branch `rfc/013-language-pluggability`. | The three dependency decisions the RFC named are now written. |
| 2026-06-04 | D-7 / D6a / Q4 | Open Question #4 resolved: shared first-class analyzer (one invocation per build root), trust per `(corpus, analyzer)`, silent auto-onboard with background Phase-B pairing verification (output held until pass). New D-7 row, D6a section, Phase-4 tasks; ADR-019/020 patched. Storage-layout sub-question remains open. | User chose shared analyzer + auto-onboard; `scip-java` indexes the whole JVM build in one run, so per-language replication is wrong. |
| _next_ | _e.g. D4_ | _e.g. added `Quarantined{IncompatibleAbi}` verdict_ | _e.g. discovered during Phase 2 that a grammar ABI mismatch needs its own sticky reason_ |
