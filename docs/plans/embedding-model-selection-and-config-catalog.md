# Scope: Non-Chinese Embedding Model (arctic-embed-m) + Config-Driven Catalog

Status: **SCOPE + evaluation complete — implementation pending.** Repos affected:
`travsr` (main) + `travsr-embed`.

Supersedes the earlier granite-only scope. The model evaluation (Appendix A) changed
the chosen model from `granite-small-english-r2` → **`snowflake-arctic-embed-m-v1.5`**.

---

## 1. Goal

1. Replace / augment the Chinese BAAI-BGE family with a **non-Chinese** embedding model
   that is **at least as good as BGE** on our own retrieval workload and **tract-native**
   (no inference-speed regression).
2. Convert the model catalog from a **hardcoded Rust array** to a **config file** in
   both repos, so a model can be added without recompiling either binary.

## 2. Decision + evidence

**Chosen model: `Snowflake/snowflake-arctic-embed-m-v1.5` (arctic-embed-m).**

Measured on the `bench/` query set (16 answerable queries) against the real travsr
node corpus (4,114 symbol nodes), model-pure KNN, each model with its correct
pooling/prefix. Full methodology + all 7 models in **Appendix A**.

| model | dim | params | hit@1 | hit@3 | hit@10 | MRR | nonsense↓ | org | tract |
|---|---:|---:|---:|---:|---:|---:|---:|:--:|---|
| bge-small *(current)* | 384 | 33M | 0.688 | 0.812 | 0.875 | 0.766 | 0.691 | 🇨🇳 | native |
| bge-base | 768 | 109M | 0.688 | 0.875 | 1.000 | 0.797 | 0.628 | 🇨🇳 | native |
| bge-large | 1024 | 335M | 0.750 | 0.750 | 0.875 | 0.788 | 0.657 | 🇨🇳 | native |
| granite-r2 | 384 | 47M | 0.625 | 0.750 | 0.938 | 0.725 | 0.834 | 🇺🇸 | RoPE (slow) |
| codesage | 1024 | 130M | 0.688 | 0.812 | 0.938 | 0.768 | 0.243 | 🇺🇸 | ok |
| mxbai-large | 1024 | 335M | 0.750 | 0.812 | 0.875 | 0.786 | 0.629 | 🇩🇪 | native |
| **arctic-embed-m** | **768** | **109M** | **0.938** | **1.000** | **1.000** | **0.958** | 0.363 | 🇺🇸 | **native** |

**Why arctic-embed-m wins on every axis that matters:**
- **Accuracy:** best in the field — hit@1 0.938 (15/16 at rank 1) vs the pack's
  0.69–0.75, perfect hit@3, MRR 0.958 vs ~0.79. The margin (4 queries) is above the
  1-query noise floor of a 16-query set; second-best abstention (nonsense 0.363).
- **Non-Chinese:** Snowflake (US), apache-2.0 (commercial-safe).
- **tract-native, no regression:** standard BERT / absolute positions → **44 snips/s
  in tract, identical to bge-base** (confirmed via `spike_probe`). No RoPE penalty
  (granite), no ALiBi (jina).
- **Easiest to ship:** ONNX **already published** (onnx-community / Snowflake) — unlike
  granite & codesage, nothing to self-export or self-host.
- **Storage tunable:** 768-dim (3072 B/vector), and v1.5 is trained for **Matryoshka**
  truncation to 256-dim → 1024 B/vector (below today's bge-small).

**Confirmation run: WAIVED.** The margin over bge-large is decisive and the author
accepted it without a bigger-corpus / second-repo run. (Caveat retained for the record:
evidence is 16 queries on one repo, Rust/TS.)

## 3. Verified facts about arctic-embed-m

- **Architecture: standard BERT, `position_embedding_type: absolute`** — same family as
  BGE, so tract runs it at full BGE speed. `model_type: bert`, hidden 768, 12 layers.
- **ONNX inputs: 2** (`input_ids`, `attention_mask`) — the published export **drops
  `token_type_ids`** (single-sequence BERT needs no segment embeddings). So `n_inputs = 2`.
- **Pooling: CLS** (`1_Pooling/config.json`: `pooling_mode_cls_token: true`) — matches
  the current sidecar.
- **Query prefix:** `"Represent this sentence for searching relevant passages: "`
  (from `config_sentence_transformers.json`); **documents get no prefix.** Different
  from BGE's `"Represent this sentence: "` → must be config-driven.
- **Dim 768**, params 109M, max_seq 512, apache-2.0.
- **Matryoshka (v1.5):** truncate 768 → 256 with minimal quality loss (renormalize
  after slicing). Optional storage lever.
- **ONNX:** `Snowflake/snowflake-arctic-embed-m-v1.5` ships `onnx/model.onnx` (416 MB
  fp32) + fp16/int8/q4 variants. **Ship fp32** — quantized variants are slower in tract
  (no int8 kernels; shaky fp16), same lesson as Appendix B.

## 4. What is hardcoded today (unchanged from prior scope)

### 4a. Catalog
`crates/travsr-plugin-host/src/embed_catalog.rs` → `BACKENDS: &[EmbedBackend]`
(`&'static` array, 3 BGE entries). Consumed by `lookup()`, download/init, RAM-aware
worker sizing (`derive_num_workers`).

### 4b. Sidecar model runner — the BGE assumptions that break on arctic
`travsr-embed/src/model.rs` + `src/main.rs`:

| Hardcoded | Location | Current | Needed for arctic-embed-m |
|---|---|---|---|
| `dim_for_model()` match | main.rs:51 | 384/768/1024 by bge id | config-driven (arctic id → 768) |
| `QUERY_PREFIX` | model.rs:38 | `"Represent this sentence: "` | `"…for searching relevant passages: "` |
| 3rd `token_type` input | model.rs:144,157,163 | always sent (3 inputs) | **gate on `n_inputs`; arctic = 2** |
| CLS pooling | model.rs:177 | CLS | CLS (compatible ✓) |
| `MAX_SEQ` | model.rs:37 | 512 | keep 512 |

Note: the arch-awareness work is the **same** as the granite plan needed — a per-model
descriptor covering dim / prefix / n_inputs / pooling. arctic just happens to keep CLS
pooling and needs a different prefix + `n_inputs = 2`.

## 5. Implementation phases

The tract-compatibility **gate is gone** (arctic is standard BERT — already confirmed
loading + running in tract at 44 snips/s), and there is **no self-host export** step
(ONNX is published). So the plan is purely the config/descriptor refactor.

### Phase 1 — Sidecar becomes architecture-aware (Option C)
`travsr embed init`, when downloading a model into `~/.travsr/models/<id>/`, also
writes `~/.travsr/models/<id>/model.toml`:

```toml
# ~/.travsr/models/<id>/model.toml — written by `travsr embed init`
dim          = 768
pooling      = "cls"          # "cls" | "mean"
query_prefix = "Represent this sentence for searching relevant passages: "
n_inputs     = 2              # 2 = no token_type; 3 = classic BERT
truncate_dim = 0              # 0 = full; e.g. 256 for Matryoshka (renormalize after)
arch         = "bert"         # informational
```

Sidecar changes (`travsr-embed`):
- `main.rs`: replace `dim_for_model()` match with a read of `<model_dir>/model.toml`;
  fall back to BGE defaults (dim=384, cls, BGE prefix, `n_inputs=3`) when absent, so
  existing bge installs keep working with no re-init.
- `model.rs`: `BgeModel::load` takes a `ModelDescriptor`. Gate the 3rd input on
  `n_inputs == 3`; take `query_prefix` from the descriptor; keep CLS, add a `mean`
  branch (cheap future-proofing for codesage-style models); optional Matryoshka
  truncate+renormalize when `truncate_dim > 0`. Rename `BgeModel` → `EncoderModel`.
- Add `toml` + `serde` to `travsr-embed/Cargo.toml`.

### Phase 2 — Catalog to config (both repos)
- Add `EmbedBackend` fields (`pooling`, `query_prefix`, `n_inputs`, `truncate_dim`, `arch`).
- Move `BACKENDS` from the `&'static` Rust array to a bundled-default TOML via
  `include_str!("embed_catalog.toml")`, merged at runtime with an optional user
  override `~/.travsr/embed_catalog.toml`.
- `lookup()` returns an owned struct. Fix every call site (blast radius from
  `travsr graph BACKENDS`): `resolve_backend`, `derive_num_workers_for_cli`, all
  `spawn_background_reindex_*`, init/list/status. `&'static` → owned is the main cost.
- `travsr embed init` writes the per-model `model.toml` from the catalog entry.

### Phase 3 — Add the arctic entry
```toml
[[backend]]
id           = "arctic-embed-m-v1.5"
description  = "Best retrieval accuracy in-house (hit@1 0.94). Non-Chinese (Snowflake), standard BERT, tract-native. 768-dim, Matryoshka-truncatable."
dim          = 768
params_m     = 109
ram_mb       = 450
init_secs    = 47            # ~bge-base class; confirm
pooling      = "cls"
query_prefix = "Represent this sentence for searching relevant passages: "
n_inputs     = 2
arch         = "bert"
  [[backend.model_files]]
  name = "model.onnx"
  url_path = "onnx/model.onnx"
  hf_repo = "Snowflake/snowflake-arctic-embed-m-v1.5"
  size_hint_mb = 416
  [[backend.model_files]]
  name = "tokenizer.json"
  url_path = "tokenizer.json"
  hf_repo = "Snowflake/snowflake-arctic-embed-m-v1.5"
  size_hint_mb = 2
```
Expose in `travsr embed init` picker + VS Code model selector.

### Phase 4 — (optional) broaden the eval
Author waived the bigger-repo confirmation. If ever desired: rerun Appendix A's harness
on a second repo (e.g. the k8s corpus, a non-Rust language) to confirm the margin holds.

## 6. Risks
1. **`&'static` → owned catalog** touches many call sites; mechanical but broad.
   Mitigate with the `travsr graph BACKENDS` blast-radius list before editing.
2. **Backward compat:** absent `model.toml` must fall back to BGE-3-input defaults so
   existing installs don't break on upgrade.
3. **Migration:** switching an existing repo bge→arctic changes dim (384→768) → the
   HNSW must be rebuilt (`embed reindex`), and old bge embeddings in `embed.db` are a
   different model_id (already keyed by model_id, so they coexist).
4. **Cross-repo release ordering:** plugin-protocol/descriptor changes ship before the
   sidecar binary that reads them.
5. **Evidence breadth:** 16 queries, one repo (accepted). If arctic ever underperforms
   on a real user repo, Phase 4 is the fallback.

---

## Appendix A — Model evaluation (2026-07-02/03)

**Method.** Model-pure eval isolating embedding quality (no PPR/knapsack confounders):
each model embeds all 4,114 travsr symbol nodes (the exact `embed_text` the sidecar
builds — `kind: sig | module: path | callers | callees`) + the `bench/queries.json`
queries, with its **correct pooling + query prefix + input count**; cosine-KNN; a query
HITS if any top-k node's signature OR path contains an `expect` substring. Computed via
ORT (Python) — valid for our tract setup because **tract and ORT produce numerically
identical vectors** (verified cos 1.00000 for granite; and ORT bge-small = 0.688 matches
the real tract `bench/` baseline of 0.69). Full table in §2.

**tract speed (via `spike_probe`, seq23/batch8, single-thread):** bge-small 93/s,
bge-base 44/s, **arctic-m 44/s**, granite 73/s, codesage 58/s, jina-code 34/s.

**Caveats:** 16 answerable queries (1 query = 0.0625 on hit@1); one repo (travsr, Rust/TS);
model-pure KNN not the full `get_context` pipeline. Cross-model `nonsense_cos` is scale-
sensitive across different pooling (codesage's low 0.243 is partly mean-pooling scale).

## Appendix B — Why not the others (rejected candidates + tract learnings)

- **granite-small-english-r2 (IBM):** ModernBERT / **RoPE**. Weakest accuracy of all 7
  (hit@1 0.625) *and* worst abstention (0.834). tract learnings from the spike:
  - Stock `onnx-community` export **fails in tract** — uses the `com.microsoft.MultiHeadAttention`
    contrib op. An unfused `optimum` re-export works (cos 1.0 vs ORT) but must be self-hosted.
  - **RoPE is the tract slowdown**, not matmuls/GeLU/LayerNorm: rotary at every layer
    explodes into memory-bound Slice/Cast/Mul (84/90/139 vs bge's 1/1/50). tract runs
    them un-fused; dominates at short seq. Only fix = a custom fused-RoPE tract op (deferred).
  - **int8 is *slower* in tract** (no int8 GEMM kernels): 4× smaller, 1.7× slower.
    **Graph cleanup (O1 / onnx-simplifier) is redundant** — tract's `.into_optimized()`
    already collapses ~40% of nodes on load. → For tract, **ship plain fp32.**
- **codesage (Amazon):** code-specific, mean-pooled, absolute positions (tract-ok, 58/s).
  Only reached bge-*small* accuracy at bge-*large* cost (1024-dim, ~500 MB, self-host
  export, custom `trust_remote_code`). Its one edge — very low nonsense cosine (0.243,
  best abstention) — is partly a mean-pooling scale artifact (unconfirmed). Kept on file
  as the "abstention" candidate if that ever becomes the priority.
- **jina-embeddings-v2-base-code (Jina, DE):** ALiBi; slowest in tract (34/s); not
  evaluated on accuracy (dropped after the tract-speed result).
- **mxbai-embed-large-v1 (mixedbread, DE):** standard BERT, tract-native, but only
  bge-large-level accuracy at 335M/1024-dim — no reason to pick over arctic-m.
- **SFR-Embedding-Code (Salesforce):** RoPE **and** cc-by-nc (non-commercial) → excluded.
- **nomic-embed-text (Nomic):** RoPE → tract slowdown → excluded.
