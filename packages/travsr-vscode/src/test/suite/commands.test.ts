import * as assert from "assert";
import {
  stripEnvelope,
  stripLangTokens,
  kindCodicon,
  parseGraphSymbols,
  parseSynonymList,
  parseExecutionPath,
  parseReposList,
  parseLanguageCounts,
  parseAvailableLanguages,
  buildStatsView,
  buildClickableFileListHtml,
  buildDepListHtml,
  resolveDepSpec,
} from "../../commands";

suite("VSCODE-247: stripEnvelope", () => {
  test("strips a populated travsr-data envelope", () => {
    assert.strictEqual(stripEnvelope("<travsr-data>\nsrc/a.ts\n</travsr-data>"), "src/a.ts");
  });
  test("empty envelope yields empty string", () => {
    assert.strictEqual(stripEnvelope("<travsr-data></travsr-data>"), "");
  });
  test("passes through non-enveloped text", () => {
    assert.strictEqual(stripEnvelope("nodes: 1\nedges: 0"), "nodes: 1\nedges: 0");
  });
});

suite("VSCODE-247: parseGraphSymbols (askSymbol)", () => {
  test("maps symbol nodes to items with path and line, drops file nodes", () => {
    const raw = JSON.stringify({
      nodes: [
        { id: "fn:bar", label: "bar", kind: "function", path: "src/foo.ts", package: "", score: 0.42, line: 12 },
        { id: "file", label: "foo.ts", kind: "file", path: "src/foo.ts", package: "", score: 0.1 },
      ],
      edges: [],
    });
    const items = parseGraphSymbols(raw);
    assert.strictEqual(items.length, 1, "file node must be filtered out");
    assert.strictEqual(items[0].path, "src/foo.ts");
    assert.strictEqual(items[0].line, 12);
    assert.ok(items[0].label.includes("symbol-method"), "function → symbol-method codicon");
    assert.ok(items[0].detail?.includes("0.420"));
  });
  test("malformed JSON returns empty array (no throw)", () => {
    assert.deepStrictEqual(parseGraphSymbols("not json{{"), []);
    assert.deepStrictEqual(parseGraphSymbols(""), []);
  });
});

suite("VSCODE-247: kindCodicon", () => {
  test("known kinds map to codicons, unknown falls back", () => {
    assert.strictEqual(kindCodicon("class"), "symbol-class");
    assert.strictEqual(kindCodicon("function"), "symbol-method");
    assert.strictEqual(kindCodicon("whatever"), "symbol-misc");
  });
});

suite("VSCODE-247: parseSynonymList", () => {
  test("splits `term => alias` lines, ignoring malformed rows", () => {
    const pairs = parseSynonymList("payment => charge\nauth => login\ngarbage line");
    assert.strictEqual(pairs.length, 2);
    assert.deepStrictEqual(pairs[0], { term: "payment", alias: "charge" });
    assert.deepStrictEqual(pairs[1], { term: "auth", alias: "login" });
  });
  test("handles enveloped output", () => {
    const pairs = parseSynonymList("<travsr-data>\ndb => database\n</travsr-data>");
    assert.deepStrictEqual(pairs, [{ term: "db", alias: "database" }]);
  });
});

suite("VSCODE-247: parseExecutionPath", () => {
  test("builds chained synthetic graph from prose lines", () => {
    const raw = "fn:a (function) — src/a.ts\nfn:b (function) — src/b.ts\nfn:c (function) — src/c.ts";
    const data = parseExecutionPath(raw);
    assert.strictEqual(data.nodes.length, 3);
    assert.strictEqual(data.edges.length, 2, "n nodes → n-1 chained edges");
    assert.strictEqual(data.nodes[0].id, "fn:a");
    assert.strictEqual(data.nodes[0].path, "src/a.ts");
    assert.strictEqual(data.nodes[0].root, true, "path nodes flagged root for highlight");
    assert.strictEqual(data.edges[0].source, "fn:a");
    assert.strictEqual(data.edges[0].target, "fn:b");
  });
  test("tolerates lines without the `(kind), path` shape", () => {
    const data = parseExecutionPath("just-a-signature");
    assert.strictEqual(data.nodes.length, 1);
    assert.strictEqual(data.nodes[0].id, "just-a-signature");
    assert.strictEqual(data.edges.length, 0);
  });
  test("empty input yields empty graph", () => {
    const data = parseExecutionPath("<travsr-data></travsr-data>");
    assert.strictEqual(data.nodes.length, 0);
  });
});

suite("VSCODE-247: parseReposList", () => {
  test("parses TSV name/path/exists into rows", () => {
    const raw = "live\t/a/.travsr/graph.db\t1\ndead\t/tmp/x/.travsr/graph.db\t0";
    const rows = parseReposList(raw);
    assert.strictEqual(rows.length, 2);
    assert.deepStrictEqual(rows[0], { name: "live", path: "/a/.travsr/graph.db", exists: true });
    assert.strictEqual(rows[1].exists, false);
  });
  test("empty / enveloped input", () => {
    assert.deepStrictEqual(parseReposList("<travsr-data></travsr-data>"), []);
    assert.deepStrictEqual(parseReposList(""), []);
  });
});

suite("VSCODE-247: buildStatsView", () => {
  test("extracts nodes/edges/schema_version fields", () => {
    const view = buildStatsView("nodes: 3623\nedges: 3691\nschema_version: 11");
    assert.strictEqual(view.nodes, "3623");
    assert.strictEqual(view.edges, "3691");
    assert.strictEqual(view.schemaVersion, "11");
  });
  test("missing fields fall back to dash", () => {
    const view = buildStatsView("nodes: 5");
    assert.strictEqual(view.nodes, "5");
    assert.strictEqual(view.edges, ",");
    assert.strictEqual(view.schemaVersion, ",");
  });
});

suite("VSCODE-247: buildClickableFileListHtml", () => {
  test("renders direct deps and a collapsible transitive section", () => {
    const html = buildClickableFileListHtml("Deps", ["src/a.ts", "src/b.ts"], ["  ↳ src/c.ts"]);
    assert.ok(html.includes("acquireVsCodeApi"));
    assert.ok(html.includes('data-path="src/a.ts"'));
    assert.ok(html.includes("<details>"), "transitive deps under <details>");
    assert.ok(html.includes('data-path="src/c.ts"'), "↳ prefix stripped from data-path");
    assert.ok(html.includes("command: 'open'"));
  });
  test("no transitive section when none provided", () => {
    const html = buildClickableFileListHtml("Deps", ["src/a.ts"], []);
    assert.ok(!html.includes("<details>"));
  });
});

suite("VSCODE-247: resolveDepSpec", () => {
  test("returns undefined for external (non-relative) specifiers", () => {
    assert.strictEqual(resolveDepSpec("import:fs", "/src/a.ts", () => false), undefined);
    assert.strictEqual(resolveDepSpec("import:vscode", "/src/a.ts", () => false), undefined);
    assert.strictEqual(resolveDepSpec("use:std::io::Write", "/src/a.ts", () => false), undefined);
    assert.strictEqual(resolveDepSpec("use:clap::Parser", "/src/a.ts", () => false), undefined);
  });
  test("resolves relative import with .ts extension candidate", () => {
    // Normalize separators so the mock works on Windows (path.resolve uses backslashes there).
    const exists = (p: string) => p.replace(/\\/g, "/").endsWith("/src/status.ts");
    const result = resolveDepSpec("import:./status", "/src/a.ts", exists);
    assert.ok(result?.includes("status.ts"), `expected status.ts, got ${result ?? "undefined"}`);
  });
  test("resolves relative import via index.ts fallback", () => {
    const exists = (p: string) => p.replace(/\\/g, "/").endsWith("/src/utils/index.ts");
    const result = resolveDepSpec("import:./utils", "/src/a.ts", exists);
    assert.ok(result?.includes("index.ts"), `expected index.ts, got ${result ?? "undefined"}`);
  });
  test("returns undefined when no candidate matches", () => {
    const result = resolveDepSpec("import:./missing", "/src/a.ts", () => false);
    assert.strictEqual(result, undefined);
  });
  test("strips kind prefix when spec has no colon", () => {
    // bare specifier without kind prefix — treated as external
    assert.strictEqual(resolveDepSpec("fs", "/src/a.ts", () => false), undefined);
  });
});

suite("VSCODE-247: buildDepListHtml", () => {
  test("clickable entries get data-path, external entries get dep-ext class", () => {
    const html = buildDepListHtml("Deps", [
      { display: "./status", path: "src/status.ts" },
      { display: "fs" },
    ], []);
    assert.ok(html.includes('data-path="src/status.ts"'), "resolved path clickable");
    assert.ok(html.includes("dep-ext"), "external dep has dimmed class");
    assert.ok(!html.includes("<details>"), "no transitive section");
  });
  test("transitive section rendered under details", () => {
    const html = buildDepListHtml("Deps", [{ display: "./a", path: "src/a.ts" }],
      [{ display: "./b", path: "src/b.ts" }]);
    assert.ok(html.includes("<details>"));
    assert.ok(html.includes('data-path="src/b.ts"'));
  });
  test("all-external direct deps, no data-path attributes", () => {
    const html = buildDepListHtml("Deps", [{ display: "vscode" }, { display: "fs" }], []);
    assert.ok(!html.includes("data-path="), "no clickable paths for external deps");
    assert.ok(html.includes("dep-ext"));
  });
});

suite("VSCODE-247: parseLanguageCounts", () => {
  test("parses TSV language/count pairs", () => {
    const counts = parseLanguageCounts("typescript\t1234\nrust\t567");
    assert.strictEqual(counts.length, 2);
    assert.deepStrictEqual(counts[0], { language: "typescript", count: 1234 });
    assert.deepStrictEqual(counts[1], { language: "rust", count: 567 });
  });
  test("empty / enveloped input", () => {
    assert.deepStrictEqual(parseLanguageCounts(""), []);
    assert.deepStrictEqual(parseLanguageCounts("<travsr-data></travsr-data>"), []);
  });
});

suite("VSCODE-247: parseAvailableLanguages", () => {
  test("parses valid JSON array", () => {
    const raw = JSON.stringify([
      { language: "rust", package: "scip-rust", sandbox: "Standard",
        installed: true, registered: true, needsApproval: false,
        scipInstallType: "Command", installHint: "", elevatedHosts: [] }
    ]);
    const langs = parseAvailableLanguages(raw);
    assert.strictEqual(langs.length, 1);
    assert.strictEqual(langs[0].language, "rust");
  });
  test("tolerates empty / malformed input", () => {
    assert.deepStrictEqual(parseAvailableLanguages(""), []);
    assert.deepStrictEqual(parseAvailableLanguages("not json"), []);
  });
});

suite("ITEM 4: stripLangTokens", () => {
  test("strips leading language keywords", () => {
    assert.strictEqual(stripLangTokens("fn PaymentService"), "PaymentService");
    assert.strictEqual(stripLangTokens("function foo"), "foo");
    assert.strictEqual(stripLangTokens("class Baz"), "Baz");
    assert.strictEqual(stripLangTokens("def processPayment"), "processPayment");
  });
  test("leaves non-keyword queries intact", () => {
    assert.strictEqual(stripLangTokens("PaymentService"), "PaymentService");
    assert.strictEqual(stripLangTokens("foo bar"), "foo bar");
  });
  test("preserves all words when all are keywords (safety fallback)", () => {
    // Should not return empty — fall back to original words
    const result = stripLangTokens("fn class");
    assert.ok(result.length > 0);
  });
  test("case-insensitive keyword matching", () => {
    assert.strictEqual(stripLangTokens("FN PaymentService"), "PaymentService");
    assert.strictEqual(stripLangTokens("Class Baz"), "Baz");
  });
});
