# RFC-012 Amendment A1 — Heuristic L2 Floor + Tiered LLM/Embeddings on L3

> **Amends:** `RFC-012-fuzzy-seed-selection.md` §2 (L2) and §3 (L3).
> **Status:** **Accepted — pending conditions.** Principal Architect + Tech Lead signed off
> 2026-06-02 with conditions C1–C5 (PA) and TL1–TL6 (TL), now folded into this revision (v2).
> **Supersedes:** `RFC-012-L2-plan.md` (the client-side LLM-translator implementation plan).
> **Issue:** #258 · Sprint S20 · **Author:** Abhishek · **Date:** 2026-06-02
> **Crate(s) affected:** `travsr-store` (floor), `travsr-cli` + `packages/travsr-vscode` (LLM translator, opt-in),
> `travsr-store --features embedding` (embeddings, opt-in).

---

## 1. Summary

This amendment makes two changes to the ratified fuzzy-seed stack:

1. **L2 is redefined from "LLM query translator" to a deterministic heuristic normalizer.**
   It merges into L1's query-side path (the **deterministic floor**): identifier tokenization +
   stopword/intent-word stripping + a bounded, curated synonym map. No LLM, no network, no API
   key, no secret storage. Runs in-daemon, everywhere, including old low-resource machines.

2. **All non-deterministic seed selection moves to L3 as a capability-tiered, opt-in layer.**
   L3 hosts both **embedding retrieval** (server-side, feature-flagged) and an **optional LLM
   translator** (client-side: local model or bring-your-own cloud key). Every L3 mechanism is
   opt-in, gated on machine capability, and only ever **selects PPR seed nodes** — never curates,
   reranks, or defines context.

**Net effect:** the LLM leaves the default path entirely, *removing* the "owned exception to
algorithms-first, LLM-last" that RFC-012 §2.0 had to ratify. The amended stack is manifesto-compliant
by construction.

---

## 2. Motivation

- **The §2.0 exception is avoidable.** We get the same UX without putting an LLM in the default path.
- **L1's trigram tokenizer already does most of L2's job.** The canonical NL queries resolve via
  L1 alone, zero LLM (`crates/travsr-store/tests/fuzzy.rs`). The only genuine LLM-only capability is
  **open-vocabulary synonymy** — exactly L3's mandate.
- **The cloud-LLM L2 has a UX hole.** Keyless users silently got only L1. The heuristic floor gives
  *every* user better-than-L1 NL search with zero setup.
- **Local LLMs don't run everywhere.** A ~1–3B model needs ~2–4 GB free RAM; older machines can't.
  A load-bearing LLM excludes those users; a deterministic floor + opt-in tiers excludes no one from
  the baseline.
- **Determinism.** RFC-012 §2.3 concedes `temperature=0` is not reproducible. The floor is genuinely
  deterministic.

### Measured justification

Graph-gated structural discovery vs a grep/read agent on this repo
(`docs/benchmarks/token_savings.py`, tiktoken cl100k_base):

| Baseline | Aggregate token reduction |
|---|---|
| vs surgical grep-agent (±25 lines around hits) | **~86%** |
| vs whole-file reads | ~97% |

Honest figure: **~86%** for the discovery phase (blended over a full edit task ~60–80%). This is a
**per-query** figure realized *once a query lands on the graph path*; see §10 (Drawbacks) for the
coverage/hit-rate distinction. The heuristic floor is the free, deterministic on-ramp (~0 LLM tokens
vs ~280/query for the cloud translator) that keeps queries resolving so this saving materializes.

---

## 3. Amended layer model

| Layer | Before (RFC-012) | After (this amendment) |
|---|---|---|
| **L1** | Lexical FTS5 (exact + trigram) | **Unchanged** |
| **L2** | LLM query translator (client-side, default-on) | **Heuristic normalizer** — stopwords + stemming + synonym map; deterministic, in-daemon. Merges with L1 as the universal floor. **No LLM.** |
| **L3** | Embedding sidecar (`--features embedding`) | **Tiered semantic layer** — embeddings (server-side, feature-flagged) *and* optional LLM translator (client-side, local / cloud-key). All opt-in, capability-gated, seed-selection only. |

Ordering invariant (RFC-012 §0, preserved): **lexical → deterministic-heuristic → semantic.**

---

## 4. §2 redefined — Heuristic L2 (the deterministic floor)

### 4.1 What it does

A deterministic query-normalization step inside `search_nodes_fuzzy`. It is an enhancement of L1's
query-side tokenization, not a new transport layer — hence "the floor."

**Pipeline ordering [TL3 / PA C1 — exact stays on the raw literal; expansion feeds only the FTS step]:**

```
search_nodes_fuzzy(raw_query):
  STEP 1 — exact LIKE on the RAW literal query        (UNCHANGED from L1; never sees expansion)
  if hit -> return                                    (single-token exact queries never regress)

  STEP 2 — FTS5 MATCH on the normalized token set:
     fragments = tokenize_identifier(raw_query)        (reuse crates/travsr-store/src/fts_tokenize.rs)
     fragments -= stopwords / intent words             (static list)
     fragments += synonym/alias expansion              (bounded curated map)
     fragments += raw tokens                           (PA C1: union ALWAYS includes the literal tokens)
     fragments = fts_escape(fragments)                 (TL4: reuse L1's MATCH-injection escaper; no bypass)
     MATCH fragments -> seed nodes
```

- **PA C1 (additive union seeding):** expansion is *strictly additive*. The seed set is the union
  that **always includes the raw literal tokens**; a synonym may only *add* a candidate seed, never
  suppress or replace the literal. Worst case of a bad synonym = one noise seed, never a wrong seed.
- **TL4 (escaper parity):** synonym/stem output passes through the *same* double-quote/OR FTS5
  escaper as L1 (`fts_tokenize.rs`); it must not reach `MATCH` unescaped.

### 4.2 Placement & dependency rules

- Lives in `travsr-store` (alongside `fts_tokenize.rs`), `travsr-core`-only dependency. **Zero new
  crates, zero LLM deps, zero network.** `travsr-mcp` stays LLM-free — trivially.
- Stopword + synonym data is a static, versioned, auditable in-repo file
  (`crates/travsr-store/src/seed_lexicon.rs`). Curated, not learned.

### 4.3 Properties

Universal (≈zero RAM/CPU, runs on old machines) · deterministic & reproducible · zero setup / cost /
latency · strictly ≥ L1 (exact step preserved; expansion only adds candidates).

### 4.4 `:` bypass (preserved)

`:`-prefixed queries skip normalization and pass the literal straight through, in both the VS Code
graph panel and `travsr ask`.

---

## 5. §3 redefined — Tiered semantic layer

L3 is the **only** home for non-deterministic seed selection. Three tiers, auto-selected by the
client from detected machine capability; each strictly opt-in; all degrade to the §4 floor.

| Tier | Mechanism | Where it runs | Requires | Adds over floor |
|---|---|---|---|---|
| **T0 (floor)** | L1 + heuristic L2 | in-daemon | nothing | — (universal baseline) |
| **T1-emb** | embedding retrieval | **server-side**, `travsr-store --features embedding` | opt-in model download | open-vocab seed recall by vector distance |
| **T1-llm** | local small LLM translator | **client-side** (`travsr-cli` feat-flag / vscode) | ~2–4 GB free RAM | open-vocab translation, on-device, private |
| **T2-llm** | cloud LLM translator | **client-side** | user-supplied API key | open-vocab translation, no local compute |

### 5.1 The bright line — enforced by contract + test [PA C4]

Every L3 mechanism **only selects PPR seed nodes.** It reads the query and emits symbol fragments
(LLM) or returns ≤N nodes by vector distance (embeddings). It **never** reranks, summarizes, curates,
or *defines* the returned context. The deterministic PPR/knapsack/PCST pipeline defines context from
the seeds, exactly as today.

**Enforcement is structural, not prose:** the daemon tool surface accepts only structured seed args
(`symbols`/`paths`/`kinds`) and never accepts "here is context, rerank it." A **blocking** test
(§8 #6/#7) asserts no free-text context or ranked-node list crosses the daemon boundary in either
direction.

> Rationale: the host AI consuming Travsr *is already an LLM*. Travsr hands it precise,
> structurally-correct, deterministic context — it does not do LLM work on the agent's behalf. An LLM
> that curated context would rebuild vector-RAG's "guess relevance" failure mode on top of the graph.
> Permitted: *query in, fragments out.* Forbidden: *context in, reranked context out.*

### 5.2 LLM translator contract (T1-llm / T2-llm)

Identical to RFC-012 §2.2 (`StructuredQuery`), same caps (≤5 symbols, ≤3 paths, ≤3 kinds), same prompt
(RFC-012 §2.3), same fallback floor (RFC-012 §2.6). Only **placement and tiering** change, not the
contract.

### 5.3 Embeddings — bounded to seed recall [PA C2] and separable [PA C3]

- **PA C2:** vector search may return **only ≤N seed nodes** that enter the *unchanged* PPR pipeline.
  Embedding distance must **never** weight graph edges, traversal order, or knapsack scoring. (This is
  the hard rule that keeps RFC-012 Open Problem #3 from becoming a manifesto violation.)
- **PA C3:** the vector index (**migration v10** — v9 is taken by the FTS table) is an opt-in,
  independently-rebuildable artifact that does **not** alter the canonical multiplex graph. Invariant
  #4 (full == incremental) is asserted on the **canonical graph**, not the vector index. The vectors
  are also accounted separately and do **not** count against the L1 FTS index-size budget (< 5%).

### 5.4 Packaging & dependency rules [TL1 / TL2]

- **TL1 — placement is split, not lumped:**
  - **LLM translator** (T1-llm, T2-llm) → **client-side**: `travsr-cli` (feature-flagged) + VS Code
    extension. Produces fragments *before* any MCP call.
  - **Embedding retrieval** (T1-emb) → **server-side** in `travsr-store` behind `--features embedding`
    (RFC-012 §3); it returns seeds *into* the daemon's PPR pipeline, so under the flag it is
    legitimately transitive into `travsr-mcp` via `travsr-retrieval → travsr-store`.
- **TL2 — corrected CI gate** (the old "no LLM/embedding deps in `travsr-mcp`" is false under the
  feature flag):
  - `travsr-mcp` has **zero LLM deps in *all* configs** —
    `cargo tree -p travsr-mcp | grep -iE 'anthropic|openai|llama|llm' && exit 1 || echo OK`
  - `travsr-mcp` has **zero embedding deps in the *default* config** —
    `cargo tree -p travsr-mcp | grep -iE 'onnx|sqlite-vec|tokenizers' && exit 1 || echo OK`
- Opt-in models (LLM or embedding) ship like the existing `embedding` feature: separate download,
  **not** in the default 25 MB tarball (ADR-004).

### 5.5 Capability auto-detection

The client probes free RAM / CPU at startup and selects the highest available tier: T1 (local model
present + resources suffice) → else T2 (cloud key configured) → else T0. No manual config, no nag-loop,
never below T0.

### 5.6 Determinism guarantee — scoped [PA C5]

The guarantee is **"identical seed set → identical context."** It is **not** "identical NL query →
identical context across tiers": a T0 machine and a T1 machine may produce *different seeds*, each
deterministic from its seeds onward. **L1 is the reproducibility floor.** Local inference (T1) on CPU
can be *slower* than a cloud round-trip (T2) — local is a privacy/no-key tradeoff, not a speed win.

---

## 6. Scope & sprint plan [TL5]

**S20 ships only T0 — the deterministic floor.** Estimate **5 pts (M–L)**: normalizer + lexicon +
union seeding + escaper reuse + tests. Reuses `tokenize_identifier`; contained, in-store.

The semantic tiers are **separate follow-on PRs in Phase 5**, matching RFC-012's existing L3 deferral:

| Work item | Sprint | Est. |
|---|---|---|
| T0 heuristic floor | **S20 (this PR)** | 5 pts |
| T1-emb embedding retrieval (`--features embedding`) | Phase 5 | L–XL |
| T1-llm local LLM translator | Phase 5 | L–XL |
| T2-llm cloud-key translator (relocated from RFC-012-L2-plan) | Phase 5 | L |

---

## 7. Security (Principal Security Engineer lens)

- Prompt injection cannot reach the daemon: only `validateMcpArg`-clean fragments are forwarded; raw
  NL and `echo` never leave the client. Worst case → empty → T0.
- No central attack surface / no data egress: T0/T1 are fully local; the daemon never calls an LLM.
  Centrally-hosted inference is explicitly rejected (off-machine queries, single-box SPOF on free-tier).
- Secret hygiene (T2 only): API key in SecretStorage, never in settings/telemetry/logs.
- Bright line enforced by contract + blocking test (§5.1, §8 #6/#7).

---

## 8. Acceptance criteria

- [ ] Heuristic L2 normalizer in `travsr-store`, in-daemon, zero LLM deps; reuses `tokenize_identifier`.
- [ ] Static, versioned stopword + synonym lexicon (`seed_lexicon.rs`); documented and auditable.
- [ ] **[PA C1]** Union-seeding test: synonym expansion is additive — the raw literal token is always
      in the seed set; a synonym never suppresses the literal seed.
- [ ] **[TL3]** Ordering test: exact `LIKE` runs on the raw literal (unaffected by expansion); FTS step
      receives the normalized union.
- [ ] **[TL4]** Escaper-parity test: synonym/stem fragments pass through the L1 FTS5 MATCH escaper.
- [ ] Floor (T0) resolves the ≥5 canonical NL queries with no model present (parity with L1 benchmark).
- [ ] `:` bypass preserved (VS Code + `travsr ask`).
- [ ] **[PA C4 — blocking]** #6: daemon args contain only `symbols`/`paths`/`kinds` — no free text.
      #7: no ranked-node list or rewritten context crosses the daemon boundary in either direction.
- [ ] **[PA C3 / TL6]** Full == incremental on the **canonical** graph (vector index excluded).
- [ ] **[TL1/TL2]** `travsr-mcp`: zero LLM deps in all configs; zero embedding deps in default config
      (CI `cargo tree` gates per §5.4).
- [ ] `docs/benchmarks/token_savings.py` retained; ≥5 NL queries documented end-to-end in `dogfooding.md`.

---

## 9. Alternatives considered

- **Keep cloud-LLM L2 default-on (RFC-012-L2-plan).** Rejected: keeps the §2.0 manifesto exception,
  excludes keyless users, non-reproducible, ~280 tokens + a remote round-trip per query, large
  client surface (provider abstraction, SecretStorage, prompt-injection suite). Relocated to T2 (opt-in).
- **Embeddings-only semantic layer (skip the heuristic floor).** Rejected: embeddings don't run on old
  machines (model size), aren't deterministic, and leave keyless/low-resource users at bare L1. The
  floor is the universal baseline that everything else sits on top of.
- **Lazy LLM-on-miss without a heuristic floor.** Rejected: still needs an LLM to be present to beat
  L1, so it doesn't help low-resource/keyless users; the heuristic floor closes most of that gap for free.
- **Centrally-hosted free LLM endpoint.** Rejected: violates local-first (query egress), single
  free-tier box can't serve concurrent inference (SPOF/bottleneck), wrong LLM placement (§7).

## 10. Drawbacks

- **Open-vocabulary recall is deferred to opt-in tiers.** T0 is bounded by the curated synonym map +
  stopword list; a phrase with no lexical overlap and no mapped synonym misses until T1/T2.
- **Coverage, not magnitude, is phased.** The ~86% per-query saving is unchanged, but in S20 it is
  realized only on **floor-resolvable** queries; open-vocab queries get 0% until Phase 5. Blended
  saving ≈ 86% × floor-hit-rate until the tiers ship.
- **Synonym-map maintenance.** Someone owns curation; it can inject noise seeds (bounded by PA C1).
- **Cross-tier inconsistency.** Different machines may seed differently (scoped by PA C5).

---

## 11. Migration from RFC-012-L2-plan.md

The old plan's LLM-translator code (`queryTranslator.ts`, `translator.js`, provider abstraction,
SecretStorage, `setApiKey`, prompt-injection suite, `validateMcpArg` mirror) is **relocated to T2**
(bring-your-own-key) and demoted from default-on to opt-in — not deleted. The heuristic floor is net
new. The graph-panel single-call consumption and the npm reference fan-out are unchanged in contract;
they call the floor first and escalate to a tier only on miss.

## 12. Open questions

1. **Synonym lexicon ownership** — who curates it; per-language variants?
2. **Score-gated escalation** — fire T1/T2 on L1 *empty* only, or also on low FTS5 score? (Needs the
   daemon to surface a relevance score from `get_graph_json` — additive, still LLM-free.)
3. **Local model choice for T1-llm** — target model/quantization + minimum-resource auto-enable threshold.

## 13. Out of scope

- Server-side / centrally-hosted LLM (rejected — §7).
- LLM-driven context reranking or summarization (forbidden — §5.1).
- Multilingual stemming, learned synonym dictionaries (RFC-012 Non-Goals, unchanged).

---

## 14. Sign-off ledger

| ID | Condition | Folded into |
|---|---|---|
| PA C1 | Additive union seeding (literal always in seed set) | §4.1, §8 |
| PA C2 | Embeddings bounded to seed recall; never weight edges/traversal/knapsack | §5.3 |
| PA C3 | Embedding index separable (v10); full==incremental on canonical graph | §5.3, §8 |
| PA C4 | Bright line enforced by contract + blocking test | §5.1, §7, §8 |
| PA C5 | Determinism scoped to "identical seeds → identical context" | §5.6 |
| TL1 | Split placement: LLM client-side / embeddings server-side feature-flagged | §5, §5.4 |
| TL2 | Corrected `cargo tree` gate (LLM all-configs; embedding default-config) | §5.4, §8 |
| TL3 | Normalizer ordering: exact on raw, expansion feeds FTS only | §4.1, §8 |
| TL4 | Expanded fragments reuse L1 FTS5 escaper | §4.1, §8 |
| TL5 | S20 scoped to T0 floor (5 pts); tiers → Phase 5 | §6 |
| TL6 | Test additions; bright-line tests blocking | §8 |

**Sign-off:** Principal Architect ✅ (conditions C1–C5) · Tech Lead ✅ (conditions TL1–TL6) — 2026-06-02.
Status moves to **Accepted** once §8 acceptance criteria land in the S20 PR.
