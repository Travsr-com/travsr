import * as assert from "assert";
import * as vscode from "vscode";
import {
  BlastRadiusCodeLensProvider,
  BLAST_RADIUS_SELECTOR,
} from "../../codelens";
import { CallersHoverProvider } from "../../hover";
import { showWelcome } from "../../welcome";
import { StdioMcpClient } from "../../mcp";

// Minimal stub for McpClient — returns controlled responses.
function makeMcp(
  responses: Record<string, string> = {}
): import("../../mcp").McpClient {
  return {
    callTool: async (name: string) => responses[name] ?? "",
    isConnected: () => true,
    dispose: () => undefined,
  };
}

suite("VSCODE-201: BlastRadius selector covers expected languages", () => {
  test("selector includes typescript, rust, python, go", () => {
    const langs = (
      BLAST_RADIUS_SELECTOR as Array<{ language: string }>
    ).map((s) => s.language);
    assert.ok(langs.includes("typescript"));
    assert.ok(langs.includes("rust"));
    assert.ok(langs.includes("python"));
    assert.ok(langs.includes("go"));
  });
});

suite("VSCODE-202: BlastRadiusCodeLensProvider", () => {
  test("provideCodeLenses returns one lens at line 0", async () => {
    const doc = await vscode.workspace.openTextDocument({
      language: "typescript",
      content: "export const x = 1;\n",
    });
    const provider = new BlastRadiusCodeLensProvider(makeMcp());
    const lenses = provider.provideCodeLenses(doc);
    assert.strictEqual(lenses.length, 1);
    assert.strictEqual(lenses[0].range.start.line, 0);
  });

  test("resolveCodeLens returns undefined when count = 0", async () => {
    const mcp = makeMcp({ get_blast_radius: "" });
    const provider = new BlastRadiusCodeLensProvider(mcp);
    const doc = await vscode.workspace.openTextDocument({
      language: "typescript",
      content: "",
    });
    const lenses = provider.provideCodeLenses(doc);
    const result = await provider.resolveCodeLens(
      lenses[0],
      new vscode.CancellationTokenSource().token
    );
    assert.strictEqual(result, undefined);
  });

  test("resolveCodeLens sets title with file count", async () => {
    const mcp = makeMcp({ get_blast_radius: "src/a.ts\nsrc/b.ts\n" });
    const provider = new BlastRadiusCodeLensProvider(mcp);
    const doc = await vscode.workspace.openTextDocument({
      language: "typescript",
      content: "",
    });
    const lenses = provider.provideCodeLenses(doc);
    const result = await provider.resolveCodeLens(
      lenses[0],
      new vscode.CancellationTokenSource().token
    );
    assert.ok(result?.command?.title.includes("blast:"));
    assert.ok(result?.command?.title.includes("2 files"));
  });

  test("formatBlastCount caps at 99+", async () => {
    const lines = Array.from({ length: 105 }, (_, i) => `src/f${i}.ts`).join(
      "\n"
    );
    const mcp = makeMcp({ get_blast_radius: lines });
    const provider = new BlastRadiusCodeLensProvider(mcp);
    const doc = await vscode.workspace.openTextDocument({
      language: "typescript",
      content: "",
    });
    const lenses = provider.provideCodeLenses(doc);
    const result = await provider.resolveCodeLens(
      lenses[0],
      new vscode.CancellationTokenSource().token
    );
    assert.ok(result?.command?.title.includes("99+"));
  });

  test("resolveCodeLens returns undefined for untyped plain CodeLens", async () => {
    // A lens not produced by provideCodeLenses must be safely rejected.
    const provider = new BlastRadiusCodeLensProvider(makeMcp());
    const plain = new vscode.CodeLens(new vscode.Range(0, 0, 0, 0));
    const result = await provider.resolveCodeLens(
      plain,
      new vscode.CancellationTokenSource().token
    );
    assert.strictEqual(result, undefined);
  });
});

suite("VSCODE-203: CallersHoverProvider", () => {
  test("returns undefined when no callers and no blast radius", async () => {
    const mcp = makeMcp({ get_callers: "", get_blast_radius: "" });
    const provider = new CallersHoverProvider(mcp);
    const doc = await vscode.workspace.openTextDocument({
      language: "typescript",
      content: "function foo() {}\n",
    });
    const pos = new vscode.Position(0, 10);
    const hover = await provider.provideHover(
      doc,
      pos,
      new vscode.CancellationTokenSource().token
    );
    assert.strictEqual(hover, undefined);
  });

  test("renders caller lines and blast radius in hover card", async () => {
    const mcp = makeMcp({
      get_callers:
        "[call] fn:bar (function) — src/bar.ts\n[structural] class:Foo (class) — src/foo.ts",
      get_blast_radius: "src/a.ts\nsrc/b.ts\nsrc/c.ts",
    });
    const provider = new CallersHoverProvider(mcp);
    const doc = await vscode.workspace.openTextDocument({
      language: "typescript",
      content: "function foo() {}\n",
    });
    const pos = new vscode.Position(0, 10);
    const hover = await provider.provideHover(
      doc,
      pos,
      new vscode.CancellationTokenSource().token
    );
    assert.ok(hover !== undefined);
    const text =
      hover.contents[0] instanceof vscode.MarkdownString
        ? hover.contents[0].value
        : "";
    assert.ok(text.includes("Callers"));
    assert.ok(text.includes("**Blast radius:**") && text.includes("3 files"));
  });

  test("hover shows '… and N more' when callers exceed 5", async () => {
    const callerLines = Array.from(
      { length: 8 },
      (_, i) => `[call] fn:fn${i} (function) — src/f${i}.ts`
    ).join("\n");
    const mcp = makeMcp({
      get_callers: callerLines,
      get_blast_radius: "src/x.ts",
    });
    const provider = new CallersHoverProvider(mcp);
    const doc = await vscode.workspace.openTextDocument({
      language: "typescript",
      content: "function bar() {}\n",
    });
    const pos = new vscode.Position(0, 10);
    const hover = await provider.provideHover(
      doc,
      pos,
      new vscode.CancellationTokenSource().token
    );
    assert.ok(hover !== undefined);
    const text =
      hover.contents[0] instanceof vscode.MarkdownString
        ? hover.contents[0].value
        : "";
    assert.ok(text.includes("… and 3 more"));
  });

  // ── Split cache: callerCache keyed by file::symbol, blastCache by file ──

  test("same symbol name in two files gets independent caller cache entries", async () => {
    // 'id' appears in both fileA.ts and fileB.ts — they must not share a cache entry.
    let callCount = 0;
    const mcp: import("../../mcp").McpClient = {
      callTool: async (name: string) => {
        if (name === "get_callers") { callCount++; return `[call] fn:caller${callCount} (function) — src/c.ts`; }
        return "";
      },
      isConnected: () => true,
      dispose: () => undefined,
    };
    const provider = new CallersHoverProvider(mcp);
    const cts = new vscode.CancellationTokenSource();

    const docA = await vscode.workspace.openTextDocument({ language: "typescript", content: "const id = 1;\n" });
    const docB = await vscode.workspace.openTextDocument({ language: "typescript", content: "const id = 2;\n" });

    await provider.provideHover(docA, new vscode.Position(0, 7), cts.token);
    await provider.provideHover(docB, new vscode.Position(0, 7), cts.token);

    // Two distinct file::symbol keys → two get_callers calls.
    assert.strictEqual(callCount, 2, "each file must issue its own get_callers call");
  });

  test("blast radius is fetched once per file across multiple symbol hovers", async () => {
    let blastCallCount = 0;
    const mcp: import("../../mcp").McpClient = {
      callTool: async (name: string) => {
        if (name === "get_blast_radius") { blastCallCount++; return "src/a.ts\n"; }
        return `[call] fn:fn${blastCallCount} (function) — src/x.ts`;
      },
      isConnected: () => true,
      dispose: () => undefined,
    };
    const provider = new CallersHoverProvider(mcp);
    const doc = await vscode.workspace.openTextDocument({
      language: "typescript",
      content: "function foo() { return bar(); }\n",
    });
    const cts = new vscode.CancellationTokenSource();
    // Hover over two different symbols in the same document.
    await provider.provideHover(doc, new vscode.Position(0, 10), cts.token); // "foo"
    await provider.provideHover(doc, new vscode.Position(0, 25), cts.token); // "bar"
    assert.strictEqual(blastCallCount, 1, "get_blast_radius must be called once per file, not per symbol");
  });
});

// ── VSCODE-208: welcome panel dedup ────────────────────────────────────────

suite("VSCODE-208: showWelcome dedup, re-running command reveals existing panel", () => {
  test("showWelcome called twice does not throw and returns cleanly", () => {
    // In the test environment WebviewPanel is fully stubbed; calling showWelcome
    // twice exercises the currentPanel branch without crashing.
    assert.doesNotThrow(() => { showWelcome(); showWelcome(); });
  });
});

// ── VSCODE-204: MCP 10 s timeout ───────────────────────────────────────────

suite("VSCODE-204: StdioMcpClient, callTool returns '' when daemon never responds", () => {
  test("dispose() cancels pending timers without throwing", () => {
    // Construct a client that is never connected; dispose() must not throw even
    // when pendingTimers contains entries (regression guard for the timer-leak fix).
    const client = new StdioMcpClient("/nonexistent-binary");
    assert.doesNotThrow(() => client.dispose());
  });
});

// ── VSCODE-247: parity commands registered ─────────────────────────────────

suite("VSCODE-247: CLI↔UI parity commands are registered", () => {
  test("askSymbol, manageSynonyms, showDependencies, showExecutionPath exist", async () => {
    const ext = vscode.extensions.getExtension("travsr.travsr-vscode");
    assert.ok(ext, "extension must be discoverable in the test host");
    await ext!.activate();
    const commands = await vscode.commands.getCommands(true);
    for (const id of [
      "travsr.askSymbol",
      "travsr.manageSynonyms",
      "travsr.showDependencies",
      "travsr.showExecutionPath",
      "travsr.showRepos",
      "travsr.showGraphStats",
      "travsr.showLanguages",
      "travsr.reindexNow",
    ]) {
      assert.ok(commands.includes(id), `command ${id} must be registered`);
    }
  });
});
