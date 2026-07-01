/**
 * Unit tests for src/context/parse.ts.
 *
 * Fixtures mirror the exact format emitted by tools.rs (QUERY_PROTOCOL_VERSION).
 * Test any change to the backend format here first before touching the parser.
 */

import * as assert from "assert";
import {
  parseContextResult,
  isParsed,
  parseSnippetsResult,
  type ParsedContext,
} from "../../context/parse";

// ── Fixtures ─────────────────────────────────────────────────────────────────

const STRONG_FIXTURE = `[index commit: abc1234f, embeddings: on]
[retrieval: embed | coverage 3/3 | confidence: strong ]

PaymentService (class) — src/payment/service.ts:42 [package: @acme/billing] [via: seed] [score: 0.95]
───
class PaymentService {
  constructor(private readonly repo: PaymentRepo) {}
}

processPayment (function) — src/payment/service.ts:58 [via: caller] [score: 0.72]

IPaymentRepo (interface) — src/payment/repo.ts:10 [package: @acme/billing] [via: dependency] [score: 0.61]

[3 nodes, 1 with snippets, ~180 metadata-tokens + ~60 snippet-tokens (2000 budget)]
`;

const ABSTAIN_FIXTURE = `[index commit: def5678a, embeddings: off]
[retrieval: fts | coverage 0/3 | confidence: none ]

[0 nodes, 0 with snippets, ~0 metadata-tokens + ~0 snippet-tokens (2000 budget)]
[note: no strong match found — results are weak/structural and may not be relevant to this query]
`;

const DEGRADED_FIXTURE = `[index commit: abc1234f, embeddings: off]
[retrieval: fts | coverage 2/3 | confidence: moderate ]

foo (function) — src/foo.ts:10 [via: seed] [score: 0.80]

[1 nodes, 0 with snippets, ~40 metadata-tokens + ~0 snippet-tokens (2000 budget)]
[note: semantic search disabled — run \`travsr embed init\` for better results]
[note: call traversal limited — run \`travsr lang install <lang>\` to enable call-graph edges]
`;

const NO_LINE_FIXTURE = `[index commit: aaabbb11, embeddings: on]
[retrieval: embed | coverage 1/1 | confidence: strong ]

bar (function) — src/bar.ts [via: seed] [score: 0.90]

[1 nodes, 0 with snippets, ~30 metadata-tokens + ~0 snippet-tokens (2000 budget)]
`;

// Metadata-only footer (include_snippets=false or no repo_root)
const SIMPLE_FOOTER_FIXTURE = `[index commit: 36acbbcb753, embeddings: off]
[retrieval: exact+lexical | coverage 1/1 | confidence: strong ]

fn:isSyncPodWorthy (function) — pkg/kubelet/kubelet.go:3462 [via: seed] [score: 0.03]
method:Kubelet.syncPodNow (method) — pkg/kubelet/kubelet.go:3545 [via: seed] [score: 0.03]

[24 nodes, ~748 tokens]
`;

// No-repo-root footer (init not run yet)
const INIT_NEEDED_FIXTURE = `[index commit: 36acbbcb753, embeddings: off]
[retrieval: exact+lexical | coverage 1/1 | confidence: strong ]

fn:isSyncPodWorthy (function) — pkg/kubelet/kubelet.go:3462 [via: seed] [score: 0.03]

[2 nodes, ~42 tokens — run \`travsr init\` to enable inline snippets]
`;

// Phase 1 done (embed.db exists) but KNN hook not yet active — "in progress" note
const EMBED_PROGRESS_FIXTURE = `[index commit: abc1234f, embeddings: off]
[retrieval: fts | coverage 2/3 | confidence: moderate ]

foo (function) — src/foo.ts:10 [via: seed] [score: 0.80]

[1 nodes, 0 with snippets, ~40 metadata-tokens + ~0 snippet-tokens (2000 budget)]
[note: embedding in progress — run \`travsr embed status\` to check; results improve as index builds]
`;

const MALFORMED_FIXTURE = `not a valid get_context response at all`;

// ── Tests ─────────────────────────────────────────────────────────────────────

suite("context/parse: parseContextResult", () => {
  test("parses a strong-match result with snippet", () => {
    const r = parseContextResult(STRONG_FIXTURE);
    assert.ok(isParsed(r), "should be ParsedContext");
    const p = r as ParsedContext;

    assert.strictEqual(p.header.indexCommit, "abc1234f");
    assert.strictEqual(p.header.embeddings, "on");
    assert.strictEqual(p.header.tier, "embed");
    assert.deepStrictEqual(p.header.coverage, [3, 3]);
    assert.strictEqual(p.header.confidence, "strong");

    assert.strictEqual(p.nodes.length, 3);
    const [n0, n1, n2] = p.nodes;

    assert.strictEqual(n0.sig, "PaymentService");
    assert.strictEqual(n0.kind, "class");
    assert.strictEqual(n0.path, "src/payment/service.ts");
    assert.strictEqual(n0.line, 42);
    assert.strictEqual(n0.pkg, "@acme/billing");
    assert.strictEqual(n0.via, "seed");
    assert.ok(Math.abs((n0.score ?? 0) - 0.95) < 0.001);
    assert.ok(n0.snippet?.includes("PaymentService"), "snippet should be present");

    assert.strictEqual(n1.sig, "processPayment");
    assert.strictEqual(n1.kind, "function");
    assert.strictEqual(n1.via, "caller");
    assert.strictEqual(n1.pkg, undefined);

    assert.strictEqual(n2.via, "dependency");
    assert.strictEqual(n2.pkg, "@acme/billing");

    assert.strictEqual(p.footer.nodes, 3);
    assert.strictEqual(p.footer.withSnippets, 1);
    assert.strictEqual(p.footer.metaTokens, 180);
    assert.strictEqual(p.footer.snippetTokens, 60);
    assert.strictEqual(p.footer.budget, "2000");
    assert.strictEqual(p.notes.length, 0);
  });

  test("parses an abstain result (zero nodes, has note)", () => {
    const r = parseContextResult(ABSTAIN_FIXTURE);
    assert.ok(isParsed(r));
    const p = r as ParsedContext;
    assert.strictEqual(p.header.confidence, "none");
    assert.strictEqual(p.nodes.length, 0);
    assert.strictEqual(p.notes.length, 1);
    assert.ok(p.notes[0].includes("no strong match"));
  });

  test("parses a degraded result with two notes", () => {
    const r = parseContextResult(DEGRADED_FIXTURE);
    assert.ok(isParsed(r));
    const p = r as ParsedContext;
    assert.strictEqual(p.header.embeddings, "off");
    assert.strictEqual(p.nodes.length, 1);
    assert.strictEqual(p.notes.length, 2);
    assert.ok(p.notes[0].includes("semantic search disabled"));
    assert.ok(p.notes[1].includes("call traversal limited"));
  });

  test("parses a node with no line number", () => {
    const r = parseContextResult(NO_LINE_FIXTURE);
    assert.ok(isParsed(r));
    const p = r as ParsedContext;
    assert.strictEqual(p.nodes[0].line, undefined);
    assert.strictEqual(p.nodes[0].path, "src/bar.ts");
  });

  test("parses metadata-only footer [N nodes, ~T tokens]", () => {
    const r = parseContextResult(SIMPLE_FOOTER_FIXTURE);
    assert.ok(isParsed(r), "should parse with simple footer");
    const p = r as ParsedContext;
    assert.strictEqual(p.header.confidence, "strong");
    assert.strictEqual(p.nodes.length, 2);
    assert.strictEqual(p.footer.nodes, 24);
    assert.strictEqual(p.footer.withSnippets, 0);
    assert.strictEqual(p.footer.metaTokens, 748);
    assert.strictEqual(p.footer.snippetTokens, 0);
  });

  test("parses no-repo-root footer [N nodes, ~T tokens — run travsr init …]", () => {
    const r = parseContextResult(INIT_NEEDED_FIXTURE);
    assert.ok(isParsed(r), "should parse with init-needed footer");
    const p = r as ParsedContext;
    assert.strictEqual(p.footer.nodes, 2);
    assert.strictEqual(p.footer.withSnippets, 0);
  });

  test("parses embedding-in-progress note (Phase 1 done, hook not active)", () => {
    const r = parseContextResult(EMBED_PROGRESS_FIXTURE);
    assert.ok(isParsed(r));
    const p = r as ParsedContext;
    assert.strictEqual(p.notes.length, 1);
    assert.ok(p.notes[0].includes("embedding in progress"));
    assert.ok(p.notes[0].includes("travsr embed status"));
  });

  test("falls back to { raw } on malformed input", () => {
    const r = parseContextResult(MALFORMED_FIXTURE);
    assert.ok(!isParsed(r));
    assert.ok("raw" in r);
    assert.strictEqual((r as { raw: string }).raw, MALFORMED_FIXTURE);
  });

  test("falls back to { raw } on empty string", () => {
    const r = parseContextResult("");
    assert.ok(!isParsed(r));
  });
});

// ── parseSnippetsResult ───────────────────────────────────────────────────────

// Note: syncPodNow body has an internal blank line — the parser must NOT split here.
const SNIPPETS_FIXTURE = `fn:isSyncPodWorthy (function) — pkg/kubelet/kubelet.go [package: ]
───
func isSyncPodWorthy(pod *v1.Pod) bool {
\treturn pod.Spec.ActiveDeadlineSeconds != nil
}

method:Kubelet.syncPodNow (method) — pkg/kubelet/kubelet.go [package: ]
───
func (kl *Kubelet) syncPodNow(ctx context.Context, pod *v1.Pod) {
\tkl.probeManager.UpdatePodStartupReadinessIfNeeded(pod)

\tif err := kl.syncPod(ctx, pod); err != nil {
\t\treturn false, err
\t}
}

[2 symbols, 2 with snippets, ~120 tokens]
`;

suite("context/parse: parseSnippetsResult", () => {
  test("parses two snippets keyed by sig, preserving internal blank lines", () => {
    const m = parseSnippetsResult(SNIPPETS_FIXTURE);
    assert.strictEqual(m.size, 2);
    assert.ok(m.has("fn:isSyncPodWorthy"));
    assert.ok(m.get("fn:isSyncPodWorthy")?.includes("ActiveDeadlineSeconds"));
    assert.ok(m.has("method:Kubelet.syncPodNow"));
    const snip2 = m.get("method:Kubelet.syncPodNow") ?? "";
    assert.ok(snip2.includes("probeManager"), "should contain body before blank line");
    assert.ok(snip2.includes("syncPod"), "should contain body after internal blank line");
  });

  test("skips footer lines that start with [", () => {
    const m = parseSnippetsResult(SNIPPETS_FIXTURE);
    assert.ok(!m.has("[2 symbols"));
  });

  test("returns empty map for empty string", () => {
    const m = parseSnippetsResult("");
    assert.strictEqual(m.size, 0);
  });

  // Regression: skeleton bodies open with "[skeleton: …]" and code lines can
  // start with "[" — the body collector must not treat those as the footer.
  test("keeps a skeleton body that opens with [skeleton: …]", () => {
    const fixture = `fn:kind_boost (function) — crates/travsr-mcp/src/tools.rs [package: ]
───
[skeleton: function]
pub(crate) fn kind_boost(kind: &str, language: &str) -> f32
  params: kind: &str, language: &str
  returns: f32

[1 symbols, 1 with snippets, ~80 tokens]
`;
    const m = parseSnippetsResult(fixture);
    assert.ok(m.has("fn:kind_boost"), "skeleton snippet must be captured");
    const body = m.get("fn:kind_boost") ?? "";
    assert.ok(body.startsWith("[skeleton: function]"), "body must retain the skeleton header line");
    assert.ok(body.includes("returns: f32"), "body must retain later lines past the opening [");
    assert.ok(!body.includes("1 symbols"), "footer must not be included");
  });

  test("keeps a code line that starts with [ (array literal)", () => {
    const fixture = `fn:makeList (function) — a.ts [package: ]
───
function makeList() {
  return [
    1, 2, 3,
  ];
}

[1 symbols, 1 with snippets, ~40 tokens]
`;
    const m = parseSnippetsResult(fixture);
    const body = m.get("fn:makeList") ?? "";
    assert.ok(body.includes("1, 2, 3"), "array body must survive a line starting with [");
  });
});
