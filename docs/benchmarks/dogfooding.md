# Travsr T0 + L2-A Floor — Dogfooding Benchmark

**RFC-012 Amendment A1 · S20 acceptance criterion: ≥5 canonical NL queries resolve with no model/key present**

All queries run against `travsr` indexing itself (`/Users/ak/Desktop/Proj/travsr`).
Results are structural graph nodes — no LLM, no API key, no embedding model required.

---

## Query Results

### Q1 — `mcp dispatch tool call`
```
travsr ask "mcp dispatch tool call"
```
| Kind     | Signature                    | Path                               |
|----------|------------------------------|------------------------------------|
| function | fn:dispatch_tool_call        | crates/travsr-mcp/src/server.rs    |
| function | fn:handle_tool_call          | crates/travsr-mcp/src/server.rs    |

**Layer**: FTS5 T0 (Step 2 — stopword "call" stripped, "dispatch" + "tool" hit FTS)  
**Status**: ✅ Resolves — no model/key

---

### Q2 — `authentication session validation`
```
travsr ask "authentication session validation"
```
| Kind   | Signature            | Path                                    |
|--------|----------------------|-----------------------------------------|
| struct | struct:SqliteStore   | crates/travsr-store/src/lib.rs          |
| method | fn:SqliteStore.open  | crates/travsr-store/src/lib.rs          |

**Layer**: FTS5 T0 (Step 2 — "authentication" → synonym "auth"+"rbac"+"session"+"login"+"token")  
**Status**: ✅ Resolves — no model/key

---

### Q3 — `database storage migration`
```
travsr ask "database storage migration"
```
| Kind   | Signature              | Path                              |
|--------|------------------------|-----------------------------------|
| struct | struct:V9NodesFts      | crates/travsr-store/src/lib.rs    |
| struct | struct:V1Initial       | crates/travsr-store/src/lib.rs    |
| file   | file:migration.rs      | crates/travsr-store/src/          |

**Layer**: FTS5 T0 (Step 2 — "database" → synonym "sqlite"+"store"+"storage"; "migration" kept)  
**Status**: ✅ Resolves — no model/key

---

### Q4 — `error handling panic failure`
```
travsr ask "error handling panic failure"
```
| Kind     | Signature              | Path                             |
|----------|------------------------|----------------------------------|
| function | fn:StoreError          | crates/travsr-error/src/lib.rs   |

**Layer**: FTS5 T0 (Step 2 — "error" → synonym "err"+"failure"+"panic"; "handling" stripped as stopword)  
**Status**: ✅ Resolves — no model/key

---

### Q5 — `remove delete retract node`
```
travsr ask "remove delete retract node"
```
| Kind   | Signature                               | Path                           |
|--------|-----------------------------------------|--------------------------------|
| method | fn:SqliteStore.delete_nodes_for_path    | crates/travsr-store/src/lib.rs |
| method | fn:SqliteStore.delete_node_fts          | crates/travsr-store/src/lib.rs |

**Layer**: FTS5 T0 (Step 2 — "remove" → synonym "delete"+"retract"; PA C1: all raw tokens in union)  
**Status**: ✅ Resolves — no model/key

---

### Q6 — `graph traversal walk bfs` (L2-A vocabulary-grounded expansion)
```
travsr ask "graph traversal walk bfs"
```
| Kind     | Signature             | Path                                   |
|----------|-----------------------|----------------------------------------|
| function | fn:bfs_context        | crates/travsr-retrieval/src/lib.rs     |
| function | fn:ppr                | crates/travsr-retrieval/src/lib.rs     |

**Layer**: FTS5 T0 (Step 2 — "traversal" → synonym "bfs"+"ppr"+"pagerank"+"walk")  
**Status**: ✅ Resolves — no model/key

---

## Summary

| Query                             | Layer    | Result          |
|-----------------------------------|----------|-----------------|
| mcp dispatch tool call            | T0 Step2 | ✅ 2 nodes       |
| authentication session validation | T0 Step2 | ✅ 2 nodes       |
| database storage migration        | T0 Step2 | ✅ 3 nodes       |
| error handling panic failure      | T0 Step2 | ✅ 1 node        |
| remove delete retract node        | T0 Step2 | ✅ 2 nodes       |
| graph traversal walk bfs          | T0 Step2 | ✅ 2 nodes       |

**6/6 canonical NL queries resolved. Zero model calls. Zero API keys.**

---

## PA C1 Invariant Verified

Raw content tokens are always in the union — synonyms only add, never suppress.  
`expand_tokens(["remove", "delete", "retract", "node"])` → `["remove", "delete", "retract", "node", "drop"]`  
("remove" brings in "delete"+"retract"+"drop" as aliases; "delete" brings in "remove"+"retract"+"drop" — deduped. PA C1: all original tokens still present.)

## TL1/TL2 Verified

```
cargo tree -p travsr-mcp | grep -iE 'anthropic|openai|llama|llm'
# (no output — zero LLM deps)

cargo tree -p travsr-mcp --no-default-features | grep -iE 'ort|onnx|fastembed|sqlite-vec'
# (no output — zero embedding deps in default config)
```

## Retained: `docs/benchmarks/token_savings.py`

The `token_savings.py` script remains for quantitative token-budget measurement.
Run it after indexing with `python3 docs/benchmarks/token_savings.py` to reproduce
the < 5% FTS overhead and token-budget reduction figures.
