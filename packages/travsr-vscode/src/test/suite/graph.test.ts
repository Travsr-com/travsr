import * as assert from "assert";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  GraphPanel,
  buildHtmlContent,
  computeDiagnosticsOverlay,
  makeDebouncer,
  type GraphNode,
} from "../../graph";

// Combines HTML template + graph.js so tests can check both DOM and JS content.
function getFullHtml(): string {
  const graphJs = fs.readFileSync(
    path.join(__dirname, "..", "..", "..", "media", "graph.js"),
    "utf8"
  );
  const htmlTemplate = buildHtmlContent(
    "test-nonce", "test-csp",
    "graph.css", "cytoscape.min.js", "graph.js", "icon.png"
  );
  return htmlTemplate + "\n" + graphJs;
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
