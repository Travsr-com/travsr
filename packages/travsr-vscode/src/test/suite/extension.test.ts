import * as assert from "assert";
import * as vscode from "vscode";
import {
  BlastRadiusCodeLensProvider,
  BLAST_RADIUS_SELECTOR,
} from "../../codelens";
import { CallersHoverProvider } from "../../hover";

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
});
