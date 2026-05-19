# ARCH-102: Kythe Corpus Naming Convention

**Status:** Accepted  
**Author:** Travsr Engineering  
**Date:** 2026-05-18  
**Crate(s) affected:** `travsr-core`, `travsr-indexer`, `travsr-daemon`  
**Principal Architect sign-off:** Required (changes VName addressing scheme — Architectural Invariant #1) ✅

---

## Summary

`VName.corpus` identifies which repository a node belongs to. It is currently free-form:
tests use `github.com/raj-rkv/travsr`, production uses absolute paths, and some databases have
empty strings. Phase 5 cross-repo `Exports` edges require two Travsr instances indexing the
same repository to agree on an identical corpus string — otherwise edge joins produce empty
results with no diagnostic. This RFC locks the corpus scheme and defines a deterministic
derivation algorithm for all standard git remote URL formats.

---

## Motivation

The VName address space is global: `NodeId = BLAKE3(corpus ∥ root ∥ path ∥ language ∥ signature)`.
Two Travsr instances that independently index `github.com/acme/payments-api` must derive the
same corpus string, or the cross-repo edge `(callerNodeId) --[Exports]--> (calleeNodeId)` will
reference an unknown NodeId on the other side. There is no error — the join silently returns
empty results, which is the worst failure mode for a code intelligence tool.

The free-form corpus problem must be solved before Phase 5 work begins.

---

## Detailed Design

### Canonical scheme

```
host/org/repo
```

All lowercase. No URL scheme prefix. No `.git` suffix. No trailing slash.

**Examples:**

```
github.com/raj-rkv/travsr        ✅ canonical
gitlab.com/acme/payments-api     ✅ canonical
bitbucket.org/foo/bar-service    ✅ canonical

https://github.com/raj-rkv/travsr   ✗ (has scheme)
github.com/raj-rkv/travsr.git       ✗ (has .git suffix)
GitHub.com/Raj-Rkv/Travsr           ✗ (not lowercase)
github.com/raj-rkv/travsr/          ✗ (trailing slash)
```

### Fallback for local-only repos

Repos with no git remote receive corpus `local/<basename>` where basename is the directory
name lowercased with non-alphanumeric characters (except `-` and `_`) replaced by `-`.
Cross-repo edges are not possible for local-only repos by definition.

### Canonical derivation algorithm (`travsr-core::canonical_corpus`)

Handles all standard git remote URL formats:

| Input | Output |
|---|---|
| `https://github.com/raj-rkv/travsr.git` | `github.com/raj-rkv/travsr` |
| `https://github.com/raj-rkv/travsr` | `github.com/raj-rkv/travsr` |
| `git@github.com:raj-rkv/travsr.git` | `github.com/raj-rkv/travsr` |
| `git@github.com:raj-rkv/travsr` | `github.com/raj-rkv/travsr` |
| `ssh://git@github.com/raj-rkv/travsr.git` | `github.com/raj-rkv/travsr` |
| `git://github.com/raj-rkv/travsr.git` | `github.com/raj-rkv/travsr` |
| `HTTPS://GITHUB.COM/Raj-Rkv/Travsr.GIT` | `github.com/raj-rkv/travsr` |
| `github.com:443/raj-rkv/travsr.git` (port) | `github.com/raj-rkv/travsr` |

```rust
pub fn canonical_corpus(remote_url: &str) -> String {
    let s = remote_url.trim();

    // SCP-style SSH: git@host:org/repo[.git]
    if let Some(rest) = s.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            return format!("{}/{}", host.to_lowercase(), normalize_path(path));
        }
    }

    // URL schemes: https://, http://, ssh://, git://
    let after_scheme = s.split_once("://").map_or(s, |(_, r)| r);
    // Strip userinfo (ssh://git@host/path)
    let after_at = after_scheme.split_once('@').map_or(after_scheme, |(_, r)| r);

    if let Some((host_port, path)) = after_at.split_once('/') {
        let host = host_port.split(':').next().unwrap_or(host_port);
        return format!("{}/{}", host.to_lowercase(), normalize_path(path));
    }

    // No path component — local fallback
    format!("local/{}", sanitize_local(s))
}
```

### Storage

The corpus is stored in the `meta` table under key `"corpus"` on every `travsr init`. The
daemon reads it back at the start of `reindex_files` so that incremental hook runs use the
same corpus as the initial index.

### Mismatch detection

Legacy databases (empty corpus or absolute-path corpus) are detected when the `meta.corpus`
key is absent or starts with `/`. The daemon logs a warning and recommends `travsr init`.
No in-place rewrite — same reasoning as RFC-002: correctness risk of an in-place migration
exceeds the cost of a clean re-index.

### Non-colliding VName invariant

The VName hash includes `corpus` as a domain component. Two repos with different canonical
corpora that happen to contain files with identical paths and signatures will produce different
`NodeId`s. This is algebraically guaranteed and verified by a regression test.

---

## Alternatives Considered

### Repository UUID

Generate a random UUID on `travsr init` and store it in `.travsr/repo-id`. Rejected: requires
out-of-band synchronisation between instances (the UUID must be shared for cross-repo joins
to work). The git remote URL is already a globally agreed-upon identifier.

### Full URL (with scheme)

Use `https://github.com/raj-rkv/travsr` as the corpus. Rejected: the scheme carries no
useful information for graph identity, adds noise to all corpus strings, and creates two
valid forms for the same repo (http vs https). The Kythe VName spec uses `host/org/repo`
without a scheme.

### Absolute path fallback

Use the repo root path (current behaviour) when no remote exists. Rejected: absolute paths
are machine-specific; a CI runner and a developer laptop produce different corpora for the
same repo, breaking future multi-machine index merges.

---

## Drawbacks

- All existing databases with empty or path-based corpora must be re-indexed after upgrade.
  The daemon warns and guides the user via `travsr init`.

- Repositories hosted at custom domains (e.g. self-hosted GitLab at `git.acme.internal`)
  require no special handling — the scheme is host-agnostic. The scheme normalises them
  correctly as long as the remote URL is a standard git URL.

---

## Unresolved Questions

1. **Monorepos with multiple remotes:** Which remote takes precedence? For now: `origin`.
   If `origin` does not exist, fall back to the first remote alphabetically. Deferred to
   a follow-on issue once monorepo support is scoped.

2. **GitHub vs GitHub Enterprise:** `github.mycompany.com/org/repo` produces a different
   corpus from `github.com/org/repo` even if they mirror the same code. This is correct
   behaviour for graph identity (different instances of the same code are different corpora),
   but should be documented for users who mirror repos.
