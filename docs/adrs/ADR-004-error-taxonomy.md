# ADR-004 — Error Taxonomy

| Field | Value |
|---|---|
| Status | Accepted |
| Date | 2026-05-20 |
| Issue | #70 (ARCH-104) |
| Author | Tech Lead |

---

## Context

Every public Rust function in Travsr returned `anyhow::Result<T>` — an
untyped, heap-allocated error wrapper that is excellent for rapid prototyping
but prevents callers from:

1. **Pattern matching** on error variants (e.g. distinguishing `StoreError::Io`
   from `StoreError::Database`)
2. **Generating typed MCP error codes** without string introspection
3. **Writing exhaustive tests** against specific failure modes
4. **Giving IDEs and documentation** a machine-readable error contract

A monolithic `anyhow::Error` surface also makes it impossible to express the
layered semantics already implicit in the code: storage errors are structurally
different from parse errors and traversal errors.

---

## Decision

Introduce a dedicated `travsr-error` crate that owns the entire public error
taxonomy for the workspace.

### Crate structure

```
crates/travsr-error/
  Cargo.toml   — depends only on `thiserror`
  src/lib.rs   — re-exports TravsrError + sub-error enums
```

### Error hierarchy

```
TravsrError          (top-level, used at MCP boundary)
  ├── InvalidParams(String)
  ├── Store(StoreError)
  ├── Index(IndexError)
  ├── Retrieval(RetrievalError)
  ├── BudgetExceeded { requested, limit }
  └── Internal(String)

StoreError           (travsr-store public API)
  ├── Database(String)
  ├── Migration(String)
  └── Io(std::io::Error)

IndexError           (travsr-indexer public API)
  ├── Parse { file: String, message: String }
  ├── Lsif(String)
  └── Io(std::io::Error)

RetrievalError       (travsr-retrieval public API)
  ├── Traversal(String)
  ├── PprDivergence { iterations: u32 }
  └── Store(StoreError)
```

All enums derive `thiserror::Error`, `Debug`, and `Display`. All `From<X>`
conversions between sub-errors and `TravsrError` are derived automatically by
thiserror's `#[from]` attribute.

### Migration strategy

The entire codebase previously used `anyhow::Result`. Changing every internal
helper to use typed errors in a single PR is impractical and noisy. We
therefore apply the **closure-wrapper pattern** at public API boundaries only:

```rust
// Public: returns typed error
pub fn open(path: &Path) -> Result<Self, StoreError> {
    (|| -> anyhow::Result<Self> {
        // ... all internal anyhow-idiomatic code unchanged ...
    })()
    .map_err(|e| StoreError::Database(e.to_string()))
}
```

This approach:
- Keeps internal `?` chains idiomatic and low-noise
- Exposes a typed contract at every crate boundary
- Requires zero changes to helper functions or `anyhow::Context` chains

### Crate dependency rule (addendum to CLAUDE.md §Crate Dependency Rules)

`travsr-error` has **zero dependencies on other travsr crates**. Any crate may
depend on it. The dependency graph remains acyclic.

### anyhow interoperability

`anyhow::Error` implements `From<E: std::error::Error>`, so binary crates
(daemon, CLI) that use `anyhow::Result` internally can `?`-unwrap a
`StoreError` or `IndexError` without any adapter.

### MCP error codes

`TravsrError::mcp_error_code()` returns a JSON-RPC / MCP-compatible integer
error code:

| Variant | Code |
|---|---|
| `InvalidParams` | -32602 |
| `Store(_)` | -32000 |
| `Index(_)` | -32001 |
| `Retrieval(_)` | -32002 |
| `BudgetExceeded` | -32003 |
| `Internal(_)` | -32603 |

---

## Consequences

### Positive
- Public APIs are now machine-readable and testable.
- `travsr-mcp` can map `TravsrError` to JSON-RPC error objects without string
  parsing.
- Future contributors see a clear, layered error model in the type system.

### Negative / trade-offs
- Each new error condition must be added to the relevant enum — slightly more
  ceremony than `anyhow::bail!`.
- The closure-wrapper pattern adds one extra stack frame and one string
  allocation per error path. This is negligible in context (errors are not
  hot-path).

### Not changed by this ADR
- Internal helper functions within crates may continue to use `anyhow::Result`.
- `typescript::parse` and all tree-sitter helpers remain `anyhow::Result`
  internally; only the public `Indexer` API surface returns `IndexError`.
