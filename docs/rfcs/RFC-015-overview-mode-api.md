# RFC-015: `mode=overview` MCP API Contract

**Status:** Accepted  
**Author:** Engineering  
**Date:** 2026-06-14  
**Crates affected:** `travsr-mcp`, `travsr-store`  
**Issue:** #319 (graph webview redesign, P3)

---

## Summary

This RFC documents the `mode=overview` extension to the `get_graph_json` MCP tool, introduced in PR #327. It establishes the node schema, edge schema, `pkg:` ID namespace, `path_prefix` trust model, and the two-level LOD (level-of-detail) contract that all current and future backends must honour.

---

## Motivation

The VS Code graph webview needed a repo-map view that aggregates hundreds of file nodes into scannable package tiles without overwhelming the rendering budget. The existing `get_graph_json` tool was extended with two new optional parameters (`mode`, `path_prefix`) so the webview can request either a top-level package overview or a drill-down into a specific package.

---

## API Contract

### Parameters

| Param | Type | Valid values | Default |
|---|---|---|---|
| `mode` | string enum | `""`, `"overview"` | `""` |
| `path_prefix` | string | any safe relative path (validated by `validate_mcp_arg`) or `""` | `""` |

When `mode=""` (or omitted), `get_graph_json` behaves exactly as before this RFC — BFS from `query`.

When `mode="overview"`:
- If `path_prefix` is empty → return **repo-level tile graph** (one node per top-level package).
- If `path_prefix` is non-empty → return **package drill graph** (file nodes inside the prefix + ghost nodes for cross-boundary deps).

### Node schema (`mode=overview`, no prefix)

```json
{
  "id":         "pkg:<first-path-segment>",
  "label":      "<first-path-segment>",
  "kind":       "pkg",
  "file_count": <integer>,
  "ghost":      false
}
```

`pkg:` IDs are synthetic — they are never stored in the graph DB. The namespace is reserved.

### Node schema (`mode=overview`, with prefix — file nodes)

```json
{
  "id":    "file:<relative-path>",
  "label": "<basename>",
  "kind":  "file",
  "path":  "<relative-path>",
  "ghost": false
}
```

### Node schema (`mode=overview`, with prefix — ghost package nodes)

External dependency packages are represented as ghost nodes:

```json
{
  "id":    "pkg:<first-path-segment-of-dependency>",
  "label": "<first-path-segment>",
  "kind":  "ghost",
  "ghost": true
}
```

Ghost nodes have `kind="ghost"` (not `"pkg"`) so clients can distinguish them from real package tiles. The `ghost: true` boolean is redundant but kept for forward-compat with older clients.

### Edge schema

```json
{
  "source": "<node-id>",
  "target": "<node-id>",
  "kind":   "imports",
  "count":  <integer>   // overview level only; omitted in drill-down
}
```

### Response envelope

```json
{
  "nodes": [...],
  "edges": [...],
  "mode":  "overview" | "prefix",
  "path_prefix": "<string>"   // only present when mode=prefix
}
```

`mode` in the response tells the client which LOD level was returned:
- `"overview"` → top-level tiles
- `"prefix"` → file-level drill

---

## `path_prefix` Trust Model

`path_prefix` is an untrusted string received from the MCP caller. Before use:

1. If non-empty, `validate_mcp_arg(path_prefix)` is called — same validation as all other MCP string inputs (length limit, no control characters, no shell metacharacters).
2. `pkg_key_from_path` excludes paths starting with `..`, `/`, or containing `://` at the node-filtering level.
3. The `starts_with(path_prefix)` check in `package_drill_graph` is a data filter, not a security boundary — security is upstream in step 1.

Empty `path_prefix` is always allowed (no validation needed; routes to `repo_overview_graph`).

---

## `pkg_key_from_path` Behaviour

Maps any stored file path to its top-level directory segment (first `/`-delimited part):

| Input | Output |
|---|---|
| `"pkg/api/types.go"` | `"pkg"` |
| `"src/index.ts"` | `"src"` |
| `"main.go"` | `"(root)"` |
| `"../Library/..."` | `""` (excluded) |
| `"/abs/path"` | `""` (excluded) |
| `"scip://corpus/..."` | `""` (excluded) |

The `(root)` sentinel is used for files at the repository root with no directory prefix.

---

## Two-Level LOD Model

```
Level 0 (repo overview):  get_graph_json(mode="overview", path_prefix="")
  → N pkg nodes, cross-package import edges

Level 1 (package drill):  get_graph_json(mode="overview", path_prefix="<pkg>/")
  → M file nodes inside prefix, ghost nodes for external deps
```

There is no Level 2 (sub-directory drill) in the current implementation — a `path_prefix` of `"pkg/api/"` returns individual file nodes, which is the terminal level.

---

## Global (SSE) Merge Semantics

`get_graph_json_global` with `mode="overview"` runs `overview_graph` per registered repo and merges results:

- **Nodes**: merged by ID, first-write-wins. `file_count` is per-repo, not summed across repos.
- **Edges**: accumulated — counts from all repos are added together.

**Known limitation:** when two repos have packages with the same name (e.g., both have `"src/"`), the node for the first repo's package wins and the second is silently dropped. This is acceptable for MVP (single-repo stdio is the common case); multi-repo disambiguation is deferred to a future RFC.

---

## Alternatives Considered

**Separate MCP tool (`get_repo_map_json`):** Rejected — the `mode` parameter keeps the tool count low and lets callers use a single tool for both overview and symbol lookup.

**Two-segment package keys (`"crates/travsr-mcp"` instead of `"crates"`):** Rejected — adds complexity with no clear benefit at MVP scale; top-level segmentation is sufficient for all current repos.

---

## Drawbacks

- `pkg:` ID namespace is synthetic and could theoretically collide with a real node whose VName signature starts with `pkg:`. Probability is near-zero given Kythe VName conventions, but should be addressed when VName validation is tightened (RFC-002 follow-up).
- `file_count` in the global SSE path is per-repo, which may surprise callers. Documented above as a known limitation.

---

## Unresolved Questions

- Sub-directory drill (Level 2+): should `path_prefix="pkg/api/"` return sub-dir groupings or flat file nodes? Currently flat. Needs UX validation.
- Ghost node dedup in global mode when the same external package appears in multiple repos.
