# RFC-028: Runtime User-Defined Language Plugins (Phase A)

**Status:** Draft
**Author:** Abhishek
**Date:** 2026-08-24
**Phase:** Post-v1.0 (unscheduled)
**Crate(s) affected:** `travsr-core`, `travsr-analysis`, `travsr-plugin-host`, `travsr-indexer`, `travsr-store`, `travsr-mcp`, `travsr-cli`
**Related:** RFC-008 (multi-language extension architecture), RFC-009 (cross-language bridge plugins, section 6 rejects dynamic native loading), RFC-011 (two-transport plugin architecture), RFC-017 (travsr-analysis crate), ADR-017 (unified plugin sandbox and trust model)
**Fills:** the never-written "RFC-013" placeholder for runtime language pluggability
**Tracking issue:** #796

---

## Summary

Let a user add a **Phase A** (tree-sitter, structural) language at runtime, from
a config file plus a grammar artifact, with a visible catalog entry, and
**without recompiling the daemon**. Two transports are offered behind one shared
config and dispatch path:

- **WASM (default, recommended):** a `.wasm` grammar run in tree-sitter's
  wasmtime runtime. Isolation is intrinsic to the runtime, so an artifact the
  user downloaded can be run without an OS sandbox and without `unsafe`.
- **Native (opt-in, "I trust this grammar"):** a `.dylib` / `.so` / `.dll` loaded
  via `dlopen`. No isolation: a grammar bug crashes the whole daemon, and it is
  `unsafe` FFI. Gated behind an explicit acknowledgement and Security sign-off.

Phase B (cross-file semantic resolution) is explicitly out of scope. This RFC is
grounded in two runnable spikes (native and WASM); every feasibility claim below
is backed by measured behavior, quoted inline.

---

## Motivation

Travsr ships 16 languages, all compiled in. Adding a 17th today requires editing
the `Language` enum, adding an `analysis/src/<lang>.rs`, wiring `registry.rs`,
adding a grammar crate, regenerating `plugin-hashes.lock`, and rebuilding. An end
user who installed the prebuilt npm binary has no path at all: `scripts/install.js`
downloads a sealed binary and never compiles.

The pain is concrete: a user whose repo contains an unsupported language (Zig,
Nix, OCaml, and so on) gets zero structural intelligence for those files, and no
amount of local configuration changes that. The request "let me point Travsr at a
grammar for my language" is reasonable and currently unanswerable for the shipped
binary.

This RFC adds a runtime path: `travsr lang add ./mylang.lang.toml` registers a
grammar the daemon was never compiled with, and `travsr lang catalog` shows it.

---

## Detailed Design

### 1. What the spikes proved (grounded findings)

Two standalone loaders were built, each depending only on `tree-sitter` plus a
load mechanism, so neither knew anything about the target language at compile
time. They exercise the isolated mechanism that this RFC then wires into the
daemon.

#### 1.1 Native (dlopen), grammar compiled with the system `cc`

| Observation | Result |
|---|---|
| dlopen plus `Language::from(LanguageFn::from_raw(sym))` plus parse plus runtime query | Works. Extracted `greet`, `Account.deposit`, and a `require("json")` import via a `#eq?` predicate. The whole `LanguageConfig` surface runs from runtime strings. |
| ABI gate | `abi_version()` is observable before use. abi 15 was accepted (host window `13..=15`); a forced abi 11 made `set_language` return `Err("Incompatible language version 11. Expected minimum 13, maximum 15")`. |
| ABI trap | The error is recoverable, but ignoring it let `parse()` return `None`, which then panicked. A naive log-and-continue crashes. |
| External scanner | Lua ships `scanner.c`; the dylib had to export `tree_sitter_lua_external_scanner_*`. "Bring a grammar" means two C files, not one. |
| Negative paths | Wrong symbol, malformed query, unknown node kind, and a non-dylib file all returned clean recoverable `Result`s, with `QueryError` carrying `line:col`. |
| Lifetime | The grammar function pointer is valid only while the `Library` stays mapped. |
| Toolchain | The system `cc`, present on every dev machine, compiled the grammar. The artifact is per OS and per arch. |

#### 1.2 WASM (wasmtime via the tree-sitter `wasm` feature)

| Observation | Result |
|---|---|
| The `tree-sitter` `wasm` feature builds in-workspace | Yes. It pulls `wasmtime-c-api` (cranelift), about 36s cold. |
| Runtime path with a scanner-less JSON grammar | Works end to end. `abi_version()=14`, `set_language OK`, JSON parsed, and a runtime query pulled every key. |
| Popular prebuilt `tree-sitter-wasms` Lua artifact | Rejected: `failed to parse dylink section`. The section is the legacy emscripten `dylink`, not the standardized `dylink.0`. |
| After converting `dylink` to `dylink.0` | It advanced to instantiation, then failed with `invalid import 'tree_sitter_lua_external_scanner_create'`. Emscripten builds the scanner as a dynamic import; tree-sitter expects it statically linked. Section surgery is insufficient because the whole build model differs. |
| Producing a correct `.wasm` locally | Blocked: `tree-sitter build --wasm` requires emscripten or Docker, neither of which was present. |
| A correctly built third-party set (`tree-sitter-wasm`, 112 grammars, `dylink.0`, cosign-signed) | Loads. zig (no scanner) and ruby (with a static scanner) both loaded and parsed in our tree-sitter 0.26 runtime at abi 15. |
| Negative paths | Wrong name gave `module did not contain language function: tree_sitter_notlua`; a truncated module gave `failed to parse WebAssembly module`. Clean. |

#### 1.3 The `Language` enum blast radius (measured)

Adding a throwaway `Language` variant compiled the entire 15-crate workspace after
fixing exactly one match arm (`as_str`). The helper methods (`from_extension` and
`from_str` both end in `_ => None`) and the dispatch sites (`_ => ParseOutput::default()`)
absorb the rest.

The important consequence is the second half: the compiler will not flag missing
runtime wiring. An unknown language silently no-ops instead of erroring. This RFC
cannot use the type checker as a checklist; it needs an explicit conformance list
(section 6).

### 2. Two transports, one config

The grammar artifact loads through one of two transports. Everything above the
transport is shared: the TOML schema, the query engine, the dispatcher
registration, and the catalog.

- **WASM (default, recommended).** A `.wasm` grammar run in wasmtime. Isolation is
  intrinsic to the runtime, so no OS sandbox is needed and no `unsafe` is used.
  One artifact runs on every host arch, and a crash is a catchable trap.
- **Native (opt-in, "I trust this grammar").** A `.dylib` / `.so` / `.dll` via
  `dlopen`. No isolation: a grammar bug segfaults the daemon, and `unsafe` FFI
  requires an ADR sanction (project rule: no unsafe without RFC plus Tech Lead
  plus Security sign-off). Gated behind an explicit `--trust-native`
  acknowledgement.

The rationale is grounded, not asserted. Section 1.1 versus 1.2 shows native
carries four taxes (the ABI gate, the scanner-symbol surface, the keep-alive and
`unsafe` lifetime coupling, and per-platform artifacts) that WASM collapses, while
the core mechanism is identical.

### 3. Grammar-artifact contract

A registrable grammar is self-contained: parser plus external scanner (if any)
compiled into one artifact, exporting `tree_sitter_<name>`.

- WASM: built by `tree-sitter build --wasm` with a tree-sitter CLI of version 0.25
  or newer, which produces `dylink.0` with a statically linked scanner. Legacy
  emscripten `dylink` artifacts (the older `tree-sitter-wasms` package) are not
  accepted (section 1.2).
- Native: `cc -shared -fPIC parser.c scanner.c -o lib<name>.<ext>`.

### 4. The `<lang>.lang.toml` schema

```toml
[language]
name = "lua"                 # the tree_sitter_<name> export and the catalog id
extensions = ["lua"]
transport = "wasm"           # "wasm" | "native"
artifact = "./lua.wasm"      # path to a .wasm or a .dylib/.so/.dll

[parse]
queries = """
(function_declaration name: (identifier) @fn.name)
(function_call name: (variable) @_req
   arguments: (arguments (string) @import) (#eq? @_req "require"))
"""
capture_kinds     = [["fn.name","function","fn"], ["import","import","import"]]
method_containers = [["function","function"]]
```

The fields mirror the compiled `LanguageConfig` (`extensions`, `queries`,
`capture_kinds`, `method_containers`, `decl_kinds`, `type_refinements`). Authoring
the queries requires the grammar's `node-types.json` (section 1.1).

### 5. Validating registration (never trust and run)

At `lang add` and at daemon startup, each catalog entry transitions through:

```
discovered
  -> artifact_ok?   (file exists and loads: dlopen or WasmStore::load_language)   else error
  -> symbol_ok?     (tree_sitter_<name> present)                                  else error
  -> abi_ok?        (MIN_COMPATIBLE_LANGUAGE_VERSION..=LANGUAGE_VERSION)          else incompatible
  -> query_ok?      (Query::new compiles against the loaded grammar)              else error
  -> registered     (added to Dispatcher.by_ext at runtime)
```

Every failure is a recoverable catalog state with a reason string, never a panic.
This directly fixes the two spike traps: the ABI error that panicked downstream
(section 1.1) and the shipped `GenericTreeSitterPlugin::new` doing
`Query::new(...).unwrap_or_else(|| panic!)`. A user TOML typo must not crash the
daemon.

### 6. Runtime dispatch and the `Language::Custom` conformance list

Registration goes into the existing `Dispatcher.by_ext` chokepoint at runtime,
the same table `register_builtins` fills. A `Language::Custom(CustomLangId)`
variant carries the name string for the one exhaustive match (`as_str`, section
1.3).

Because the catch-all `_ =>` sites silently skip (section 1.3), the compiler will
not enforce completeness. This RFC therefore ships a conformance list of every
site a custom language must reach. From the code walked during the spike:

1. `travsr-plugin-host` `Dispatcher::parse_file` and `register_*`: the primary
   Phase A dispatch, keyed by extension. This is the clean insertion point.
2. `travsr-indexer` `Indexer::parse_file_with_vname`: matches on
   `Language::from_extension` and falls through `_ => ParseOutput::default()`.
   A custom language hits that arm and is silently skipped unless handled.
3. `travsr-core` `Language::as_str` (the one forced arm), and the runtime
   extension-to-language map that replaces the compile-time `from_extension`.
4. `travsr-store`: the `nodes.language` column takes the custom name as a string
   (no schema change expected, to be confirmed).
5. `travsr-mcp` and `travsr-cli`: `get_lang_status`, `repo_languages`,
   `travsr lang list`, and status output enumerate known languages; custom ones
   must appear.

### 7. Config production and lifecycle

The user TOML is parsed into a runtime-owned `LanguageConfig`. Today
`GenericTreeSitterPlugin` takes a `&'static LanguageConfig` because the shipped
configs are `const`; the plugin type and `register_generic` must accept a
runtime-owned (heap) config. The generic parser `parse_with_config` is already
data-driven, so no per-language Rust is needed.

Lifecycle:

- Native: keep every `dlopen` `Library` alive for the process (or refcount against
  in-flight parses). `lang remove` marks the entry unregistered but cannot
  `dlclose` mid-parse (section 1.1).
- WASM: the `WasmStore` owns the module; drop is safe once no parser holds it.
  Each parser running a WASM language needs `set_wasm_store`, so the indexer needs
  a shared `wasmtime::Engine` and careful store handling in the parse loop. This
  plumbing is unique to WASM (`WasmStore` is `Send + Sync`).

### 8. Artifact distribution and guardrails (no catalog service)

We deliberately do not build a catalog or metadata service. The spike failures
were all format and version mismatches, and we control the format when we build
the artifact. Three pieces suffice:

**8.1 Prebuilt `.wasm` assets, CI-built, version-pinned.** CI runs
`tree-sitter build --wasm` for a curated set of extra grammars against the same
tree-sitter version the binary ships, which guarantees `dylink.0`, a static
scanner, and an in-range ABI (the three section 1.2 failure modes, eliminated at
the source). Assets are cosign-signed like the binaries and stored in the OCI
`travsr-releases` bucket. The set is coupled to the release. Retrieval is either a
docs download link, or a deterministic fetch: `travsr lang add <name>` resolves to
`releases/<own-version>/grammars/<name>.wasm`, a plain download derived from the
running version, not a listing API. Any network fetch is explicit, in keeping with
local-first.

The build cost is low. The `.wasm` compilation is fast and fully automatable;
artifacts are tens to hundreds of KB. The real recurring cost is per-language
query authoring, which is the same work as shipping a language natively, minus the
Rust wiring and recompile.

**8.2 `lang add` accepts any `.wasm` behind a validating guardrail** that
translates raw wasmtime errors into actions. Every row below is an error observed
in the spike:

| Raw error (observed) | Guardrail message |
|---|---|
| `failed to parse dylink section` | Built with an old or emscripten toolchain (legacy `dylink`); rebuild with tree-sitter 0.25 or newer, or use a Travsr prebuilt. |
| `invalid import '...external_scanner_create'` | The scanner is dynamically linked; rebuild it statically (`tree-sitter build --wasm` does this). |
| `Incompatible language version N` | The grammar ABI is out of range for this Travsr build; rebuild with a newer tree-sitter. |
| `module did not contain language function: tree_sitter_<n>` | The `name` in the TOML must match the grammar export. |
| `unexpected end-of-file` | Not a valid or complete `.wasm`. |

A `travsr lang check ./foo.wasm` runs this validation as a dry run, with no
registration, so users test before committing.

**8.3 Build-your-own is docs-first.** Document the TOML properties and the honest
toolchain requirement: `tree-sitter build --wasm` needs emscripten or Docker
(section 1.2), and no wrapper can remove that. An optional `travsr lang build` may
improve errors but must not imply it eliminates the dependency.

### 9. Security posture

This feature adds a new threat surface: executing a user-supplied or third-party
grammar artifact inside the daemon process, which holds the entire indexed graph
(function names, call relationships, package layout). Proposed threat-model row:

| # | Asset | Threat | Likelihood | Impact | Mitigation |
|---|---|---|---|---|---|
| T11 | Local graph DB and daemon process | A user-supplied grammar artifact executes hostile or buggy code in the daemon | Med (native), Low (WASM) | High | WASM runs sandboxed in wasmtime with no host access, which is the default. Native is `dlopen` in-process with full graph access and is gated behind explicit `--trust-native` plus Security sign-off. Our own prebuilt assets are cosign-signed; `lang add` may verify the signature. Third-party artifacts are acceptable only under the WASM sandbox. |

Per-transport verdict, using the Principal Security Engineer framing:

- WASM: APPROVED. The wasmtime boundary satisfies the "sandbox anything that
  executes user code" principle. Accepting a downloaded `.wasm` is defensible
  precisely because it cannot reach the host.
- Native: APPROVED_WITH_MITIGATIONS for source builders only, REJECTED as an
  end-user path on the sealed npm binary. In-process `dlopen` of an artifact the
  daemon did not build is the "run it anyway, the user trusts their own toolchain"
  fallback that ADR-017 and the sandbox policy explicitly name as the
  vulnerability. It requires the `unsafe` sanction and an explicit opt-in, and it
  should not be the default anywhere.

Supply chain: prebuilt assets follow the existing release-signing path (sigstore
or cosign). The third-party `tree-sitter-wasm` package is a viable known-good
source, but we do not control its tree-sitter version, so the ABI gate (section 5)
is the guard against drift, and it stays "known-good per Travsr version."

---

## Alternatives Considered

**Compile-time scaffolding only (the earlier "Design A").** A command that
generates the wiring from a TOML, then the user rebuilds. Rejected as the primary
path because it cannot serve the sealed npm binary at all: npm ships a prebuilt
binary and never compiles, so a compiled-in grammar is unreachable without
becoming a source builder. It remains a valid contributor workflow but does not
solve the stated problem.

**Dynamic native loading as the default.** Rejected for the same reason RFC-009
section 6 and RFC-011 section E rejected it: an in-process native plugin can read
and exfiltrate the entire graph, and the trust model (signing, capability
restriction) is non-trivial. WASM provides isolation for free, so native is an
opt-in, not a default.

**A catalog or metadata service.** Rejected as unnecessary. The failures we
measured are format and version mismatches, which are solved by building correct
artifacts and by a validating `lang add`, not by a listing service. A deterministic
per-version download URL covers the convenience case without a service.

**Relying solely on the third-party `tree-sitter-wasm` package.** Rejected as the
only source because we do not control its tree-sitter version or its language set,
and it ships raw `.wasm` plus highlight `.scm`, not our `capture_kinds`. It is
documented as a known-good fallback, while our own signed, version-pinned assets
are the hardening step.

---

## Drawbacks

- The wiring is broad. It touches core, store, indexer, plugin-host, mcp, and cli,
  mostly for the `Language::Custom` identity, and the compiler will not guard it
  (section 1.3, section 6).
- WASM shifts the burden to artifact production: the user needs a correctly built
  `.wasm`, and `tree-sitter build --wasm` needs emscripten or Docker. The
  "no recompile" promise applies to the daemon, not to the grammar.
- ABI coupling is permanent. Assets are per-release, and an old asset against a
  newer binary can fall out of the ABI window by design. The guardrail makes this
  a clear error rather than a silent failure, but it is real.
- Native adds an `unsafe` surface and a crash domain: a grammar bug takes down the
  daemon serving all repos.
- The WASM feature pulls wasmtime and cranelift into the binary, which increases
  build time and binary size. The size and startup impact on the shipped binary
  needs measurement before this is accepted.

---

## Unresolved Questions

1. Artifact-production UX: do we document `tree-sitter build --wasm` (with its
   emscripten or Docker dependency), or ship a `travsr lang build` helper that
   wraps it, knowing the wrapper cannot remove the dependency?
2. Is native offered at all on the sealed npm binary, given the `unsafe` and ADR
   gate, or is WASM the only end-user path with native restricted to source
   builders?
3. Binary-size and cold-start budget: what is the measured cost of compiling in
   the WASM feature (wasmtime plus cranelift), and is it acceptable for the default
   shipped binary, or does it belong behind a build feature or a separate binary?
4. Signature policy: do we require cosign verification for our own assets, and do
   we verify third-party signatures (such as `tree-sitter-wasm`'s sigstore data)
   at `lang add`?
5. Phase B for custom languages is deliberately deferred. The sidecar model
   (RFC-011) already exists but is a separate effort.

---

## Appendix: spike artifacts

Reproducible under `scratchpad/design-b-spike/`:

- `loader/`: the native dlopen loader (depends only on `tree-sitter` and
  `libloading`).
- `wasmloader/`: the WASM loader (depends only on `tree-sitter` with the `wasm`
  feature).
- `grammar/liblua_grammar.dylib`: Lua grammar compiled with the system `cc`.
- The `dylink` to `dylink.0` converter and the converted JSON wasm.
- The `tree-sitter-wasm` (version 1.1.6) prebuilt set, used to confirm that a
  correctly built third-party artifact loads at abi 15.

All spike code lives outside the repo tree; the workspace carries no code changes
from the spike (the throwaway `Language` variant used to measure section 1.3 was
reverted).
