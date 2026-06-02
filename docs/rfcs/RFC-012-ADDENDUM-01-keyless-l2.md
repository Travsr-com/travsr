# RFC-012 · Addendum 01 — Key-Free L2 Query Understanding

| Field | Value |
|---|---|
| **Status** | Draft (ratification gate before fold-in to RFC-012) |
| **Author** | Tech Lead |
| **Date** | 2026-06-02 |
| **Revision** | Rev 3 — folds in the SOTA retrieval synthesis (*Advanced Retrieval Architecture for Deterministic Code Graph Navigation*): SymSpell+FST for L2-A scale, MRL+RaBitQ+AVX-512 for L2-B footprint, Elias-Fano for hyperscale postings. Rev 2 hardened against a 5-persona review (Principal Architect · Principal Security Engineer · Tech Lead · Solution Architect · Senior SWE). See § Review Record. |
| **Supersedes** | RFC-012 § "Detailed Design — L2: LLM Query Translator" (§2.0–§2.7) |
| **Leaves intact** | L1 (shipped, #258, migration `v9_nodes_fts.sql`), L3 sidecar mechanics (§3.x) — L3 is *re-roled*, not removed |
| **Issue** | #258 |
| **Decision owner** | Principal Architect (same gate as RFC-012); security-surface sub-gate: Principal Security Engineer (L2-C/L2-D only) |

---

## Summary

The ratified-Draft L2 puts a **hosted-LLM query translator in the MCP client, default-on**, and notes the model defaults to "whatever the MCP client already has access to (Claude, GPT, local)." In practice this forces the operator to **supply and pay for an LLM API key** for the natural-language translation step.

This addendum removes that requirement. It replaces "client-side hosted LLM (bring a key)" with a **four-mechanism, key-free ladder** (here and throughout, *key* means an **LLM API credential** — nothing in this design changes database primary/foreign keys or node identity): a deterministic, vocabulary-grounded algorithm as the always-on default (**L2-A**); the already-planned **local** embedding sidecar re-roled as the semantic tier (**L2-B** = L3); the host LLM's own (free) translation when the client *is* an LLM (**L2-C**); and an opportunistic, capability-gated `sampling` borrow (**L2-D**). **No mechanism requires a user-provisioned key.**

Scope note on the manifesto: this makes the **default** seed-understanding path a pure algorithm, so the §2.0 "owned exception to *algorithms first, LLM last*" is **no longer needed for the default path.** It is *narrowed*, not abolished — L2-C and L2-D remain LLM tiers (host-side only, opt-in, never in the daemon), and that residue is ratified explicitly here rather than hand-waved away.

---

## Why the ratified-Draft L2 is mis-targeted

L2 attaches a paid translator to the MCP **client**. But enumerating which clients exist shows the key is either redundant or avoidable in every case:

| Client | Who poses the NL question | LLM already in the loop? | Consequence of hosted-LLM L2 |
|---|---|---|---|
| Claude Desktop / Code / Cursor / Continue | A frontier LLM **is the client** | **Yes** — it already decomposes "what handles MCP requests" into `search_symbol("dispatch")` as normal tool-calling | **Double-billed**: a *second* translation of work the host already did for free |
| `travsr ask` (CLI), scripts, `curl`, cloud SSE tier | A human / script; no upstream model | No | **Worst place to demand a key**: a CLI user will not paste an OpenAI key to run one query |
| Standalone IDE panel (VS Code, JetBrains) | A human clicking | Sometimes (host may expose a model) | Key demanded even when a host model is reachable for free |

The API-key requirement is therefore not load-bearing. It is an artifact of placing the translator in the wrong tier — on the consumer side, as a generative call, instead of (a) in the deterministic engine where most of the value is closed-vocabulary lookup, and (b) deferring the genuinely-semantic remainder to a *local* model or the host's *own* model.

---

## Decision — the key-free L2 ladder

Strict fallback order. The first mechanism that yields ≥1 fragment wins; lower tiers are consulted only on miss. **None requires a user-supplied key.** The **Off-box?** column is load-bearing for the security gate.

| # | Mechanism | Where it runs | User key? | Off-box egress? | Determinism | Default? |
|---|---|---|---|---|---|---|
| **L2-A** | Algorithmic vocabulary-grounded expansion | In the daemon (`travsr-store`), additive step in `search_nodes_fuzzy` | None | **No** | Full | Yes (all clients) |
| **L2-B** | Local embedding semantic bridge — **L3 re-roled** | On-device ONNX model, no network | None | **No** | Full (fixed model bytes) | Opt-in (`--features embedding`) |
| **L2-C** | Host-native translation — the host LLM already translates | Host LLM | None (host's own inference) | Inherent (the host already sees the query) | Best-effort | Yes, for LLM hosts |
| **L2-D** | Borrowed host model via MCP `sampling/createMessage` | Client's configured model, via protocol | None (reuses host model) | **Yes — requires explicit opt-in + egress notice** | Best-effort | **Off by default; capability-gated; near-vestigial at ship** |

Invariant preserved: **no LLM runs in the daemon.** L2-A is pure string algebra (same category as the existing `tokenize_identifier`); the generative tiers (L2-C, L2-D) stay on the host/client side. RFC-012 §2.1's rule ("the MCP server is purely an algorithmic graph engine; the LLM lives on the consumer side") is *upheld* — the only thing this addendum moves into the daemon is an algorithm.

---

## Detailed Design — L2-A: algorithmic vocabulary-grounded expansion

**The reframing that makes this work:** translating a natural-language query into seeds for *this* graph is **not** open-domain language understanding. It is mapping English words onto a **closed, known vocabulary** — the exact set of tokens `tokenize_identifier` already emits for every node. You do not need a frontier model to learn that this repository spells the concept `dispatch`; the repository already told you.

L2-A is a deterministic expansion step. It changes no MCP tool schema, no transport, and benefits every client at once.

### Where it slots (no-regression by construction)

It runs as an **additive Step 3** inside `search_nodes_fuzzy` (`crates/travsr-store/src/lib.rs:889`), reached **only when Step 1 (exact substring, `lib.rs:892`) and Step 2 (raw FTS `build_match_expr`, `lib.rs:899`) both miss.** Because every query that resolves today resolves at Step 1 or Step 2 unchanged, the merged L1 conformance tests cannot regress — L2-A only fires where the answer is currently *empty*.

```
Step 1  exact substring            (unchanged, lib.rs:892)
Step 2  raw FTS build_match_expr    (unchanged, lib.rs:899)
Step 3  L2-A expand_query → FTS     (NEW — only on Step 1+2 miss)
Step 4  L2-B local embeddings       (only if --features embedding, on Step 3 miss)
```

### `expand_query`

```rust
// L2-A — deterministic NL → identifier fragments, grounded in the graph's
// own vocabulary. Lives in travsr-store; no LLM, no network, no key.
// Reads the vocabulary via &self; the vocabulary table is MAINTAINED in the
// write/retract paths (see "Vocabulary cache" below), not here.
fn expand_query(&self, nl: &str) -> Result<Vec<String>, StoreError> {
    // 1. tokenize_identifier(nl); drop a small static stop-word set
    //    ("what","how","does","the","where","is","are","that","which",
    //     "code","function","method"...). NL question words carry no signal.
    //
    // 2. For each content token, gather candidates from TWO closed dictionaries:
    //    (a) the graph's own vocabulary (the fts_vocab table — see below),
    //        ranked by trigram-Jaccard similarity, edit-distance tie-break.
    //        GROUNDED: can only ever propose tokens that DEMONSTRABLY EXIST,
    //        so it cannot hallucinate a symbol or path.
    //    (b) a small static programmer-jargon lexicon (compiled-in const slice):
    //        auth↔authentication/authorize · db↔database · req↔request
    //        cfg↔config · msg↔message · recv↔receive · init↔initialize
    //        repo↔repository · ctx↔context · dispatch↔route↔handle
    //        fetch↔get↔load · persist↔save↔store · del↔delete↔remove ...
    //        Bridges the synonym gaps trigram CANNOT (handle→dispatch).
    //
    // 3. Sort all candidates by a TOTAL order (jaccard desc, edit_dist asc,
    //    token asc) — deterministic, no HashMap iteration order. Then cap to
    //    the FINAL OR-ARM budget (see "OR-arm budget" below), NOT to a
    //    pre-expansion fragment count. OR-join the survivors and hand them to
    //    the existing build_match_expr / FTS path.
    todo!()
}
```

### Vocabulary cache — the load-bearing correctness fix

The vocabulary is **not** a `SELECT DISTINCT` over `nodes_fts`: that table is `content=''` and exposes no token column. Tokens live as space-joined strings in `nodes_fts_map.tokens`. So L2-A introduces:

```sql
-- migration v10 (L2-A; ships in S20, after the shipped v9 L1 FTS table)
CREATE TABLE IF NOT EXISTS fts_vocab (
    token    TEXT    PRIMARY KEY,   -- distinct identifier token, [a-z0-9_]+ only
    refcount INTEGER NOT NULL       -- live nodes whose tokens include this token
);
```

**Maintenance must hook the paths that actually run — not the dead one.** In the merged code:
- `delete_node_fts` (`lib.rs:772`) is `#[allow(dead_code)]` and is **never called**.
- The live deletions are the bulk SQL paths `delete_nodes_for_path` (`lib.rs:375`) and `delete_nodes_for_path_prefix` (`lib.rs:433`), which hand-roll FTS retraction inline.

Therefore the refcount deltas must be folded into **`put_node_fts` (insert + rename-retract) AND both bulk-delete sites**, inside their existing transactions, derived from the same `nodes_fts_map.tokens` strings those sites already read/write. Hooking only a `put/delete` pair (as Rev 1 implied) would leak decrements on every real delete → stale vocabulary → tokens proposed for symbols that no longer exist, breaking the "always-fresh" non-negotiable.

### OR-arm budget (the BM25-noise seam)

`build_match_expr` already OR-joins *all* tokens of a single query. If L2-A feeds it 5 *expanded* fragments, each itself OR-expanding into synonyms + trigram candidates, the effective arm count is `5 × (synonyms + candidates)` — which can exceed FTS5's practical clause budget and flatten BM25 into noise (every node matches *something*). **The cap is therefore on the final OR-arm count (≤ 16), applied after the deterministic sort — not on a pre-expansion fragment count.**

### Determinism & scale

- **Determinism:** vocabulary loaded into a `BTreeSet<String>` (or sorted `Vec`) once per process; total-order ranking; cap-after-sort. No `HashMap`/`HashSet` on any output path. Identical query + identical graph → identical fragments, every run.
- **Scale:** a per-token linear scan over `fts_vocab` is sub-millisecond at the 2.6k-node self-index. It is `O(V·|q|)` and **must not** be claimed sub-ms at 75M-node scale. The MVP ships the linear scan (correct, simple, fast enough on the L1-miss-only path at MVP node counts); the scale-out structure is **SymSpell pre-computation encoded into an FST** (§ "Scale-out: SymSpell + FST", which resolves Open Q #10). The cost table below is the self-index measurement, not a universal claim.

### Scale-out: SymSpell + FST (resolves Open Q #10)

The Rev 2 deferral named "a trigram inverted index or BK-tree over `fts_vocab`." The research synthesis supersedes that choice with a strictly better pairing — **SymSpell** for the algorithm, an **FST** for the storage — and also fixes a recall bug Rev 2's trigram fallback carried:

- **The `<3`-char gap is a *tokenizer* decision, not something SymSpell alone fixes — corrected from the paper's framing.** The paper credits SymSpell with indexing the two-char tokens (`db`, `fs`, `tx`, `id`, `io`) that FTS5 trigram cannot. **In Travsr that is currently moot:** `tokenize_identifier` already drops every token `< 3` chars (`fts_tokenize.rs:81`, `tokens.retain(|t| t.len() >= 3)`), so those tokens never enter `fts_vocab` and the FST has nothing to key them on regardless of SymSpell's length-agnosticism. SymSpell is length-agnostic *in principle*, but capturing sub-3-char tokens here requires **first lowering the `tokenize_identifier` vocab-path threshold** (and re-assessing the BM25-noise/charset impact that the `>= 3` filter exists to control) — a separate change, tracked in Open Q #15. The real, defensible win of SymSpell is below.
- **SymSpell (Symmetric Delete) — the real win is typo/abbreviation tolerance on the ≥3-char vocabulary, in constant time w.r.t. vocabulary size.** Instead of generating insertions/substitutions/transpositions at query time (the `O(alphabet^d · |q|)` blow-up that is fatal for multi-lingual / Unicode identifiers), pre-compute — **at index time only** — every deletion variant up to edit distance `d` for each `fts_vocab` token, mapped back to the source token. At query time, generate just the deletion variants of the input and probe them as exact keys. This makes the lookup **`O(1)` in vocabulary cardinality `V`** (independent of `|fts_vocab|`) — replacing the `O(V·|q|)` linear scan. It is **not** constant in query length or `d`: candidate generation is `O(\binom{|q|}{≤d})` deletes per probe and each probe may return several source tokens to rank, so cost is governed by `|q|` and `d`, both bounded (`d=1` shipped). Alphabet-agnostic by construction.
- **FST (Finite State Transducer) for the deletion map.** SymSpell's deletes balloon storage; a naïve `HashMap` of variants blows the tarball/heap budget. Encode the delete→source map into an FST (BurntSushi `fst` crate): it compresses shared prefixes/suffixes into a minimal DAFSA, **serializes to disk, and is accessed by `mmap`** — near-zero resident heap, lookups straight against the memory-mapped bytes. The query path does **not** build a Levenshtein automaton at runtime (that is the heap-spiking path the `fst` docs warn against); it computes the SymSpell deletes of the input and does exact lookups.
- **`d` is bounded.** Ship `d = 1` (covers the dominant single-typo / single-abbreviation case) and treat `d = 2` as a tunable; the delete-count grows binomially in `d`, so the FST size is the governing budget.

**Two honesty caveats this introduces (do not paper over):**
1. **New dependency — the "no new deps" claim is narrowed.** The `fst` crate is **not** in ADR-001's allowed set. L2-A's *MVP* (linear scan + const-slice lexicon) still adds **zero** deps; the *scale-out* path adds `fst` and must clear an **ADR-001 amendment + Tech Lead sign-off** before it lands. The MVP shipping with no new dep is unchanged; only the >500k-node tier takes the dependency. (See Open Q #10, Dependencies note, AC (g).)
2. **Still L1-miss-only and still grounded.** SymSpell/FST changes the *index structure*, never the ladder: it fires only on Step 1+2 miss, and every key in the FST is a token that demonstrably exists in `fts_vocab`, so the grounding/anti-hallucination invariant (candidate (a) can only emit live tokens) is preserved. The FST is rebuilt/patched from the same `fts_vocab` refcount maintenance points — when a token's refcount hits 0 it leaves the vocabulary and must leave the FST, or "always-fresh" breaks.

### Dependencies & charset (no new deps; safe-by-construction)

- Static lexicon: `&[(&str, &[&str])]` **const slice** — zero new dependencies, MSRV-1.75-safe. **Do not** add `phf`/`strsim` (not in ADR-001's allowed set) or use `LazyLock` (Rust 1.80 > MSRV). Bidirectional `auth↔authentication` via a symmetric const table.
- **MVP adds no deps.** The only new dependency in this addendum is `fst`, and it belongs **exclusively to the scale-out tier** (SymSpell delete-map storage, §"Scale-out: SymSpell + FST"), not the MVP linear-scan path. It does **not** land until an ADR-001 amendment + Tech Lead sign-off, and only ahead of a >500k-node deployment. Until then every default build is dep-clean and key-free.
- **Charset constraint:** every `fts_vocab` token and every lexicon entry is constrained to `^[a-z0-9_]+$` (CI-asserted). This matters because of crate layering: `validate_mcp_arg` lives in `travsr-mcp`, which sits *above* `travsr-store` (`mcp→retrieval→store`), so it **cannot** wrap a store-layer code path. Constraining the vocabulary/lexicon at the source makes derived fragments safe-by-construction — they cannot smuggle `../`, `:`, null, or control chars into `build_match_expr` (which also double-quotes every token, belt-and-suspenders).

### Cost (self-index measurement, not a universal claim)

| Resource | L2-A |
|---|---|
| Binary size | + a few KB (static lexicon + stop-words) |
| Disk | `fts_vocab` table; bounded by distinct-token cardinality (~single-digit KB at 2.6k nodes) |
| Latency | sub-ms at self-index scale (linear scan); bounded at large scale via SymSpell+FST `O(1)`-in-`V` lookup (Open Q #10, resolved) |
| Network / model / key | none |

---

## Detailed Design — L2-B: local semantic bridge (L3 re-roled)

The residual L2-A cannot reach is open-ended paraphrase — "the thing that decides which handler runs" → `dispatch`. That is a **semantic** match, which is exactly what an embedding does. RFC-012 already specifies a **local** `bge-small` sidecar (§3.x) that runs on-device, is downloaded once via `travsr embed install`, and **needs no API key at any point.**

The conceptual change: **L3 *is* the semantic translator L2 was reaching a hosted LLM for** — minus the credential and minus the network. Re-roling it (rather than keeping a separate hosted-LLM "semantic L2") removes an entire layer of justification and cost. Query→node cosine-NN bridges synonyms locally and deterministically (fixed model bytes, §3.10). All §3.x mechanics — build gating, the `vec0` schema (renumbered **v11**, see fold-in), the 10-NN fetch merged under the 50-seed cap — are unchanged in *ladder position*; the **footprint and compute path** are upgraded per the research synthesis below. L3's "embeddings last, behind a feature flag" quarantine is fully preserved; only its *label* and its *storage representation* change.

### Footprint problem (why raw FP32 violates the tarball budget)

§3.x as drafted stores FP32 embeddings via `sqlite-vec`. At 384–1024 dims that is ~1.5 KB/node and a brute-force (no ANN) scan — a multi-million-node index easily eclipses several GB, blowing the **25–50 MB tarball limit (ADR-004)** and saturating client memory bandwidth past the 50 ms bound. The synthesis fixes both axes — dimension count **and** per-coordinate precision — without the naïve-downsampling recall cliff:

### Axis 1 — Matryoshka Representation Learning (dimension truncation)

Use an **MRL-trained** embedding model (e.g. MRL iterations of `bge-small` / `mxbai-embed`). MRL concentrates high-variance semantic information in the *leading* dimensions, so the vector can be **truncated at runtime** (1024 → 256 / 128 / 64) by simply dropping the tail — no retraining, no second inference, no model swap. Blindly truncating a *non-MRL* vector destroys the geometry (96% → sub-60% recall); MRL truncation to ~256 dims retains **>96%** of full-model retrieval. This is what makes a *local* sidecar fit on-disk at all.

### Axis 2 — RaBitQ (1-bit quantization with a theoretical error bound)

Quantize the truncated dimensions to **1 bit** via **RaBitQ** — an ~32× compression to **~32–40 B/node** (32 B payload + 8 B of scalar correctors). RaBitQ ≠ naïve sign-bit binarization (which suffers severe angular distortion on the anisotropic distributions real embedding models produce). RaBitQ:
1. **Random rotation (Johnson–Lindenstrauss) — with a *pinned* seed (load-bearing for determinism).** Multiply every normalized vector by a random orthogonal matrix to force an isotropic distribution before bit-casting — no dimension carries disproportionate weight, so rounding does not discard critical variance. **The matrix must be derived from a constant, committed, schema-versioned PRNG seed**, shipped as part of the model/index artifact, and the query vector must undergo the *identical* rotation. If the seed is not pinned: (a) index-time and query-time rotations diverge → silent recall collapse; (b) two index builds of the same graph emit different bit payloads → the "fixed model bytes / Full determinism" claim (decision table, § Determinism) is false and "always-fresh" reindex-on-commit becomes non-reproducible. CI must assert byte-identical quantization for a fixed input+seed (AC (j); threat T13).
2. **Centroid snapping.** Snap each rotated vector to the nearest hypercube vertex on the unit sphere; the quantized vector is just the sign bits. The centroid configuration is mathematically predetermined, so — unlike Product Quantization — **no floating-point codebook** is stored or queried in RAM.
3. **Per-vector scalar correctors.** Store two 32-bit floats (centroid distance + inner product) per vector; at query time they algebraically correct the Hamming baseline into an **unbiased, sub-Gaussian estimate** of the true cosine similarity. Topology of the nearest-neighbour search is preserved.

| Method | Payload (256-dim) | Codebook? | Fidelity |
|---|---|---|---|
| Naïve FP32 | 1024 B | No | Exact |
| Product Quantization | 64–128 B | Yes (heavy RAM) | Moderate distortion |
| Naïve binary (sign bit) | 32 B | No | Severe distortion |
| **RaBitQ (1-bit + scalars)** | **32 B + 8 B** | **No** | **Unbiased estimator** |

### Axis 3 — Hamming distance, with a *safe* portable baseline and gated SIMD

RaBitQ comparison is **Hamming distance** = XOR + popcount over the bit-strings — no floating-point FMA in the *baseline* compare.

**Default / MVP path is safe and MSRV-clean.** The always-correct baseline is plain **`u64::count_ones()`** (stable since Rust 1.0; LLVM lowers it to hardware `POPCNT` on x86 and `CNT` on aarch64 and auto-vectorizes loops). This is the **only** Hamming path the default `--features embedding` build needs; it is safe Rust, MSRV-1.75-clean, and already portable across x86 and ARM.

**Explicit SIMD intrinsics are deferred and double-gated — they are NOT MVP.** A further AVX-512 `VPOPCNTDQ` (`_mm512_popcnt_epi64`) / ARM NEON `vcnt` (`vcntq_u8`) acceleration is a *throughput-only* upgrade, and it trips **two** project gates that the baseline does not:
- **`unsafe` (CLAUDE.md principle 7).** `core::arch` SIMD intrinsics are `unsafe fn`. Principle 7 / the PR convention ("no `unsafe` without an RFC + Tech Lead sign-off") therefore fires. The SIMD fast path **must not land on the strength of this addendum alone**: it requires its own `unsafe`-authorizing RFC + Tech Lead sign-off, with a `// SAFETY:` contract on every intrinsic block, behind `is_x86_feature_detected!` / `std::arch::is_aarch64_feature_detected!`.
- **MSRV (CLAUDE.md principle 6 toolchain).** `_mm512_popcnt_epi64` (the AVX512VPOPCNTDQ intrinsic) was unstable until **Rust 1.89** — it is **not** MSRV-1.75-safe. The doc's "MSRV-1.75-safe" guarantee covers the `count_ones()` baseline only; the AVX-512 intrinsic path requires raising MSRV for that feature/arch, stated explicitly when it is proposed. (`std::simd`/portable-SIMD is likewise nightly-only and not an MVP option.)

> **ARM64 / OCI is satisfied by the baseline, not by AVX-512.** AVX-512 is **x86-only**; CLAUDE.md principle 6 mandates **ARM64 (aarch64) for OCI A1** and the cloud-SSE tier runs there. Because the *baseline* `count_ones()` already lowers to ARM `CNT`, the cloud/ARM tier is correct and fast **without any intrinsic**. AVX-512 (x86) and NEON (aarch64) are optional accelerations on top, never the only path.

**Cross-architecture determinism — the popcount is exact, but the *ranked output* is the real gate.** Popcount is integer-exact across scalar/AVX-512/NEON, so the Hamming baseline is identical everywhere. But the RaBitQ *distance estimate* then applies two FP32 scalar correctors (centroid distance + inner product); FP reduction order / FMA can differ across x86 vs aarch64 and perturb the **nearest-neighbour ordering** that actually feeds seed selection. The determinism gate is therefore **identical *ranked seed output* across architectures** (canonical/total-ordered corrector application or fixed-point accumulation), not merely identical popcount — see AC (i) and Open Q #13.

### Net footprint

MRL (256-dim) + RaBitQ (1-bit) = **~40 B/node** including correctors — ~38× smaller than FP32, comfortably inside the ADR-004 tarball budget, and the same `--features embedding` quarantine and "L2-B only on L2-A miss" gate as before. (The paper's "whole topology resides in L3 cache" claim is **dropped**: at ~40 B/node a 75M-node graph is ~3 GB — far past any L3; L3 residency holds only at small/self-index-class graphs and is not load-bearing here.) Recall validation (RaBitQ + MRL truncation vs FP32 baseline on the dogfooding query set) is a ship gate, not an assumption.

### Crate placement (layering)

Mirroring the L2-A placement: the **vector store + RaBitQ quantization live in `travsr-store`**; the **Hamming/ANN seed *ranking* lives in `travsr-retrieval`** (allowed to depend on `store`). The RocksDB-tier Elias-Fano postings + FST column families are also `travsr-store` concerns. No ranking logic reaches "up" the dependency graph, so the `core→…→store`, `retrieval→core+store` rules hold.

---

## Detailed Design — L2-C: host-native translation (highest quality, near-free — but not zero-surface)

For the largest client class — LLM hosts — the host LLM already converts the NL question into specific tool arguments. There are **two** distinct levers, with different costs:

1. **Richer tool descriptions (low-cost, schema-compatible).** Enrich the `description` fields of `search_symbol` / `get_context` so the host LLM reliably expands NL → good fragments. Under RFC-004 versioning this is a **Patch** (no contract change). **Critical:** the descriptions are emitted by **two** generators — `tools_list()` (`server.rs:150`) and `tools_list_global()` (`server.rs:410`). Edit **both**, or stdio clients improve while global/cloud-SSE clients silently do not.
2. **An MCP `prompts` entry (net-new surface — not free).** The daemon today advertises only `capabilities:{tools:{}}` (`server.rs:63,330`; `sse.rs:569`). Shipping a `prompts` capability is genuinely new daemon protocol surface and routes through the security sub-gate before merge.

Egress: L2-C adds **no new** off-box flow — the host LLM is the client and inherently already sees the query. Double-billing disappears because translation happens exactly once, on inference the user already pays for.

---

## Detailed Design — L2-D: borrowed host model via MCP `sampling` (future / opportunistic, default-off)

A thin slice remains: a client that advertises MCP's `sampling/createMessage` capability (the protocol's "ask the client to run a completion on the server's behalf"). **This is net-new, currently unimplemented surface**, and must be specified honestly:

- **Capability negotiation:** the *client* advertises `sampling` in the `initialize` handshake; the server must **inspect and store** that capability on the session, then gate any outbound `sampling/createMessage` on it. This is **new server code**, not docs-only.
- **Reality at ship:** no current Travsr client advertises `sampling` — the VS Code extension sends `capabilities:{}`. Among hosts, Claude Desktop supports sampling; Cursor / Copilot / Continue are inconsistent. So L2-D is **near-vestigial at ship**; it is a forward-looking option, not a load-bearing tier. For the CLI (`travsr ask`) and `curl` it is structurally unreachable (no upstream model) — L2-A/B carry those clients.
- **Safety envelope (all required):** default-off; fires only when the capability is present **and** the user has opted in; **single** completion per query; the §2.2 caps (≤5 fragments, ≤3 globs) and §2.5 `:` bypass apply verbatim; the daemon holds **no** credential; an **audit-log line** records every sampling borrow; an **egress notice** is shown because the user's NL query leaves the box.

---

## Determinism & Security (rewritten per security sub-gate)

The blanket "security improves" claim from Rev 1 is **withdrawn.** The accurate statement:

- **Default path (L2-A/B) genuinely improves posture.** It makes no generative call and no network call, so the common-case prompt-injection surface shrinks and "local first" holds offline. L2-A is grounded — candidate (a) can only emit tokens already in `fts_vocab` — so it cannot invent a path or symbol (benign-on-error by construction). The daemon-side determinism floor (identical structured inputs → identical `search_nodes_fuzzy` outputs) is unchanged and reached far more often.
- **L2-C / L2-D move query text off-box and add protocol surface.** They are **not** "free, zero new failure modes." Removing the API key also removes the friction that implicitly gated egress and cost — so off-box flows must be **explicit opt-in + documented egress notice**, never silent.

**Threat-model additions (MUST land before merge):**

| ID | Threat | Mitigation |
|---|---|---|
| **T11** | **Jargon-lexicon poisoning.** A malicious PR adds `auth↔adminbypass`, silently redirecting every "auth" query's seeds. "Additive recall only" is false when the injected token *exists* in the target repo. | Lexicon is a CODEOWNERS-gated constant; CI asserts every entry matches `^[a-z0-9_]+$`; an additive-recall test asserts no lexicon entry can suppress a legitimate match. |
| **T12** | **MCP `sampling` server-initiated egress/abuse.** A malicious/compromised server drives the client's paid model (cost + injection amplification); the user's query egresses to whatever model the client wired up. | L2-D off by default; capability-present **and** user-opt-in; one completion per query; audit-log line; no daemon-held credential; in the future cloud SSE tier a sampling borrow must never cross a tenant boundary (ties to T7). |
| **T13** | **Quantization/index-artifact integrity (Rev 3).** The mmap'd FST and the RaBitQ `vec0` payload are *derived binary artifacts read without parse-time validation*. A substituted/hand-edited FST could map a benign delete-key to an out-of-charset or non-existent "source token" (T11 one layer down); a swapped `vec0` payload or a rotation seed that differs from the query path could skew seed selection toward attacker-chosen nodes, or silently degrade recall (looks like a quality regression, not a security event). | FST builder **re-asserts** every emitted source token still matches `^[a-z0-9_]+$` **and** exists in `fts_vocab` at build time (grounding re-checked at the FST boundary, not inherited from the table); RaBitQ rotation seed pinned+versioned (T-row sibling of the determinism gate); FST/`vec0` artifacts either **rebuilt-from-`fts_vocab` on load** or covered by **release signing + SLSA provenance** rather than trusted as-is; CI reproducibility assertion (fixed input+seed → byte-identical artifact). |

**Supply-chain note (Rev 3):** the scale-out / hyperscale tiers add **three** dependencies — `fst` (L2-A SymSpell map), a **named** RaBitQ/quantization helper crate (L2-B), and `ef_rs`/Elias-Fano (RocksDB tier, post-Phase-5). None is in the MVP default build. Each lands only behind an ADR-001 amendment with a **hard pre-merge gate** (not just "vetting"): pinned exact version, `cargo-deny` advisories+licenses green, license in the ADR-001 allowlist, a named maintenance/CVE bar, recorded in the amendment. The RaBitQ helper in particular is young/low-download and its popcount path may carry `unsafe` — if so it **also** trips the principle-7 RFC gate (§ Axis 3), independent of the ADR amendment. The mmap'd FST is built from the daemon's own `fts_vocab` (no external *query* input) — but it is a **derived on-disk artifact consumed without parse-time validation**, so its integrity is covered by T13 (re-validate-on-build + sign/provenance or rebuild-on-load), not assumed. **No new off-box egress:** SymSpell/FST, MRL, RaBitQ, Elias-Fano are all on-device; the only artifacts that *download* (the MRL model via `travsr embed install`, a prebuilt FST if ever shipped) reuse the existing signed-release channel and add no new fetch endpoint or telemetry.

**Other security fixes:** the arg cap is **512 bytes** (`sanitize.rs:22`), not 256 — correct this here and in the §2.7 fold-in. All tiers' fragments still pass the §2.2 caps; L2-A fragments are safe-by-construction via the charset constraint above (since `validate_mcp_arg` is upstream of the store and cannot wrap L2-A directly).

---

## Why this is better than the ratified-Draft L2

- **The actual ask: no key on any default path.** The only LLM paths reuse a model the host already runs; none needs a user-provisioned key.
- **No double-billing** inside LLM hosts — the host translates once, for free (L2-C).
- **Stronger default determinism** — the deterministic algorithm is the default; the non-deterministic LLM call is opt-in and last.
- **Manifesto exception removed for the default path** — default seed-understanding is an algorithm; the residual L2-C/L2-D LLM use is explicitly ratified and host-side only.
- **Offline / local-first default** — L2-A and L2-B need no network (CLAUDE.md principle 3).
- **Smaller default-path attack surface**, with off-box tiers gated + consented rather than implicitly key-gated.

---

## The one honest tradeoff

For a **CLI user, without the embedding model installed, asking a highly abstract question**, L2-A alone will not match a frontier model on open-ended paraphrase. That user (a) still gets a large jump over today's raw-FTS `travsr ask`, and (b) opts into frontier-free semantics with a single `travsr embed install`. Every other scenario — the entire LLM-host class (L2-C), plus any client with embeddings (L2-B) — meets or beats the ratified-Draft L2. Deleting a key requirement for that bounded, self-serviceable tradeoff is the right call.

---

## Per-client routing matrix (completed)

| Client | Primary tier | Notes |
|---|---|---|
| Claude Desktop / Code / Cursor / Continue | L2-C | Host LLM translates; enrich tool descriptions in **both** generators |
| `travsr ask` (CLI), scripts, `curl` | L2-A → L2-B | No upstream model; L2-D structurally unreachable |
| Cloud SSE tier (RFC-007) | L2-A → L2-B | No upstream model; behaves like CLI |
| Future web dashboard | L2-A → L2-B | (+ L2-D only if a sampling-capable model is wired) |
| JetBrains plugin (future) | L2-A → L2-B | Non-host client |
| VS Code / standalone GUI **with** `sampling` | L2-D → L2-A/B | Near-vestigial today (extension sends `capabilities:{}`) |
| Any client, exact symbol known | `:` bypass → deterministic substring | §2.5 |

Multi-client consistency: every non-host client funnels through the identical deterministic `search_nodes_fuzzy`; only hosts (L2-C) and sampling-capable GUIs (L2-D) diverge — and they diverge toward *better* seeds over the same deterministic floor, never below it.

---

## Storage-tier portability (SQLite → Kùzu → RocksDB)

L2-A and L2-B are specified above against the **SQLite MVP tier** (`fts_vocab` table + FTS5 trigram; `sqlite-vec` for embeddings). CLAUDE.md mandates a storage progression as the graph grows (SQLite < 75M nodes → Kùzu < 2.5B edges → RocksDB hyperscale), so the gate will ask how the L2 ladder carries across engines. **The migration trigger is graph scale, not the query layer** — the ladder and the storage tier are orthogonal axes, and L2 does not "move" from one engine to the next. The portability answer is a clean separation of *concept* from *physical implementation*:

- **The vocabulary-grounding *concept* is engine-independent.** L2-A's core idea — map NL tokens onto the graph's *own closed token vocabulary* before reaching for any model — does not depend on SQLite. Only the physical home of the vocabulary set and the lexical match change per backend. This is the durable contract and must not be re-litigated per engine.
- **SQLite (MVP, shipped):** `fts_vocab` + FTS5 trigram `MATCH`; `sqlite-vec` for L2-B. Note `sqlite-vec` performs an **exhaustive (brute-force) vector scan** — there is no sub-linear ANN index in current releases — which is fine on the L1-miss-only path at MVP node counts, but means L2-B latency at this tier grows with node count. The RaBitQ Hamming path (§"Axis 3") keeps even this brute-force scan cheap (popcount, not FMA), deferring the latency wall well past MVP.
- **Kùzu (prod):** map the lexical layer onto Kùzu's **native FTS (BM25) extension** and L2-B onto Kùzu's **native vector index**; the vocabulary set becomes a derived token relation rather than a side table. RaBitQ binary vectors line up cleanly with a **`similarityFunction:'hamming'`** index (`CREATE VECTOR INDEX … OPTIONS {dimension:256, similarityFunction:'hamming'}`), and Kùzu's Node-Masker prunes by graph topology *before* the ANN compare. Exact DDL/Cypher is deferred to the Kùzu migration RFC and **must be taken from current Kùzu docs at implementation time** — several widely-circulated "Kùzu" FTS/vector snippets are actually Neo4j/FalkorDB syntax (`db.index.fulltext.createNodeIndex`) and will not run on Kùzu.
- **RocksDB (hyperscale):** neither FTS nor a vector index is native; both L2-A's inverted/vocabulary structures and L2-B's ANN must be built in the application tier. *Candidate* structures from the synthesis (not ratified design): an **FST (SymSpell delete-map) column family** for the lexical layer; RaBitQ binary payloads + Hamming rerank for vectors; and **Elias-Fano** encoding for the inverted-index postings lists (monotonic node-ID sequences), whose `O(1)` rank/select makes multi-term seed intersection a constant-time predecessor/successor jump across gigabytes of compressed postings instead of a linear decode (a candidate replacement for Roaring Bitmaps on these strictly-monotonic graph-ID lists). This is a substantial engineering burden, **out of scope until the RocksDB tier is itself roadmapped** (post-Phase-5); it is recorded here only so the L2 design is known to be portable, not orphaned.

**Invariant across all three engines:** the ladder ordering (L2-A grounded lexical → L2-B local vectors), the determinism floor, the no-key guarantee, and the "L2-B only on L2-A miss" gate. Engines change the *index implementation*, never the ladder semantics.

---

## Concrete edits to fold into RFC-012 on ratification

1. **§ Summary, bullet 2** — "L2 - key-free query understanding (deterministic vocabulary expansion default; local embeddings / host model for paraphrase)."
2. **§2.0** — **narrow** (do not claim to abolish) the exception: the default L2 path is algorithmic, so no exception is needed *for the default*; retain an explicit, ratified note that L2-C/L2-D remain host-side LLM tiers and that any *daemon-side* LLM or LLM-determined edges still need a new RFC.
3. **§2.1–§2.7** — supersede with L2-A/B/C/D above. Keep §2.5 (`:` bypass) and §2.2 caps verbatim. **Fix "256-byte" → "512-byte" in §2.7.**
4. **Migrations** — renumber: **L1 = v9 (shipped reality)**, **L2-A `fts_vocab` = v10**, **L3 embedding `vec0` = v11** (RFC-012 §3.5 currently says v9 — a collision with the shipped L1 table; it must move to v11).
5. **§3.x (L3)** — add a note that L3 also fulfils the L2 semantic-paraphrase role (L2-B); ladder mechanics unchanged, but the **storage representation is upgraded** from FP32 `vec0` to **MRL-truncated + RaBitQ 1-bit** (~40 B/node) with a portable Hamming compute path (AVX-512 / NEON / scalar). The `vec0` schema (v11) holds binary payloads + per-vector scalar correctors, not FP32 arrays. Recall vs FP32 is a measured ship gate (Open Q #14).
6. **§ Sprint Shape — S20 is MVP-only; the Rev-3 scale-out tiers are their own later sprints.** **S20 (unchanged scope, ~350–550 LOC, complexity L):** `expand_query` + stop-words + static lexicon + `fts_vocab` table + **refcount maintenance in `put_node_fts` and both bulk-delete paths** (flag the vocabulary cache as its own task) + the **linear `O(V·|q|)` scan** (no scale index yet), in `travsr-store`; rich tool descriptions in **both** `tools_list()`/`tools_list_global()` (L2-C); optional `prompts`/`sampling` (L2-C/L2-D) gated by security review; add **`:` bypass to `ask.rs`** — it does not exist in the CLI today (only proposed for VS Code), so "§2.5 bypass unchanged" is false for the CLI until added. **Explicitly deferred out of S20 (each its own sprint, behind its own ADR-001 amendment + feature flag):** **(S2x-a) L2-A scale-out** = SymSpell delete-map + `fst` build/serialize/mmap + refcount-driven FST patching (~400+ LOC, dep: `fst`); **(S2x-b) L2-B storage upgrade** = MRL truncation + RaBitQ (rotation/snap/correctors) + portable `count_ones` Hamming + recall harness (~500+ LOC, dep: RaBitQ helper) — and the optional `unsafe` AVX-512/NEON acceleration is a *further* sprint gated by its own principle-7 RFC + MSRV bump. The original "~350–550 LOC/L" applies to S20 **only**; piling SymSpell/FST/RaBitQ/MRL onto it would blow that estimate ~3×.
7. **§ Acceptance Criteria L2** — replace hosted-translator criteria with: (a) `expand_query` is deterministic (sorted output) and grounded (proposes only in-vocabulary tokens); (b) **name ≥5 NL queries and their expected top seed in `dogfooding.md`** (as L1's AC does — make it falsifiable); (c) `:` bypass added to `ask.rs` + VS Code; (d) lexicon CI charset assertion (`^[a-z0-9_]+$`) + additive-recall test (T11); (e) `fts_vocab` refcount stays correct across both bulk-delete paths (test); (f) **no regression** vs the existing FTS path (L2-A is additive Step 3); (g) **no new dependency and no key in any *shipped* artifact — including the cloud-SSE/Docker image, not just the local default build** — asserted by a `cargo tree` / feature-matrix CI check; `fst`, the named RaBitQ helper, and `ef_rs` are each gated behind an ADR-001 amendment + feature flag and excluded from every default-feature build; (h) L2-C `prompts` and L2-D `sampling` route through the security sub-gate.

**Belonging to the later L2-A-scaleout / L2-B sprints (NOT S20 — see fold-in §6):** (i) **L2-B determinism across architectures** — the MVP `u64::count_ones()` baseline gives byte-identical popcount on x86/aarch64/scalar, and the gate asserts **identical *ranked seed output*** across architectures (canonical corrector application), so the OCI/ARM tier is not broken; any `unsafe` AVX-512/NEON intrinsic acceleration is out of scope until its own principle-7 RFC + MSRV bump (Open Q #13); (j) **RaBitQ+MRL recall is measured, not assumed** — a recorded recall delta vs FP32 over the dogfooding set gates the embedding-storage upgrade, the chosen MRL dim is justified from that curve, and the RaBitQ rotation seed is pinned with a CI reproducibility assertion (Open Q #14, T13). These cannot pass in S20 (no MRL model artifact in-repo yet) and are gates on their own sprints.

---

## Open Questions (new / updated)

7. **Stop-word list scope.** Hard-coded minimal English set vs configurable in `travsr.toml`? Lean: hard-coded minimal (the grounding step already discards non-vocabulary words).
8. **`fts_vocab` maintenance choke points.** Confirmed: `put_node_fts` + `delete_nodes_for_path` + `delete_nodes_for_path_prefix` (the dead `delete_node_fts` is not a path). Decide whether to also revive/wire `delete_node_fts` for future single-node deletes.
9. **Lexicon governance.** CODEOWNERS-gated in-tree constant; per-language tables revisited only if telemetry (RFC-012 Open Q #4) shows synonym misses after L2-A + L2-B ship.
10. **Vocabulary-scan index at scale. — RESOLVED (Rev 3).** Linear `O(V·|q|)` ships at the self-index; the scale-out structure is **SymSpell pre-computation encoded into an mmap'd FST**, `O(1)` in vocabulary cardinality `V` (not a trigram index or BK-tree — both considered and rejected: trigram silently drops `<3`-char tokens; BK-tree is `O(log V)` not `O(1)` and heap-resident). The "`O(1)`" is in `V` only — cost is still governed by `|q|` and `d`. Dependency residue is owned by #13a (do not restate). Tunable: `d=1` (shipped) vs `d=2`. AC must not claim sub-ms beyond the self-index *for the linear MVP path*; the FST path may.
11. **L2-D consent UX.** How is the egress opt-in surfaced (one-time per workspace vs per-query)? Defer to the security sub-gate.
12. **Engine-specific L2-A/B implementation.** The vocabulary-grounding concept is engine-independent, but `fts_vocab`/FTS5 (SQLite), native FTS+vector index (Kùzu), and manual inverted-index+ANN (RocksDB) are three different implementations. Sequence: ship SQLite now; specify the Kùzu mapping in the Kùzu migration RFC; defer RocksDB until that tier is roadmapped. Exact engine APIs must be verified against current upstream docs, not secondary write-ups (see § Storage-tier portability).
13. **L2-B compute portability + `unsafe` gate (AVX-512 vs ARM64).** AVX-512 popcount is x86-only but CLAUDE.md mandates ARM64 for OCI and the cloud-SSE tier runs there. The **MVP answer is the safe `u64::count_ones()` baseline** (lowers to x86 `POPCNT` / ARM `CNT`), which alone satisfies the ARM tier. Any `core::arch` AVX-512 (`_mm512_popcnt_epi64`) / NEON (`vcntq_u8`) acceleration is `unsafe` (principle-7 RFC gate) **and** the AVX-512 intrinsic is not stable until Rust 1.89 (MSRV-1.75 violation) — so it is a separate, double-gated sprint, not part of L2-B's first landing. Determinism gate is identical *ranked seed output* across arches (FP correctors), not just popcount.
13a. **New-dependency approvals.** `fst` (L2-A scale-out), a **named** RaBitQ/quantization helper (L2-B), and `ef_rs`/Elias-Fano (RocksDB tier) are **not** in ADR-001's allowed set. Each needs an ADR-001 amendment + Tech Lead sign-off with the hard gate in § Supply-chain note (pinned version, `cargo-deny` green, license allowlisted, CVE/maintenance bar); none is in the MVP default build *or any shipped artifact incl. cloud-SSE/Docker*. List exact crates + versions at implementation time.
14. **RaBitQ + MRL recall validation, seed pinning, `vec0` binary support.** Treat the unbiased-estimator and >96% MRL-truncation claims as *hypotheses to verify on Travsr's own embeddings*, not givens. Gate ship on a measured recall delta (RaBitQ-256 vs FP32 baseline) over the dogfooding query set; pick the MRL truncation dim (256/128/64) from that curve. The RaBitQ rotation seed must be pinned+versioned with a CI reproducibility assertion (T13). Verify the pinned `sqlite-vec` release actually supports a binary/`bit` `vec0` column + per-row auxiliary f32 correctors before assuming the v11 schema.
15. **Sub-3-char vocabulary tokens.** `tokenize_identifier` drops tokens `< 3` chars (`fts_tokenize.rs:81`), so `db`/`id`/`tx`/`io` are absent from `fts_vocab` and SymSpell cannot recover them. Capturing them requires lowering the vocab-path threshold **and** re-assessing the BM25-noise/charset impact the `>= 3` filter controls — a separate change, not implied by SymSpell. Decide whether the recall gain justifies it.

---

## Review Record

**Rev 3** folds in the research synthesis *Advanced Retrieval Architecture for Deterministic Code Graph Navigation*, then hardens it against a three-track multi-agent review (Principal Architect · Principal Security Engineer · Tech Lead + Senior SWE, all reading the merged code). It sharpens two tiers and the hyperscale story without changing the ladder semantics:
- **L2-A scale-out** is now concrete — **SymSpell + mmap'd FST**, `O(1)` *in vocabulary size `V`* (not constant in `|q|`/`d`), resolving Open Q #10. Cost: a gated `fst` dependency (ADR-001 amendment required; MVP stays dep-clean). The paper's "SymSpell indexes the `<3`-char tokens trigram drops" claim was **corrected as inapplicable here** — `tokenize_identifier` already discards `<3`-char tokens upstream (Open Q #15).
- **L2-B footprint** drops from FP32 (~1.5 KB/node) to **MRL-truncated + RaBitQ 1-bit (~40 B/node)** with an unbiased-estimator distance, fitting the ADR-004 tarball budget. Compute is XOR+popcount; the MVP path is the **safe `u64::count_ones()` baseline** (satisfies ARM/OCI on its own). The RaBitQ rotation seed is **pinned** or the determinism floor breaks (BLOCKER fix).
- **Hyperscale** lexical→FST, vectors→RaBitQ-Hamming, postings→**Elias-Fano** (candidate, not ratified) on the RocksDB tier; Kùzu maps onto a native `hamming` vector index.

Review findings folded into Rev 3 (the four BLOCKERs in **bold**):
| Reviewer | Highest-value finding folded in |
|---|---|
| Principal Architect | **`unsafe` SIMD intrinsics fire CLAUDE.md principle 7** → baseline is safe `count_ones()`, SIMD double-gated; "L3-cache residency" over-claim struck; SymSpell "`O(1)`" qualified to *in `V`*; L2-B/Elias-Fano crate placement pinned (`store`/`retrieval`). |
| Principal Security Engineer | **Unpinned RaBitQ rotation seed breaks the determinism floor** → seed pinned+versioned; **gating exempted the cloud-SSE artifact** → AC (g) now covers *every shipped artifact*; new threat **T13** (FST/`vec0` artifact integrity); hard dep-vetting gate; FST grounding re-checked at build boundary. |
| Tech Lead + Senior SWE | **AVX-512 `_mm512_popcnt_epi64` violates MSRV 1.75** (stable only at 1.89) → intrinsic deferred; S20 re-scoped to MVP-only with scale-out split to own sprints (LOC estimate was ~3× off); Open-Questions renumbered monotonically; stale "trigram/BK" Cost row + AC (j) un-shippable-in-S20 fixed; `ef_rs` added to supply-chain note. All cited code line numbers re-verified accurate. |

New ship gates: cross-arch *ranked-output* determinism (AC i), measured RaBitQ/MRL recall vs FP32 + seed reproducibility (AC j, Open Q #14, T13), new-dep approvals (Open Q #13a).

Rev 2 incorporates a five-persona review of Rev 1 (all read the merged L1 code, not just the prose):

| Persona | Verdict | Highest-value finding folded in |
|---|---|---|
| Principal Architect | Ratify-with-changes | §2.0 can be *narrowed*, not *dissolved* (L2-C/D are still LLM tiers); this addendum removes the "reach for an LLM at the seed boundary" **precedent**, which is the manifesto made executable. |
| Principal Security Engineer | Ratify-with-changes | Deleting the key deletes the friction that implicitly gated egress; T11 (lexicon poisoning) + T12 (sampling egress/abuse) added; arg cap is 512 not 256. |
| Tech Lead | Ratify-with-changes | Migration collision (L1 shipped as **v9**, not v8 → L3 must move to v11); the 5-cap belongs on **final OR-arms**, not pre-expansion fragments. |
| Solution Architect | Ratify-with-changes | L2-C lever lives in **two** generators (`tools_list` + `tools_list_global`); L2-D is near-vestigial at ship and needs real capability negotiation. |
| Senior SWE | Buildable with changes | Vocab refcount must hook the **bulk-delete** paths (the `delete_node_fts` hook is `#[allow(dead_code)]`); deterministic sorted output; const-slice lexicon, no new deps (MSRV 1.75). |

One reviewer claim was **rejected after verification**: the Solution Architect flagged "RFC-004 → RFC-005" for the MCP-schema reference, but `docs/rfcs/RFC-004-mcp-tool-json-schema.md` is in fact the MCP tool-schema RFC (RFC-005 is cross-language edge resolution). The original RFC-004 citation stands.

---

## References

- RFC-012 — Fuzzy Seed Selection (this addendum revises its L2 section)
- RFC-012 §3.x — L3 embedding sidecar (re-roled as L2-B; `vec0` table renumbered v11)
- RFC-012 §A5 — synonym expansion (rejection re-scoped to NL-locale stemming, not the programmer-jargon table; not overturned)
- RFC-004 — MCP tool JSON schemas (verified: the MCP-schema RFC; L2-C description edits are Patch-level here)
- RFC-007 — MCP SSE transport (cloud-SSE routing row; metrics endpoint for telemetry)
- ADR-001 — coding standards / allowed dependencies (L2-A adds none)
- ADR-003 — PPR policy (50-seed bound, unchanged)
- ADR-004 — error taxonomy / tarball budget (unchanged; L2-A adds only KB)
- `crates/travsr-store/src/fts_tokenize.rs` — `tokenize_identifier`, `build_match_expr` (reused by L2-A)
- `crates/travsr-store/src/lib.rs:889` — `search_nodes_fuzzy` (L2-A is an additive Step 3); `:375`/`:433` bulk deletes (vocab refcount hooks); `:772` dead `delete_node_fts`
- `crates/travsr-mcp/src/server.rs:150,410` — `tools_list` / `tools_list_global` (L2-C edits both); `:63,330` advertised capabilities
- `crates/travsr-mcp/src/sanitize.rs:22` — `MAX_ARG_BYTES = 512`
- MCP `sampling/createMessage` — https://modelcontextprotocol.io (capability behind L2-D)
- *Advanced Retrieval Architecture for Deterministic Code Graph Navigation* (research synthesis, folded into Rev 3) — SymSpell, FST, MRL, RaBitQ, AVX-512, Elias-Fano
- SymSpell (Symmetric Delete) — Garbe; `fst` crate (BurntSushi) — mmap'd FST for the SymSpell delete-map (L2-A scale-out; **not yet in ADR-001**)
- Matryoshka Representation Learning (MRL) — runtime dimension truncation for `bge-small`/`mxbai-embed` (L2-B Axis 1)
- RaBitQ — *Quantizing High-Dimensional Vectors with a Theoretical Error Bound* (arXiv 2405.12497); 1-bit + scalar correctors (L2-B Axis 2)
- `hamming-bitwise-fast` / AVX-512 `VPOPCNTDQ`, ARM NEON `vcnt` — portable popcount Hamming compute (L2-B Axis 3; see Open Q #13)
- Elias-Fano (`ef_rs`) — quasi-succinct monotonic postings compression for the RocksDB hyperscale tier
