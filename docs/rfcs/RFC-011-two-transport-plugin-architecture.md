# RFC-011: Two-Transport Language Plugin Architecture

**Status:** Proposed
**Author:** Principal Architect
**Date:** 2026-05-30
**Phase:** 4 (Sprint 12 design gate; implementation S12–S16)
**Crate(s) affected:** `travsr-core`, `travsr-indexer`, `travsr-ingest`, `travsr-daemon`, `travsr-plugin-protocol` (new), `travsr-plugin-sdk` (new)
**Supersedes (in part):** RFC-003 §2 (hardcoded `LanguageIndexer` implementors), RFC-003 §3 (enum-match dispatcher), RFC-008 §2 (`LanguageDescriptor` TOML loader + `GenericTreeSitterIndexer`)
**Related:** RFC-005 (cross-language edge resolution), RFC-008 (multi-language extension architecture — the umbrella this RFC redirects), RFC-009 (cross-language bridge plugins), ADR-005 (per-language corpus naming), ADR-009 (SCIP vs LSIF wire format), ADR-010 (`travsr-ingest` crate boundary), **ADR-017 (unified plugin sandbox & trust model — MUST merge in the same sprint)**

---

## Summary

Decouple **language dispatch** from the **trust boundary**. Today (RFC-003) and as planned (RFC-008), the indexer conflates "which language is this file?" with "where does the parsing code run?" — both are answered inside `travsr-indexer` by hardcoded implementors and an enum-match dispatcher, and Phase B (subprocess) trust is enforced per-tool with a separate ADR per language.

This RFC introduces **one `Plugin` contract** behind **two transports**:

- **In-process transport** — for first-party, monorepo-bundled Phase A (Tree-sitter) parsers. Zero IPC. The trusted, hot-path default.
- **Sidecar transport** — a subprocess speaking a length-prefixed protobuf protocol, run under the unified sandbox (ADR-017). **Mandatory** for every Phase B semantic invoker and every community plugin.

A new language becomes "one plugin crate + one golden fixture." The per-language subprocess-trust ADR chain (ADR-006/011/012/013/015/016) collapses into a single sandbox policy (ADR-017). The `Language` enum, `ParseOutput`, the storage schema, and the MCP surface are all unchanged.

---

## Motivation

RFC-008 correctly identified that RFC-003's hardcoded indexers do not scale, and proposed a config-driven `LanguageDescriptor` TOML loader plus a `GenericTreeSitterIndexer`. That design solves the *dispatch* problem but leaves three structural issues unaddressed:

1. **Trust is still per-tool.** RFC-008 §7 makes every new subprocess invoker a hard merge gate on its own ADR (ADR-011 scip-java, ADR-012 scip-typescript, ADR-013 scip-kotlin, …). The sandbox is re-argued per language. This is process cost with no correctness benefit — the threat (a SCIP/LSIF tool executing the indexed repo's build scripts) is identical across languages.

2. **The hot path pays for isolation it doesn't need.** A uniform subprocess model would push every Phase A file parse through IPC. On a 50k-file repo at ~0.3 ms/round-trip that is ~15 s of pure transport overhead on a full reindex — paid even though Tree-sitter parsing executes **no untrusted code** and needs no sandbox.

3. **The TOML descriptor is a second config language to design, version, and trust.** RFC-008 §2 introduces `descriptor_version`, a `custom_module` escape hatch, and a three-tier descriptor trust table. A plugin that self-describes at runtime (a handshake) eliminates the descriptor format entirely.

The insight: **transport is orthogonal to edge determination.** Tree-sitter and LSIF/SCIP still produce every node and edge (CLAUDE.md non-negotiable #1 — algorithms first, LLM last — is untouched). *Where* that code runs is a trust-and-performance decision that should be made per-plugin, not globally.

---

## Detailed Design

### 1. The `Plugin` contract

A plugin is a unit that parses files of one language. It is implemented once per language and is transport-agnostic — the same `impl` is callable in-process or driven over the wire.

```rust
// travsr-plugin-sdk/src/lib.rs
use travsr_core::Language;
use travsr_plugin_protocol::{ParseRequest, ParseResponse, InvokeRequest, InvokeResponse};

/// Implemented once per language. Stateless and `Send + Sync` so a single
/// instance is shared across walker threads (in-process) or owns one child
/// process (sidecar).
pub trait Plugin: Send + Sync {
    /// Language identity (maps to the `nodes.language` column, ADR-005 Rule 3).
    fn language(&self) -> Language;

    /// File extensions this plugin claims. Reported in the handshake; the
    /// daemon builds its extension→plugin dispatch map from this.
    fn extensions(&self) -> &[&str];

    /// Whether Phase B semantic indexing is available *right now* — typically
    /// "is the external SCIP/LSIF tool on PATH?". Reported in the handshake so
    /// the daemon and `travsr language list` can surface it without knowing
    /// anything about the external toolchain.
    fn supports_phase_b(&self) -> bool { false }

    /// Phase A — structural parse of a single file (Tree-sitter).
    /// MUST NOT spawn subprocesses or touch the network.
    fn parse(&self, req: &ParseRequest) -> ParseResponse;

    /// Phase B — semantic indexing of a whole repo root via an external
    /// toolchain. ALWAYS runs under the sidecar transport + sandbox (ADR-017).
    /// The default `unimplemented` is correct for Phase-A-only languages.
    fn invoke_phase_b(&self, _req: &InvokeRequest) -> InvokeResponse {
        InvokeResponse::unsupported()
    }
}
```

`ParseRequest`, `ParseResponse`, `InvokeRequest`, and `InvokeResponse` are defined once in `travsr-plugin-protocol` (§4) and are the **only** types crossing the plugin boundary. They decode to the existing `ParseOutput` — downstream merge, FFI resolution (RFC-009), and upsert are unchanged.

### 2. The `Transport` trait

```rust
// travsr-indexer/src/transport.rs

/// How the daemon reaches a plugin. The dispatcher holds one boxed transport
/// per extension and never knows or cares which variant it is.
pub trait Transport: Send + Sync {
    fn parse(&self, req: ParseRequest) -> Result<ParseOutput, IndexError>;
    fn invoke_phase_b(&self, req: InvokeRequest) -> Result<ParseOutput, IndexError>;
    fn health(&self) -> PluginHealth;
}

/// Zero-IPC. Wraps a `Box<dyn Plugin>` and calls it directly in the daemon's
/// address space. PERMITTED ONLY for first-party, monorepo-bundled Phase A
/// grammars (ADR-017 §in-process-restriction). NEVER for Phase B. NEVER for
/// `--command` community plugins.
pub struct InProcess { plugin: Box<dyn Plugin> }

/// Subprocess speaking the framed protobuf protocol (§4), spawned under the
/// unified sandbox (ADR-017). The mandatory transport for Phase B and for all
/// community plugins.
pub struct Sidecar { child: SandboxedChild, codec: FrameCodec }
```

**The dispatcher** replaces RFC-003 §3's enum-match with a map built from handshakes:

```rust
pub struct Indexer {
    pub corpus: String,
    by_ext: HashMap<String, Arc<dyn Transport>>,   // "ts" → …, "kt" → …
}

impl Indexer {
    pub fn parse_file_with_vname(
        &self, abs_path: &Path, vname_path: &str, package: &str,
    ) -> Result<ParseOutput, IndexError> {
        let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match self.by_ext.get(ext) {
            Some(t) => t.parse(ParseRequest::new(abs_path, vname_path, &self.corpus, package)),
            None    => Ok(ParseOutput::default()),   // unrecognised extension: skip
        }
    }
}
```

The `Language` enum **stays** in `travsr-core` (it still names the `nodes.language` column and remains `#[non_exhaustive]` per RFC-003 §1). What changes is that *dispatch no longer matches on it* — extension→transport routing is data-driven from handshakes, so adding a language no longer edits a `match`.

### 3. Per-plugin transport selection

The daemon assigns a transport when it registers a plugin:

| Plugin class | Phase A transport | Phase B transport |
|---|---|---|
| First-party built-in (TS, Rust, Python, Go, Java, Kotlin) | **In-process** | Sidecar + sandbox |
| Community plugin (`--command <binary>`) | **Sidecar + sandbox** | Sidecar + sandbox |

This is the load-bearing decision of the RFC. It places the IPC cost and the sandbox boundary **exactly** where untrusted code executes (Phase B always; community Phase A) and **nowhere else** (first-party Phase A — pure, fuzzed, fixture-gated parsing runs free in-process).

**Invariant (normative): Phase B ⇒ sidecar ⇒ sandbox.** There is no in-process Phase B, ever, for any plugin including first-party. Phase B drives a compiler/build-tool that executes the indexed repo's code; the sandbox is non-negotiable (ADR-017, CLAUDE.md non-negotiable #3 local-first / no untrusted egress).

### 4. The plugin protocol — `travsr-plugin-protocol` (new crate)

A new crate at the lowest tier — depends on `travsr-core` **only** — hosting the protobuf schema and the framing codec. It is the single stable contract between the daemon and any plugin.

```
travsr-core
  ├── travsr-plugin-protocol   ← NEW: protobuf schema + frame codec (depends: travsr-core)
  ├── travsr-plugin-sdk        ← NEW: Plugin trait + run_plugin() loop (depends: travsr-plugin-protocol)
  ├── travsr-indexer           ← Transport trait, dispatcher (depends: travsr-plugin-protocol)
  ├── travsr-ingest            ← Phase B invokers + bridges (depends: travsr-plugin-protocol)
  └── travsr-store → travsr-retrieval → travsr-mcp → travsr-daemon → travsr-cli
```

```protobuf
// travsr-plugin-protocol/proto/plugin.proto
syntax = "proto3";

message Request {
  oneof payload {
    HandshakeRequest handshake = 1;
    ParseRequest     parse     = 2;
    InvokeRequest    invoke    = 3;   // Phase B
  }
}
message Response {
  oneof payload {
    HandshakeResponse handshake = 1;
    ParseResponse     parse     = 2;
    InvokeResponse    invoke    = 3;
    ErrorResponse     error     = 4;
  }
}

message HandshakeRequest  { uint32 daemon_protocol_version = 1; }
message HandshakeResponse {
  uint32          protocol_version = 1;   // monotonic; daemon fail-fasts on mismatch
  string          plugin_version   = 2;   // semver; part of the cache key (§6)
  string          language         = 3;
  repeated string extensions       = 4;
  bool            supports_phase_b = 5;
}

message ParseRequest {
  string path    = 1;            // plugin reads/mmaps the file itself (§5)
  string corpus  = 2;
  string package = 3;
  optional bytes source = 4;     // ONLY for the git-blob case (content not on disk)
}
message ParseResponse {
  repeated Node      nodes       = 1;
  repeated Edge      edges       = 2;
  repeated FfiMarker ffi_markers = 3;   // consumed by RFC-009 bridge resolution
}

message InvokeRequest  { string root = 1; }
message InvokeResponse { repeated Node nodes = 1; repeated Edge edges = 2; }
message ErrorResponse  { string file = 1; string message = 2; }   // file-scoped, non-fatal
```

**Wire framing:** 4-byte big-endian length prefix + protobuf payload, over the child's stdin/stdout — the same framing discipline as the MCP stdio server (RFC-004), deliberately **not** MCP and **not** a listening socket. See §9 (this does not breach the MCP-only non-negotiable).

**Versioning (fail-fast, not silent-degrade):** `HandshakeResponse.protocol_version` is a monotonic integer. If the daemon does not support it, the plugin is **refused at registration** with a clear error — never driven with a mismatched contract. This structurally eliminates the "silent wrong output on version skew" failure mode.

### 5. Path-passing, not content-copy

`ParseRequest` carries a **path**, not the source bytes. The source is already on disk; the plugin `mmap`s it. This shrinks a request from ~file-size (often 10–100 KB) to ~200 bytes and removes the dominant IPC cost for the sidecar transport.

The inline `source` bytes field is populated **only** when the daemon is indexing content that is *not* the working-tree file — i.e. a git blob from a prior revision during historical indexing. In the common (working-tree) case it is empty and the plugin reads from `path`.

### 6. Content-addressed parse cache

The daemon already computes a SHA-256 per file for SHA-delta freshness (RFC-003 / Sprint 2). Reuse it as a parse-cache key:

```
cache[(plugin_version, sha256(file))] → ParseOutput
```

- **Performance:** unchanged files never reach a plugin — including across a fresh `init` of a second checkout sharing content. This is what makes the sidecar IPC cost a non-issue in practice.
- **Correctness (normative):** the key is **daemon-computed** — the plugin never supplies a cache key (anti-poisoning, ADR-017 T12). `plugin_version` is a **hard** invalidation component: any change to a plugin's parsing logic MUST bump `plugin_version`, and a bump MUST produce a cache miss. A plugin-logic change that does not bump the version, and therefore serves a stale parse, is a freshness bug (CLAUDE.md non-negotiable #2 — staleness is a bug).

### 7. Single multiplexed binary

All first-party plugins ship as **one** executable, multiplexed by argv:

```rust
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("__plugin") => travsr_plugin_sdk::run_builtin(/* lang = argv[2] */),
        _                => travsr_cli::run(),
    }
}
```

`__plugin` is a hidden subcommand (like the existing `hook-run`). This yields:

- **Distribution:** one binary to build, sign, and ship. CI goes from 3 targets × N plugin binaries back to **3 builds total** (DevOps constraint — ARM64 `aarch64-unknown-linux-gnu` for OCI plus the npm targets).
- **Memory:** every sidecar plugin process is the *same* executable, so the OS shares read-only text pages copy-on-write — N plugin processes cost ≈ one process's code residency.
- **Version coherence:** a first-party plugin can never protocol-skew against its daemon — they are the same release by construction.

Community plugins remain separate binaries reached via `--command`.

### 8. Lazy spawn

A plugin is spawned the first time a file of its language is encountered, not at `init`. A pure-TypeScript repo never spawns the Kotlin/Go/Java hosts. Cold-start and resident memory scale with languages **present**, not languages **available**.

### 9. Why this does not breach MCP-only (non-negotiable #4)

CLAUDE.md #4 governs the **external** interface: clients (IDEs, agents) reach Travsr **only** over MCP. The plugin protocol is an **internal** daemon↔child channel over stdin/stdout. It is never client-reachable, exposes no listening socket, and carries no client data. Principal Architect and Solution Architect affirm: internal IPC ≠ external interface. MCP-only is preserved.

### 10. Crash domains (stated explicitly — this is a deliberate trade)

- **Sidecar transport → fault-isolated.** A plugin (community or Phase B) that panics or segfaults on a crafted file does **not** take down the daemon. The supervisor catches the dead pipe / non-zero exit, marks that file failed, logs at `tracing::warn!`, and continues. A repeatedly-panicking plugin is dropped from the registry for the daemon's lifetime (same policy as RFC-009 §8 for bridges). This is a reliability **gain** the monolith never had.

- **In-process transport → shared crash domain.** A first-party Tree-sitter grammar (C via FFI) that hits a memory bug on malicious input crashes the daemon. This is the price of zero-IPC parsing. It is accepted **only** because in-process is restricted to first-party grammars that are version-pinned, `cargo-deny`-gated, **fuzzed** (we maintain `fuzz/`), and golden-fixture-gated (RFC-003 §6). The compensating controls are mandated by ADR-017 §in-process-restriction. Any grammar that cannot meet that bar runs sidecar instead.

### 11. Backward compatibility

| Existing surface | Status |
|---|---|
| `Language` enum (`#[non_exhaustive]`) | Unchanged; still names `nodes.language`; only dispatch stops matching on it |
| `ParseOutput`, `Node`, `Edge`, `EdgeKind`, `VName` | Unchanged |
| SQLite / Kùzu schema (v4 / v6 migrations) | Unchanged — no migration |
| MCP tool API | Unchanged |
| Golden fixtures (RFC-003 §6) | Unchanged — they are the per-language acceptance gate and the transport-equivalence oracle (§exit-criteria) |
| `LanguageIndexer` trait + enum dispatcher (RFC-003 §2/§3) | **Removed** — replaced by `Plugin` + `Transport` |
| `LanguageDescriptor` TOML loader + `GenericTreeSitterIndexer` (RFC-008 §2) | **Removed** — the handshake replaces runtime self-description; the TOML descriptor format is not built |
| `lsif.rs` / `scip.rs` / `ffi_resolver.rs` location | Per RFC-008/ADR-010, in `travsr-ingest`; now reached via the Phase B sidecar path |

---

## Phased rollout (re-baselines RFC-008 §7)

| Sprint | Deliverable | Gate |
|--------|-------------|------|
| S12 | Create `travsr-plugin-protocol` + `travsr-plugin-sdk`; `Transport` trait + dispatcher behind the existing path; supervisor + sandbox spawn helper. Zero functional change — fixtures green. | **ADR-017 MUST be merged in the same sprint** (sandbox + trust). Tech Lead reviews crate-dependency tiers. |
| S13 | Migrate TypeScript, Rust, Python to in-process plugins. Delete `LanguageIndexer` + enum dispatcher. Land the `(plugin_version, sha256)` cache. | Transport-equivalence property test green (below). |
| S14 | Phase B over the sidecar transport: first SCIP language (Java via `scip-java`) and rust-analyzer LSIF relocated to sidecar invokers. | Sandbox policy (ADR-017) signed off by Security; **fail-closed** verified. |
| S15 | Cross-language bridges (RFC-009) consume merged plugin output; add `jni`. | RFC-009 bridge panic-isolation (now folded into ADR-017). |
| S16 | Phase 4 exit: Java + Kotlin green as plugins. | Per-language golden fixtures + transitive cross-language fixture (RFC-009 §9). |

Community plugins (`--command`, published SDK, registry) are **deferred to Phase 5** per CTO decision (2026-05-30). Phase 4 ships built-in plugins only; the sandbox earns its battle-testing before the external surface opens.

---

## Exit criteria

- [ ] **Transport-equivalence property test** green: for every built-in fixture, `InProcess::parse(f)` and `Sidecar::parse(f)` produce byte-identical `ParseOutput` after VName-hash normalization. (Enabling Phase B must never change Phase A output.)
- [ ] All RFC-003 §6 golden fixtures pass unchanged under the plugin path.
- [ ] Cache-invalidation test: same `sha256` + bumped `plugin_version` ⇒ miss; same + same ⇒ hit; stale entry never served after a bump.
- [ ] Crash-isolation test: a malformed file segfaulting a **sidecar** grammar leaves the daemon up and the rest of the repo indexed; referential-integrity check finds no dangling edges.
- [ ] Security exit criteria from ADR-017 (network-egress blocked, sandbox-missing disables, untrusted `--command` refused without opt-in) all green.
- [ ] Full-reindex wall-time does not regress > 10% vs the pre-refactor path (DevOps bench gate); path-passing + cache should improve it.

---

## Alternatives Considered

### A. RFC-008 as written (config-driven descriptors + per-tool trust ADRs)
Rejected as the plan of record. Solves dispatch, not trust; keeps N subprocess-trust ADRs as hard sprint gates; introduces a second config language (TOML descriptors) to version and trust. The handshake makes the descriptor format unnecessary, and ADR-017 makes the per-tool ADR chain unnecessary.

### B. Uniform sidecar (every plugin, every phase, subprocess)
Rejected. Pays the IPC tax (~15 s / 50k-file reindex) on the Phase A hot path, where no untrusted code runs and no sandbox is warranted. The two-transport split removes that cost while keeping the isolation where it matters. The only thing uniform-sidecar buys over two-transport is in-process crash isolation for first-party grammars — bought instead with fuzzing + fixtures (§10, ADR-017).

### C. Uniform in-process (compile every language in; no subprocess)
Rejected. Phase B invokers execute the indexed repo's build scripts/proc-macros — running them in-daemon is an unacceptable trust violation (CLAUDE.md #3). Community plugins would be impossible. This is the status quo RFC-003 already outgrew.

### D. JSON-RPC over the plugin channel (reuse MCP machinery)
Rejected. Per-file payloads make JSON encoding measurable; protobuf + path-passing is leaner. Reusing the *framing* discipline (length-prefix over stdio) is good; reusing the *encoding* is not.

### E. Dynamic plugin loading (`dlopen` .so/.dll) instead of subprocess sidecars
Rejected for Phase 4 (consistent with RFC-009 §6). A `dlopen`'d plugin shares the daemon's address space and can read the entire graph — the trust model (signing, capability restriction) is a Phase 5+ effort requiring Security sign-off. Subprocess sidecars get isolation for free via the process boundary.

---

## Drawbacks

- **Front-loaded refactor.** The protocol, SDK, transport, and supervisor land in S12–S13 before any new language ships. Net scope is *lower* for built-ins (the handshake deletes the TOML loader) but the up-front cost delays the first new language. PM owns the re-baseline; mitigation is to migrate built-ins one at a time keeping fixtures green.
- **Two crash domains to reason about.** In-process (first-party) and sidecar (untrusted) fail differently. §10 documents both; QA tests both.
- **Tarball size.** Bundling ~6 grammars in the multiplexed binary approaches the 25 MB tarball gate. Mitigation: feature-flag grammars for slim builds (DevOps).
- **Binary surface area.** One executable now contains the CLI, the daemon, and every built-in plugin. The `__plugin` subcommand must be hidden and must not widen the user-facing CLI.

---

## Unresolved Questions

1. **Per-directory parse batching.** §5 sends one `ParseRequest` per file. A `ParseBatchRequest` (one request per directory) would further cut sidecar round-trips ~10×. Deferred — path-passing + the cache already make the per-file cost negligible; revisit only if a bench shows IPC dominating.
2. **In-process eligibility for non-grammar first-party logic.** §3 restricts in-process to Tree-sitter Phase A. If a future first-party Phase A parser needs more than Tree-sitter (e.g. TypeScript's JSX interleaving — RFC-008's old `custom_module` case), does it stay in-process? Direction: yes, provided it executes no untrusted code and meets the §10 fuzzing bar. Finalized in the S13 implementation PR.
3. **Phase 5 community trust.** The `--command` sandbox (ADR-017) covers execution, but plugin *authenticity* (signing, a registry trust root) is Phase 5 scope. Tracked alongside the deferred SDK-publishing work.

---

## References

- RFC-003 — Multi-Language Indexer Architecture (superseded §2/§3)
- RFC-008 — Multi-Language Extension Architecture (superseded §2; this RFC redirects its rollout)
- RFC-009 — Cross-Language Bridge Plugin System (consumes merged plugin output unchanged)
- RFC-005 — Cross-Language Edge Resolution Protocol
- ADR-005 — Per-Language Corpus Naming
- ADR-009 — SCIP vs LSIF Wire Format
- ADR-010 — `travsr-ingest` Crate Boundary
- **ADR-017 — Unified Plugin Sandbox & Trust Model (companion; merges same sprint)**
- ARCH-102 — Kythe Corpus Naming Convention
- CLAUDE.md — Non-Negotiable Principles (#1 algorithms-first, #2 always-fresh, #3 local-first, #4 MCP-only, #7 no-unsafe)
