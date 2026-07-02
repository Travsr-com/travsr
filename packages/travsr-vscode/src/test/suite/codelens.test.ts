/**
 * Unit tests for src/codelens.ts.
 *
 * Covers ITEM 3 (parity plan §ITEM 3 part 2): the passive high-blast marker.
 * The title-building logic (`blastCommand`) is pure and deterministic, so the
 * threshold flip is tested directly; a mock-client `resolveCodeLens` test proves
 * the end-to-end path (fetch → cache → title) and the no-refetch re-title.
 */

import * as assert from "assert";
import * as vscode from "vscode";
import { BlastRadiusCodeLensProvider, blastCommand } from "../../codelens";

const files = (n: number): string[] =>
  Array.from({ length: n }, (_, i) => `src/dep${i}.ts`);

suite("codelens: blastCommand (high-blast threshold)", () => {
  test("below threshold → neutral title", () => {
    const cmd = blastCommand("src/a.ts", files(5), 20);
    assert.ok(cmd.title.startsWith("🩻 blast:"), `neutral expected, got: ${cmd.title}`);
    assert.ok(!cmd.title.includes("high blast"));
    assert.ok(cmd.title.includes("5 files"));
  });

  test("at threshold boundary → escalates to warning", () => {
    const cmd = blastCommand("src/a.ts", files(20), 20);
    assert.ok(cmd.title.startsWith("⚠️ high blast:"), `warning expected, got: ${cmd.title}`);
    assert.ok(cmd.title.includes("review before editing"));
  });

  test("above threshold → warning title", () => {
    const cmd = blastCommand("src/a.ts", files(37), 20);
    assert.ok(cmd.title.startsWith("⚠️ high blast:"));
    assert.ok(cmd.title.includes("37 files"));
  });

  test("singular vs plural file wording", () => {
    assert.ok(blastCommand("src/a.ts", files(1), 20).title.endsWith("1 file"));
    assert.ok(blastCommand("src/a.ts", files(2), 20).title.includes("2 files"));
  });

  test("counts over 99 render as 99+ and are always high", () => {
    const cmd = blastCommand("src/a.ts", files(150), 20);
    assert.ok(cmd.title.includes("99+ files"));
    assert.ok(cmd.title.startsWith("⚠️ high blast:"));
  });

  test("command target and arguments are preserved", () => {
    const list = files(3);
    const cmd = blastCommand("src/a.ts", list, 20);
    assert.strictEqual(cmd.command, "travsr.showBlastRadius");
    assert.deepStrictEqual(cmd.arguments, ["src/a.ts", list]);
  });

  test("a lower threshold flips a previously-neutral count to warning", () => {
    const list = files(10);
    assert.ok(blastCommand("src/a.ts", list, 20).title.startsWith("🩻 blast:"));
    assert.ok(blastCommand("src/a.ts", list, 5).title.startsWith("⚠️ high blast:"));
  });
});

suite("codelens: resolveCodeLens end-to-end", () => {
  // Minimal, non-cancelled token.
  const token = new vscode.CancellationTokenSource().token;

  function providerReturning(list: string[]): BlastRadiusCodeLensProvider {
    let calls = 0;
    const mcp: import("../../mcp").McpClient = {
      callTool: async () => {
        calls += 1;
        return list.join("\n");
      },
      isConnected: () => true,
      dispose: () => {},
    };
    const provider = new BlastRadiusCodeLensProvider(mcp);
    // Expose the call counter for the no-refetch assertion.
    (provider as unknown as { _calls: () => number })._calls = () => calls;
    return provider;
  }

  test("resolves a lens and titles it from the fetched count", async () => {
    const provider = providerReturning(files(3));
    const [lens] = provider.provideCodeLenses(
      { uri: vscode.Uri.file("/repo/src/a.ts") } as vscode.TextDocument
    );
    const resolved = await provider.resolveCodeLens(lens, token);
    assert.ok(resolved?.command, "lens should resolve to a command");
    assert.strictEqual(resolved?.command?.command, "travsr.showBlastRadius");
  });

  test("zero dependents → no lens (undefined)", async () => {
    const provider = providerReturning([]);
    const [lens] = provider.provideCodeLenses(
      { uri: vscode.Uri.file("/repo/src/a.ts") } as vscode.TextDocument
    );
    const resolved = await provider.resolveCodeLens(lens, token);
    assert.strictEqual(resolved, undefined);
  });

  test("second resolve of same file re-titles from cache without a re-fetch", async () => {
    const provider = providerReturning(files(4));
    const calls = () => (provider as unknown as { _calls: () => number })._calls();
    const doc = { uri: vscode.Uri.file("/repo/src/a.ts") } as vscode.TextDocument;

    await provider.resolveCodeLens(provider.provideCodeLenses(doc)[0], token);
    assert.strictEqual(calls(), 1, "first resolve should fetch once");

    await provider.resolveCodeLens(provider.provideCodeLenses(doc)[0], token);
    assert.strictEqual(calls(), 1, "cached count must not trigger a second fetch");
  });
});
