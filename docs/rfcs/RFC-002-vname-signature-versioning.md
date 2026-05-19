# RFC-002: VName Signature Format Versioning

**Status:** Accepted  
**Author:** Travsr Engineering  
**Date:** 2026-05-18  
**Crate(s) affected:** `travsr-core`, `travsr-store`, `travsr-daemon`  
**Principal Architect sign-off:** Required (changes VName addressing scheme — Architectural Invariant #1) ✅

---

## Summary

`NodeId = BLAKE3(VName)` is the identity primitive of the Travsr graph. Any change to the
VName signature vocabulary (e.g. adding method-level precision, renaming edge kinds, or
introducing LSIF-derived signatures) silently invalidates every `NodeId` in every existing
`.travsr/graph.db`. This RFC locks the signature format with a version byte domain-separated
into the hash input, stores the active version in the `meta` table, and defines the daemon's
response when a mismatch is detected on startup.

---

## Motivation

Today, the signature format is undeclared and unversioned:

```rust
// travsr-core — current (pre-RFC-002)
pub fn id(&self) -> NodeId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(self.corpus.as_bytes());
    // ...
}
```

If Sprint 6 renames `class:Foo` → `class:Foo@v2` for LSIF compatibility, every `NodeId`
silently changes. An existing `graph.db` built with the old format will contain stale
`NodeId`s that no longer match freshly-computed ones. Edges will silently dangle. Queries
will silently return empty results. There is no error — just wrong answers.

This is a class-1 correctness hazard that must be addressed before Phase 2 is production-ready.

---

## Detailed Design

### 1. Version table

| Version | Wire byte | Signature vocabulary | Notes |
|---|---|---|---|
| 0 | *(absent)* | Pre-RFC-002 legacy | All existing `.travsr/graph.db` files |
| 1 | `0x01` | Tree-sitter: `class:X`, `fn:X`, `method:X.Y`, `var:X`, `import:M` | **Current — introduced by this RFC** |

`0x00` is reserved and never used as a version byte (avoids ambiguity with NUL padding).  
`0xFF` is reserved for experimental/dev builds.

### 2. Hash domain separation (`travsr-core`)

The version byte is prepended as the **first input** to the BLAKE3 hasher. BLAKE3 is a PRF;
prepending a fixed byte makes outputs from different versions collision-free by construction.

```rust
/// Bump this constant whenever the VName signature vocabulary changes or the
/// hash input serialisation changes. Every increment invalidates all existing
/// NodeIds — `.travsr/graph.db` files built with a different version must be
/// fully re-indexed before they can be queried.
pub const SIGNATURE_FORMAT_VERSION: u8 = 1;

pub fn id(&self) -> NodeId {
    let mut hasher = blake3::Hasher::new();
    // Domain separator: MUST be first. Changing SIGNATURE_FORMAT_VERSION
    // produces disjoint NodeId spaces across signature format versions,
    // preventing silent identity aliasing between incompatible graphs.
    hasher.update(&[SIGNATURE_FORMAT_VERSION]);
    hasher.update(self.corpus.as_bytes());
    hasher.update(b"\0");
    hasher.update(self.root.as_bytes());
    hasher.update(b"\0");
    hasher.update(self.path.as_bytes());
    hasher.update(b"\0");
    hasher.update(self.language.as_bytes());
    hasher.update(b"\0");
    hasher.update(self.signature.as_bytes());
    // first 8 bytes of BLAKE3 → little-endian u64
    let digest = hasher.finalize();
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&digest.as_bytes()[..8]);
    NodeId(u64::from_le_bytes(buf))
}
```

### 3. Storage (`travsr-store` — migration v3)

```sql
-- v3_signature_format_version.sql
-- Records the signature format version used when this graph was last fully
-- indexed. Existing databases receive version 0 (legacy, no version byte
-- in BLAKE3 input). The daemon writes SIGNATURE_FORMAT_VERSION after every
-- successful full re-index. A mismatch between this value and the compiled
-- constant means the graph must be re-indexed before it can be trusted.
INSERT OR IGNORE INTO meta(key, value)
    VALUES('signature_format_version', '0');
```

`SqliteStore` gains two new methods:

```rust
pub fn get_signature_format_version(&self) -> Result<u8>
pub fn set_signature_format_version(&mut self, v: u8) -> Result<()>
```

### 4. Daemon mismatch policy (`travsr-daemon`)

| Entry point | Mismatch detected | Action |
|---|---|---|
| `travsr init` | Legacy or wrong version | Log info, run full re-index, write `SIGNATURE_FORMAT_VERSION` on success |
| `post-commit hook` (`reindex_files`) | Any mismatch | `tracing::warn!`, skip reindex, **return `Ok(())`** — never block a commit |
| `travsr daemon start` (future) | Any mismatch | Print actionable error, suggest `travsr init`, exit 1 |

The hook **must exit 0** on a version mismatch. Blocking `git commit` for a schema issue is
a developer-experience violation. The user will see the warning in hook output and can run
`travsr init` to resolve it.

```rust
pub enum VersionCheckResult {
    Current,
    Outdated { found: u8, expected: u8 },
}
```

---

## Alternatives Considered

### BLAKE3 keyed-hash mode (`Hasher::new_keyed`)

BLAKE3 supports a 256-bit key that produces domain-separated outputs. Rejected because it
requires a stable 32-byte key per version (key storage and derivation add complexity) while
providing identical collision-resistance properties to a simple prefix byte. A single version
byte is auditable in 10 seconds; a 32-byte key is not.

### Namespace string prefix (`"travsr-v1:"`)

Prepending a human-readable namespace string (as used in some `BLAKE3::derive_key` contexts)
adds variable-length bytes to every hash computation. A single `u8` is both faster and
equally correct for our version space of < 256 formats.

### No versioning (rely on full re-index documentation)

Rejected. Documentation is not enforced by the software. A user upgrading across a signature
format boundary with a stale `graph.db` will see silent wrong answers, not an error — the
worst possible failure mode for a code intelligence tool.

### Store version in filename (`graph-v1.db`)

Rejected. Creates migration complexity for the registry, hook paths, and every place that
references the DB path. A single `meta` row is trivially queryable and requires no path changes.

---

## Drawbacks

- **All existing `NodeId`s change on upgrade from version 0 → 1.** Every user who upgrades
  from a pre-RFC-002 build must run `travsr init` to re-index their repos. This is a
  one-time cost; subsequent format bumps will be caught automatically.

- **Version byte adds 1 byte to every hash computation.** BLAKE3 processes data in 64-byte
  blocks; a single extra byte has no measurable throughput impact.

---

## Unresolved Questions

1. **Delta migration (skip full re-index):** Could we rewrite `NodeId`s in-place by reading
   each node's VName, recomputing with the new version byte, and updating all edges? This
   would avoid a full Tree-sitter + LSIF re-parse. Deferred to a future RFC — the
   correctness risk of an in-place migration is higher than a clean re-index for Phase 2.

2. **Version incompatibility UI:** The `travsr daemon start` exit-1 path is specified but not
   implemented in this sprint. A follow-on task (issue #52 acceptance item) will add the
   interactive CLI prompt.
