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
  test("selector includes typescript, rust, python", () => {
    const langs = (
      BLAST_RADIUS_SELECTOR as Array<{ language: string }>
    ).map((s) => s.language);
    assert.ok(langs.includes("typescript"));
    assert.ok(langs.includes("rust"));
    assert.ok(langs.includes("python"));
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
    const lens = new vscode.CodeLens(new vscode.Range(0, 0, 0, 0));
    const result = await provider.resolveCodeLens(
      lens,
      new vscode.CancellationTokenSource().token
    );
    assert.strictEqual(result, undefined);
  });

  test("resolveCodeLens sets title with file count", async () => {
    const mcp = makeMcp({ get_blast_radius: "src/a.ts\nsrc/b.ts\n" });
    const provider = new BlastRadiusCodeLensProvider(mcp);
    const lens = new vscode.CodeLens(new vscode.Range(0, 0, 0, 0));

    // Open a file so activeTextEditor exists
    const doc = await vscode.workspace.openTextDocument({
      language: "typescript",
      content: "",
    });
    await vscode.window.showTextDocument(doc);

    const result = await provider.resolveCodeLens(
      lens,
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
    const lens = new vscode.CodeLens(new vscode.Range(0, 0, 0, 0));

    const doc = await vscode.workspace.openTextDocument({
      language: "typescript",
      content: "",
    });
    await vscode.window.showTextDocument(doc);

    const result = await provider.resolveCodeLens(
      lens,
      new vscode.CancellationTokenSource().token
    );
    assert.ok(result?.command?.title.includes("99+"));
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
    assert.ok(text.includes("Blast radius: 3 files"));
  });
});
