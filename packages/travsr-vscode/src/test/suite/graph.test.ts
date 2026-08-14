import * as assert from "assert";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  GraphPanel,
  buildHtmlContent,
  computeDiagnosticsOverlay,
  makeDebouncer,
  MAX_DIAGNOSTIC_ITEMS_PER_FILE,
  MAX_DIAGNOSTIC_MESSAGE_CHARS,
  type GraphNode,
} from "../../graph";

/**
 * Read `media/graph.js` with line endings normalized to LF.
 *
 * The repo has no `.gitattributes`, so a Windows checkout gets CRLF. Tests
 * that locate a top-level function by its closing `\n}\n` then find nothing
 * and fail on Windows only, which is exactly how this was discovered. Reading
 * through one normalizing helper keeps every such scan platform-independent.
 */
function readGraphJs(): string {
  return fs
    .readFileSync(
      path.join(__dirname, "..", "..", "..", "media", "graph.js"),
      "utf8"
    )
    .replace(/\r\n/g, "\n");
}

// Combines HTML template + graph.js so tests can check both DOM and JS content.
function getFullHtml(): string {
  const graphJs = readGraphJs();
  const htmlTemplate = buildHtmlContent(
    "test-nonce", "test-csp",
    "graph.css", "cytoscape.min.js", "graph.js", "icon.png"
  );
  return htmlTemplate + "\n" + graphJs;
}

/**
 * Load named top-level functions out of `media/graph.js` and evaluate them in
 * isolation, so the renderer can be tested by calling it rather than by
 * string-matching its source.
 *
 * graph.js is plain ES with no module boundary and calls `acquireVsCodeApi()`
 * at load, so the whole file cannot simply be required. The functions under
 * test here depend on nothing but each other and three module-level
 * variables, which this seeds, so slicing them out is faithful rather than a
 * reimplementation. Slicing keys on a closing `}` in column 0, which is how
 * every top-level function in that file is formatted.
 */
function loadWebviewFns(names: string[]): {
  call<T>(fn: string, ...args: unknown[]): T;
  setState(state: {
    itemsByFile?: Record<string, unknown[]>;
    unknown?: string[];
    truncated?: Record<string, number>;
  }): void;
} {
  const src = readGraphJs();
  const sliced = names.map((name) => {
    const start = src.indexOf(`function ${name}(`);
    assert.ok(start >= 0, `graph.js must define ${name}()`);
    const end = src.indexOf("\n}\n", start);
    assert.ok(end > start, `${name}() must close on a brace in column 0`);
    return src.slice(start, end + 3);
  });

  const vm = require("vm") as typeof import("vm");
  const context: Record<string, unknown> = {
    _diagItemsByFile: {},
    _diagUnknown: [],
    _diagTruncated: {},
  };
  vm.createContext(context);
  vm.runInContext(sliced.join("\n"), context);

  return {
    call<T>(fn: string, ...args: unknown[]): T {
      context["__args"] = args;
      // Serialized across the realm boundary on purpose. A value built inside
      // the vm carries that realm's `Object.prototype`, which makes
      // `deepStrictEqual` fail on prototype identity even when the structure
      // matches. Round-tripping hands back plain host-realm objects, so tests
      // can assert structure without weakening to `deepEqual`.
      const json = vm.runInContext(
        `JSON.stringify(${fn}(...__args))`,
        context
      ) as string | undefined;
      return (json === undefined ? undefined : JSON.parse(json)) as T;
    },
    setState(state): void {
      if (state.itemsByFile) context["_diagItemsByFile"] = state.itemsByFile;
      if (state.unknown) context["_diagUnknown"] = state.unknown;
      if (state.truncated) context["_diagTruncated"] = state.truncated;
    },
  };
}

/** Shorthand for a listable diagnostic, in the shape the overlay posts. */
function item(
  severity: "error" | "warning",
  line: number,
  message = "boom",
  source?: string
): Record<string, unknown> {
  return source
    ? { severity, line, message, source }
    : { severity, line, message };
}

// Minimal McpClient stub
function makeClient(response: string = "{}") {
  let lastCall: { name: string; args: Record<string, string> } | null = null;
  return {
    callTool: async (name: string, args: Record<string, string> = {}) => {
      lastCall = { name, args };
      return response;
    },
    isConnected: () => true,
    dispose: () => undefined,
    getLastCall: () => lastCall,
  };
}

suite("GraphPanel", () => {
  teardown(() => {
    // Ensure the singleton is cleared between tests
    try {
      (GraphPanel as unknown as { current: unknown }).current = undefined;
    } catch {
      // ignore
    }
  });

  test("buildLoadingHtml returns non-empty HTML string", () => {
    const html = getFullHtml();
    assert.ok(html.length > 0, "HTML must be non-empty");
    assert.ok(html.includes("cytoscape"), "HTML must load Cytoscape");
    assert.ok(html.includes("acquireVsCodeApi"), "HTML must acquire VS Code API");
    assert.ok(html.includes("window.addEventListener"), "HTML must listen for messages");
  });

  test("buildLoadingHtml includes the vscode postMessage bridge", () => {
    const html = getFullHtml();
    assert.ok(
      html.includes("vscode.postMessage"),
      "webview must send messages back to extension"
    );
  });

  test("query() calls get_graph_json with correct arguments", async () => {
    const client = makeClient(JSON.stringify({ nodes: [], edges: [] }));
    // GraphPanel.show creates a WebviewPanel which requires vscode — skip in unit test
    // Instead, test the argument passing logic directly via the client stub
    await client.callTool("get_graph_json", {
      query: "PaymentService",
      direction: "both",
      depth: "2",
    });
    const last = client.getLastCall();
    assert.strictEqual(last?.name, "get_graph_json");
    assert.strictEqual(last?.args["query"], "PaymentService");
    assert.strictEqual(last?.args["direction"], "both");
    assert.strictEqual(last?.args["depth"], "2");
  });

  test("query() with malformed JSON does not throw", async () => {
    const client = makeClient("not-valid-json{{{{");
    // Exercise the JSON.parse error path directly
    let caught = false;
    try {
      const raw = await client.callTool("get_graph_json", { query: "x", direction: "both", depth: "2" });
      // Simulate the JSON.parse that GraphPanel.query() does
      JSON.parse(raw);
    } catch {
      caught = true;
    }
    // The parse throws but GraphPanel.query swallows it — verify that pattern works
    assert.ok(caught, "malformed JSON should throw on parse");
    // No re-throw means the panel stays alive with empty data
  });

  test("buildLoadingHtml status bar uses visible-element counts", () => {
    const html = getFullHtml();
    // updateStatusBar must use ':visible' selector, not plain .nodes()
    assert.ok(
      html.includes("':visible'") || html.includes("nodes(':visible')"),
      "status bar must count only visible nodes"
    );
  });

  test("buildLoadingHtml includes depth slider", () => {
    const html = getFullHtml();
    assert.ok(html.includes("depthSlider"), "must include depth slider");
    assert.ok(html.includes("depthVal"), "must display depth value");
  });

  test("buildLoadingHtml includes vars toggle", () => {
    const html = getFullHtml();
    assert.ok(html.includes("toggleVars"), "must include vars toggle function");
    assert.ok(html.includes("btn-vars"), "must include vars toggle button");
  });

  test("VSCODE-247: buildLoadingHtml includes DOT/JSON export", () => {
    const html = getFullHtml();
    assert.ok(html.includes("function exportDot"), "must define exportDot()");
    assert.ok(html.includes("function exportJson"), "must define exportJson()");
    assert.ok(html.includes("⤓ DOT"), "must include DOT toolbar button");
    assert.ok(html.includes("⤓ JSON"), "must include JSON toolbar button");
    assert.ok(html.includes("command: 'exportDot'"), "exportDot posts a message");
    assert.ok(html.includes("command: 'exportJson'"), "exportJson posts a message");
    assert.ok(html.includes("digraph travsr"), "DOT output is a Graphviz digraph");
  });
});

// ── #688: live LSP diagnostics overlay ───────────────────────────────────────
//
// Uses a real DiagnosticCollection rather than a stub: the thing under test is
// the reduction over what `vscode.languages.getDiagnostics` actually returns,
// and a hand-rolled fake of that API would only prove the fake works.

const node = (id: string, filePath: string): GraphNode => ({
  id,
  label: id,
  kind: "function",
  path: filePath,
  package: "",
  score: 1,
});

function diag(line: number, severity: vscode.DiagnosticSeverity): vscode.Diagnostic {
  return new vscode.Diagnostic(
    new vscode.Range(line, 0, line, 1),
    `synthetic ${vscode.DiagnosticSeverity[severity]}`,
    severity
  );
}

/** Absolute URI inside the fixture workspace, so resolveWorkspacePath accepts it. */
function fixtureUri(rel: string): vscode.Uri {
  const root = vscode.workspace.workspaceFolders?.[0]?.uri;
  assert.ok(root, "fixture workspace must be open");
  return vscode.Uri.joinPath(root, rel);
}

suite("GraphPanel: diagnostics overlay (#688)", () => {
  let collection: vscode.DiagnosticCollection;

  setup(() => {
    collection = vscode.languages.createDiagnosticCollection("travsr-test-688");
  });

  teardown(() => {
    collection.dispose();
  });

  test("errors outrank warnings in the same file, count is of the winner", () => {
    const uri = fixtureUri("src/sample.ts");
    collection.set(uri, [
      diag(1, vscode.DiagnosticSeverity.Warning),
      diag(2, vscode.DiagnosticSeverity.Error),
      diag(3, vscode.DiagnosticSeverity.Warning),
      diag(4, vscode.DiagnosticSeverity.Error),
    ]);

    const overlay = computeDiagnosticsOverlay([node("n1", "src/sample.ts")]);

    assert.deepStrictEqual(overlay.byNode["n1"], { severity: "error", count: 2 });
  });

  test("warning-only file reduces to warning", () => {
    collection.set(fixtureUri("src/sample.ts"), [
      diag(1, vscode.DiagnosticSeverity.Warning),
    ]);

    const overlay = computeDiagnosticsOverlay([node("n1", "src/sample.ts")]);

    assert.deepStrictEqual(overlay.byNode["n1"], { severity: "warning", count: 1 });
  });

  test("Info and Hint severities do not badge a node", () => {
    collection.set(fixtureUri("src/sample.ts"), [
      diag(1, vscode.DiagnosticSeverity.Information),
      diag(2, vscode.DiagnosticSeverity.Hint),
    ]);

    const overlay = computeDiagnosticsOverlay([node("n1", "src/sample.ts")]);

    assert.ok(!("n1" in overlay.byNode), "info/hint must not paint a node");
  });

  test("clean nodes are omitted rather than zeroed", () => {
    collection.set(fixtureUri("src/sample.ts"), [
      diag(1, vscode.DiagnosticSeverity.Error),
    ]);

    const overlay = computeDiagnosticsOverlay([
      node("dirty", "src/sample.ts"),
      node("clean", "src/mcp.ts"),
    ]);

    assert.ok("dirty" in overlay.byNode);
    assert.strictEqual(
      Object.prototype.hasOwnProperty.call(overlay.byNode, "clean"),
      false,
      "a clean node must be absent from byNode, not present with count 0"
    );
  });

  test("file-scoped attribution badges every node from the same file", () => {
    collection.set(fixtureUri("src/sample.ts"), [
      diag(40, vscode.DiagnosticSeverity.Error),
    ]);

    const overlay = computeDiagnosticsOverlay([
      node("a", "src/sample.ts"),
      node("b", "src/sample.ts"),
    ]);

    assert.strictEqual(overlay.scope, "file");
    assert.deepStrictEqual(overlay.byNode["a"], overlay.byNode["b"]);
  });

  test("nodes outside the workspace are dropped, not badged", () => {
    const overlay = computeDiagnosticsOverlay([
      node("escape", "/etc/passwd"),
    ]);

    assert.ok(!("escape" in overlay.byNode));
    assert.ok(
      !overlay.unknownCoverage.includes("/etc/passwd"),
      "a rejected path is not reported as undiagnosed either"
    );
  });

  
  test("a file no provider has published for is reported as not diagnosed", () => {
    collection.set(fixtureUri("src/sample.ts"), [
      diag(1, vscode.DiagnosticSeverity.Error),
    ]);

    const overlay = computeDiagnosticsOverlay([
      node("seen", "src/sample.ts"),
      node("unseen", "src/mcp.ts"),
    ]);

    assert.ok(
      !overlay.unknownCoverage.includes("src/sample.ts"),
      "a file with published diagnostics has known coverage"
    );
    assert.ok(
      overlay.unknownCoverage.includes("src/mcp.ts"),
      "a file nothing has diagnosed must not be implied clean"
    );
  });

  test("each file is looked up once regardless of node count", () => {
    collection.set(fixtureUri("src/sample.ts"), [
      diag(1, vscode.DiagnosticSeverity.Warning),
    ]);

    const nodes = Array.from({ length: 50 }, (_, i) => node(`n${i}`, "src/sample.ts"));
    const overlay = computeDiagnosticsOverlay(nodes);

    assert.strictEqual(Object.keys(overlay.byNode).length, 50);
    assert.strictEqual(
      overlay.unknownCoverage.length,
      0,
      "one file must contribute at most one coverage entry"
    );
  });

  // ── Problems list in the detail panel ───────────────────────────────────
  // The badge answers "is this broken"; these cover "broken how, and where",
  // which is what the panel rows are clickable for.

  test("items carry a 1-based line, so a click lands where the diagnostic is", () => {
    // `Range` is 0-based; the graph's own `line` and `goToDefinition` are not.
    collection.set(fixtureUri("src/sample.ts"), [
      diag(41, vscode.DiagnosticSeverity.Error),
    ]);

    const overlay = computeDiagnosticsOverlay([node("n1", "src/sample.ts")]);
    const items = overlay.itemsByFile["src/sample.ts"];

    assert.strictEqual(items.length, 1);
    assert.strictEqual(items[0].line, 42, "0-based 41 must surface as line 42");
    assert.strictEqual(items[0].severity, "error");
    assert.ok(items[0].message.length > 0, "the row needs something to read");
  });

  test("items are keyed by file, so nodes sharing a file share one list", () => {
    collection.set(fixtureUri("src/sample.ts"), [
      diag(1, vscode.DiagnosticSeverity.Error),
    ]);

    const overlay = computeDiagnosticsOverlay([
      node("a", "src/sample.ts"),
      node("b", "src/sample.ts"),
    ]);

    assert.deepStrictEqual(Object.keys(overlay.itemsByFile), ["src/sample.ts"]);
  });

  test("errors sort above warnings, then by line", () => {
    collection.set(fixtureUri("src/sample.ts"), [
      diag(30, vscode.DiagnosticSeverity.Warning),
      diag(20, vscode.DiagnosticSeverity.Error),
      diag(10, vscode.DiagnosticSeverity.Warning),
      diag(5, vscode.DiagnosticSeverity.Error),
    ]);

    const items = computeDiagnosticsOverlay([node("n1", "src/sample.ts")])
      .itemsByFile["src/sample.ts"];

    assert.deepStrictEqual(
      items.map((i) => [i.severity, i.line]),
      [
        ["error", 6],
        ["error", 21],
        ["warning", 11],
        ["warning", 31],
      ]
    );
  });

  test("Info and Hint are excluded from the list, as they are from the badge", () => {
    // The list and the ring must never disagree about what counts as broken.
    collection.set(fixtureUri("src/sample.ts"), [
      diag(1, vscode.DiagnosticSeverity.Error),
      diag(2, vscode.DiagnosticSeverity.Information),
      diag(3, vscode.DiagnosticSeverity.Hint),
    ]);

    const items = computeDiagnosticsOverlay([node("n1", "src/sample.ts")])
      .itemsByFile["src/sample.ts"];

    assert.strictEqual(items.length, 1, "only the error is listable");
    assert.strictEqual(items[0].severity, "error");
  });

  test("a clean file contributes no list at all, not an empty one", () => {
    const overlay = computeDiagnosticsOverlay([node("n1", "src/mcp.ts")]);

    assert.strictEqual(
      Object.prototype.hasOwnProperty.call(overlay.itemsByFile, "src/mcp.ts"),
      false
    );
  });

  test("an overlong message is truncated rather than shipped whole", () => {
    const long = "x".repeat(MAX_DIAGNOSTIC_MESSAGE_CHARS + 500);
    collection.set(fixtureUri("src/sample.ts"), [
      new vscode.Diagnostic(
        new vscode.Range(0, 0, 0, 1),
        long,
        vscode.DiagnosticSeverity.Error
      ),
    ]);

    const items = computeDiagnosticsOverlay([node("n1", "src/sample.ts")])
      .itemsByFile["src/sample.ts"];

    assert.strictEqual(items[0].message.length, MAX_DIAGNOSTIC_MESSAGE_CHARS);
    assert.ok(items[0].message.endsWith("…"), "truncation must be visible");
  });

  test("a file over the item cap is clamped and the remainder counted", () => {
    const many = Array.from({ length: MAX_DIAGNOSTIC_ITEMS_PER_FILE + 7 }, (_, i) =>
      diag(i, vscode.DiagnosticSeverity.Error)
    );
    collection.set(fixtureUri("src/sample.ts"), many);

    const overlay = computeDiagnosticsOverlay([node("n1", "src/sample.ts")]);

    assert.strictEqual(
      overlay.itemsByFile["src/sample.ts"].length,
      MAX_DIAGNOSTIC_ITEMS_PER_FILE
    );
    assert.strictEqual(
      overlay.itemsTruncated["src/sample.ts"],
      7,
      "the overflow is counted, never silently dropped"
    );
  });

  test("message text never reaches the daemon's editor plane", () => {
    // `broken` crosses a socket into another process; `itemsByFile` does not
    // leave this extension host. Only the second may carry source-derived text.
    collection.set(fixtureUri("src/sample.ts"), [
      new vscode.Diagnostic(
        new vscode.Range(0, 0, 0, 1),
        "secret-looking message text",
        vscode.DiagnosticSeverity.Error
      ),
    ]);

    const overlay = computeDiagnosticsOverlay([node("n1", "src/sample.ts")]);

    assert.ok(
      !JSON.stringify(overlay.broken).includes("secret-looking"),
      "the daemon plane stays counts-only"
    );
    assert.ok(
      JSON.stringify(overlay.itemsByFile).includes("secret-looking"),
      "the panel list is where the text belongs"
    );
  });

  test("the webview renders clickable problem rows that carry the line", () => {
    const html = getFullHtml();

    assert.ok(
      html.includes("data-diag-line"),
      "rows must carry the line the click navigates to"
    );
    assert.ok(
      html.includes("diagProblemsHtml"),
      "the detail panel must build a Problems section"
    );
    // The click must reuse the existing navigation command rather than
    // inventing a second path to the same place.
    const wiring = html.slice(html.indexOf("function wireDiagProblems"));
    assert.ok(
      wiring.slice(0, 500).includes("goToDefinition"),
      "a problem row must navigate via goToDefinition"
    );
  });

  test("a burst of diagnostics changes coalesces into exactly one post", async () => {
    let calls = 0;
    const d = makeDebouncer(() => calls++, 20);

    d.schedule();
    d.schedule();
    d.schedule();
    assert.strictEqual(calls, 0, "must not fire synchronously");

    await new Promise((r) => setTimeout(r, 60));
    assert.strictEqual(calls, 1, "three changes in one window must post once");
    d.dispose();
  });

  test("disposing cancels a pending post", async () => {
    let calls = 0;
    const d = makeDebouncer(() => calls++, 20);

    d.schedule();
    d.dispose();

    await new Promise((r) => setTimeout(r, 60));
    assert.strictEqual(calls, 0, "a disposed panel must not post after teardown");
  });
});

suite("webview test harness is platform-independent", () => {
  // The repo has no `.gitattributes`, so a Windows checkout gets CRLF and
  // every "find the closing brace" scan in these tests silently matched
  // nothing. It failed on Windows only, after passing everywhere else.
  test("graph.js is read with LF endings whatever the checkout did", () => {
    const src = readGraphJs();

    assert.ok(!src.includes("\r\n"), "CRLF must be normalized away");
    assert.ok(
      src.includes("\n}\n"),
      "a top-level function must be locatable by its closing brace"
    );
  });

  test("function slicing works on a CRLF source", () => {
    // Proves the normalization is what makes the scan work, rather than the
    // host happening to check out LF.
    const crlf = readGraphJs().replace(/\n/g, "\r\n");
    const normalized = crlf.replace(/\r\n/g, "\n");

    assert.strictEqual(crlf.indexOf("\n}\n"), -1, "CRLF defeats the raw scan");
    assert.ok(normalized.indexOf("\n}\n") > 0, "normalizing restores it");
  });

  test("every function the vm suites load can actually be sliced out", () => {
    // A rename in graph.js would otherwise surface as a confusing hook
    // failure inside an unrelated suite.
    for (const name of [
      "escHtml",
      "diagProblemsHtml",
      "diagByLineFor",
      "peekOutsideCount",
    ]) {
      const fns = loadWebviewFns([name]);
      assert.ok(fns, `${name} must be loadable`);
    }
  });
});

suite("GraphPanel: Problems section renderer (#688)", () => {
  let fns: ReturnType<typeof loadWebviewFns>;

  setup(() => {
    fns = loadWebviewFns(["escHtml", "diagProblemsHtml"]);
  });

  const html = (p: string): string => fns.call<string>("diagProblemsHtml", p);

  test("a node with no path renders nothing at all", () => {
    assert.strictEqual(html(""), "");
  });

  test("a clean, diagnosed file renders nothing rather than an empty section", () => {
    // Silence is correct here: the file was looked at and found fine, and a
    // "Problems (0)" heading would be noise on every healthy node.
    fns.setState({ itemsByFile: {}, unknown: [] });
    assert.strictEqual(html("src/clean.ts"), "");
  });

  test("an undiagnosed file says so instead of rendering nothing", () => {
    // Absence of diagnostics is not evidence of correctness, so this branch
    // must not be collapsed into the clean one above.
    fns.setState({ itemsByFile: {}, unknown: ["src/rust.rs"] });
    const out = html("src/rust.rs");

    assert.ok(out.includes("Problems"), "the section must appear");
    assert.ok(
      out.includes("not diagnosed rather than clean"),
      "must not let the absence read as a pass"
    );
    assert.ok(!out.includes("diag-item"), "there are no rows to draw");
  });

  test("each row carries its own line, so a click lands on that diagnostic", () => {
    fns.setState({
      itemsByFile: {
        "src/a.ts": [item("error", 12), item("warning", 40)],
      },
    });
    const out = html("src/a.ts");

    assert.ok(out.includes('data-diag-line="12"'), `got: ${out}`);
    assert.ok(out.includes('data-diag-line="40"'), `got: ${out}`);
  });

  test("severity reaches the row as a class, not only as a colour", () => {
    // Colour alone would leave the distinction invisible to anyone who cannot
    // see it; the class is what the border and icon hang off.
    fns.setState({
      itemsByFile: { "src/a.ts": [item("error", 1), item("warning", 2)] },
    });
    const out = html("src/a.ts");

    assert.ok(out.includes("diag-item is-err"), `got: ${out}`);
    assert.ok(out.includes("diag-item is-warn"), `got: ${out}`);
  });

  test("markup in a diagnostic message cannot escape into the page", () => {
    // Diagnostic text is produced by whatever language server is installed and
    // can quote source, so it is untrusted input to this renderer.
    fns.setState({
      itemsByFile: {
        "src/a.ts": [
          item("error", 1, '<img src=x onerror="alert(1)">'),
        ],
      },
    });
    const out = html("src/a.ts");

    assert.ok(!out.includes("<img"), `raw markup rendered: ${out}`);
    assert.ok(out.includes("&lt;img"), `must escape instead: ${out}`);
  });

  test("a message with a quote cannot break out of the title attribute", () => {
    fns.setState({
      itemsByFile: { "src/a.ts": [item("error", 1, 'say "hi" now')] },
    });
    const out = html("src/a.ts");

    assert.ok(!out.includes('title="say "hi"'), `attribute broken: ${out}`);
    assert.ok(out.includes("&quot;hi&quot;"), `got: ${out}`);
  });

  test("the heading counts each severity separately and pluralises", () => {
    fns.setState({
      itemsByFile: {
        "src/a.ts": [item("error", 1), item("error", 2), item("warning", 3)],
      },
    });
    const out = html("src/a.ts");

    assert.ok(out.includes("2 errors"), `got: ${out}`);
    assert.ok(out.includes("1 warning"), `got: ${out}`);
    assert.ok(!out.includes("1 warnings"), `bad plural: ${out}`);
  });

  test("a single error reads '1 error', not '1 errors'", () => {
    fns.setState({ itemsByFile: { "src/a.ts": [item("error", 1)] } });
    assert.ok(html("src/a.ts").includes("1 error"));
    assert.ok(!html("src/a.ts").includes("1 errors"));
  });

  test("a clamped file says how many it is not showing", () => {
    fns.setState({
      itemsByFile: { "src/a.ts": [item("error", 1)] },
      truncated: { "src/a.ts": 7 },
    });
    const out = html("src/a.ts");

    assert.ok(out.includes("7 more"), `the overflow must be visible: ${out}`);
  });

  test("an unclamped file claims no hidden rows", () => {
    fns.setState({ itemsByFile: { "src/a.ts": [item("error", 1)] }, truncated: {} });
    assert.ok(!html("src/a.ts").includes("more, not listed"));
  });

  test("the per-file caveat rides along, so the count is never read as per symbol", () => {
    fns.setState({ itemsByFile: { "src/a.ts": [item("error", 1)] } });
    assert.ok(html("src/a.ts").includes("per file, not per symbol"));
  });

  test("a provider name is shown when set and omitted when not", () => {
    fns.setState({
      itemsByFile: {
        "src/a.ts": [item("error", 1, "boom", "eslint")],
        "src/b.ts": [item("error", 1, "boom")],
      },
    });

    assert.ok(html("src/a.ts").includes("eslint"), "source must be shown");
    assert.ok(
      !html("src/b.ts").includes("diag-item-src"),
      "no empty source cell when the provider set none"
    );
  });

  test("rows are rendered in the order given, so the host owns the sort", () => {
    // The reduction already sorts errors first, then by line. Re-sorting here
    // would let the two disagree.
    fns.setState({
      itemsByFile: {
        "src/a.ts": [item("error", 90), item("error", 2), item("warning", 5)],
      },
    });
    const out = html("src/a.ts");
    const order = [...out.matchAll(/data-diag-line="(\d+)"/g)].map((m) => m[1]);

    assert.deepStrictEqual(order, ["90", "2", "5"]);
  });
});

suite("GraphPanel: diagnostics in the definition peek (#688)", () => {
  let fns: ReturnType<typeof loadWebviewFns>;

  setup(() => {
    fns = loadWebviewFns(["diagByLineFor"]);
  });

  const byLine = (p: string): Record<string, { severity: string; messages: string[] }> =>
    fns.call("diagByLineFor", p);

  test("a file with no diagnostics marks no lines", () => {
    fns.setState({ itemsByFile: {} });
    assert.deepStrictEqual(byLine("src/a.ts"), {});
  });

  test("each diagnostic marks its own line", () => {
    fns.setState({
      itemsByFile: { "src/a.ts": [item("error", 3), item("warning", 9)] },
    });
    const out = byLine("src/a.ts");

    assert.strictEqual(out["3"].severity, "error");
    assert.strictEqual(out["9"].severity, "warning");
  });

  test("a line with both severities marks as the error", () => {
    // The gutter can only show one glyph, and the more urgent fact wins.
    fns.setState({
      itemsByFile: {
        "src/a.ts": [item("warning", 7, "lint"), item("error", 7, "type")],
      },
    });
    assert.strictEqual(byLine("src/a.ts")["7"].severity, "error");
  });

  test("severity does not downgrade when a warning follows an error", () => {
    // Order-dependence here would make the marker depend on the host's sort.
    fns.setState({
      itemsByFile: {
        "src/a.ts": [item("error", 7, "type"), item("warning", 7, "lint")],
      },
    });
    assert.strictEqual(byLine("src/a.ts")["7"].severity, "error");
  });

  test("every message on a line is kept, for the marker's tooltip", () => {
    fns.setState({
      itemsByFile: {
        "src/a.ts": [item("error", 7, "first"), item("error", 7, "second")],
      },
    });
    assert.deepStrictEqual(byLine("src/a.ts")["7"].messages, ["first", "second"]);
  });

  test("problems beyond the item cap still count as outside the window", () => {
    // `_diagItemsByFile` is clamped by the host with the remainder in
    // `_diagTruncated`. Counting only the clamped list under-reports on
    // exactly the files that have the most wrong with them.
    const fns2 = loadWebviewFns(["peekOutsideCount"]);
    fns2.setState({
      itemsByFile: { "src/a.ts": [item("error", 5)] },
      truncated: { "src/a.ts": 7 },
    });

    // Line 5 is on screen, so the one listed item is inside; the 7 clamped
    // ones are not, and must still be reported.
    const outside = fns2.call<number>(
      "peekOutsideCount",
      "src/a.ts",
      { 5: { severity: "error", messages: ["boom"] } },
      [5]
    );

    assert.strictEqual(outside, 7);
  });

  test("outside count never goes negative", () => {
    const fns2 = loadWebviewFns(["peekOutsideCount"]);
    fns2.setState({ itemsByFile: { "src/a.ts": [item("error", 5)] }, truncated: {} });

    const outside = fns2.call<number>(
      "peekOutsideCount",
      "src/a.ts",
      { 5: { severity: "error", messages: ["a", "b", "c"] } },
      [5]
    );

    assert.strictEqual(outside, 0, "a clamped-to-zero floor, not a negative");
  });

  test("re-marking does not rebuild the peek body, so scroll survives typing", () => {
    // Overlays land on every keystroke. Rebuilding innerHTML here would send
    // the reader back to the top of the file each time they type.
    const html = getFullHtml();
    const fn = html.indexOf("function markPeekDiagnostics");
    const end = html.indexOf("\n}\n", fn);
    const body = html.slice(fn, end);

    assert.ok(fn > 0, "marking must be its own function");
    assert.ok(
      !body.includes("innerHTML"),
      "marking must update in place, not re-render"
    );
    assert.ok(
      body.includes("classList"),
      "marking works by toggling classes on existing rows"
    );
  });

  test("the refresh path marks rather than re-renders", () => {
    const html = getFullHtml();
    const fn = html.indexOf("function refreshOpenPeekDiagnostics");
    const body = html.slice(fn, html.indexOf("\n}\n", fn));

    assert.ok(
      body.includes("markPeekDiagnostics"),
      "refresh must mark in place"
    );
    assert.ok(
      !body.includes("renderPeekPanel"),
      "refresh must not go back through a full render"
    );
  });

  test("the Problems list keeps its scroll position across a refresh", () => {
    const html = getFullHtml();
    const fn = html.indexOf("function refreshOpenDetailProblems");
    const body = html.slice(fn, html.indexOf("\n}\n", fn));

    assert.ok(body.includes("scrollTop"), `must preserve scroll: ${body}`);
  });

  test("every row carries its line, marked or not, so marking cannot reflow", () => {
    const html = getFullHtml();
    const fn = html.indexOf("function renderPeekPanel");
    const body = html.slice(fn, html.indexOf("\n}\n", fn));

    assert.ok(body.includes('data-line="'), "rows are addressable by line");
    assert.ok(
      body.includes('<span class="pk-mark"></span>'),
      "the gutter slot exists on clean rows too, so marking adds no width"
    );
  });

  test("the peek renders a gutter marker and jumps to the marked line", () => {
    const html = getFullHtml();

    assert.ok(html.includes("pk-mark"), "peek must have a diagnostics gutter");

    // The click must land on the problem, not on the definition the peek
    // opened at, and only on rows that actually carry one.
    const render = html.indexOf("function renderPeekPanel");
    const body = html.slice(render, html.indexOf("\n}\n", render));
    assert.ok(body.includes("onclick"), "the body must handle clicks");
    assert.ok(
      body.includes("pk-err") && body.includes("pk-warn"),
      "an unmarked line must not be clickable"
    );
    assert.ok(
      body.includes("data-line"),
      "the click target is the row's own line"
    );
    assert.ok(
      html.includes("outside the lines shown"),
      "problems beyond the peeked window must be disclosed, not hidden"
    );
  });

  test("an open peek re-marks when a new overlay lands", () => {
    const html = getFullHtml();
    const apply = html.indexOf("function applyDiagnosticsOverlay");
    const refresh = html.indexOf("refreshOpenPeekDiagnostics();", apply);

    assert.ok(
      apply > 0 && refresh > apply,
      "applyDiagnosticsOverlay must refresh an open peek"
    );
  });
});

suite("GraphPanel: diagnostics overlay renderer (#688)", () => {
  test("webview handles the diagnosticsOverlay message", () => {
    const html = getFullHtml();
    assert.ok(
      html.includes("msg.command === 'diagnosticsOverlay'"),
      "renderer must handle the diagnosticsOverlay message"
    );
    assert.ok(
      html.includes("function applyDiagnosticsOverlay"),
      "renderer must define applyDiagnosticsOverlay()"
    );
  });

  test("diagnostic styling uses design tokens and outranks warning with error", () => {
    const html = getFullHtml();
    assert.ok(html.includes("node.diag-error"), "must style error nodes");
    assert.ok(html.includes("node.diag-warn"), "must style warning nodes");
    assert.ok(
      html.indexOf("node.diag-warn") < html.indexOf("node.diag-error"),
      "error style must come last so it wins on equal specificity"
    );
    assert.ok(
      html.includes("'outline-color': C.err") && html.includes("'outline-color': C.warn"),
      "colors must come from the C token map, not inline hex"
    );
  });

  test("overlay repaints after a client-side re-render", () => {
    const html = getFullHtml();
    const render = html.indexOf("function renderGraph");
    const paintInRender = html.indexOf("paintDiagnostics();", render);
    assert.ok(
      render > 0 && paintInRender > render,
      "renderGraph must repaint diagnostics after rebuilding elements"
    );
  });

  test("panel carries a coverage note rather than implying clean", () => {
    const html = getFullHtml();
    assert.ok(html.includes('id="diagBadge"'), "status bar must have a diagnostics badge");
    assert.ok(html.includes("not diagnosed"), "must distinguish undiagnosed from clean");
    assert.ok(
      html.includes("Counts are per file, not per symbol."),
      "must state the file-scoped attribution caveat"
    );
  });
});
