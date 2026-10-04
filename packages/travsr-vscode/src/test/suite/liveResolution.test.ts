/**
 * Tests for src/liveResolution.ts `soleLocation` (issue #816 defect 2).
 *
 * A `LocationLink`'s `targetRange` spans the whole item including leading doc
 * comments and attributes, so its start line sits above the daemon's node span.
 * `targetSelectionRange` is the symbol name on the declaration line. The lane
 * must report the selection range so the daemon can map it to the node.
 *
 * The provider-driven case confirms the actual `vscode.executeDefinitionProvider`
 * return shape (a real `LocationLink` carrying both ranges), not only the
 * hand-built objects, so the fix is validated against VS Code's own plumbing.
 */

import * as assert from "assert";
import * as vscode from "vscode";

import { soleLocation } from "../../liveResolution";

async function docWith(content: string): Promise<vscode.TextDocument> {
  return vscode.workspace.openTextDocument({ content, language: "plaintext" });
}

suite("liveResolution: soleLocation", () => {
  test("prefers targetSelectionRange over the full item range", () => {
    const uri = vscode.Uri.file("/repo/src/user.ts");
    // targetRange starts on the doc comment (line 5); targetSelectionRange is
    // the name on the declaration line (line 7).
    const link = {
      targetUri: uri,
      targetRange: new vscode.Range(5, 0, 12, 1),
      targetSelectionRange: new vscode.Range(7, 11, 7, 15),
    };
    const found = soleLocation([link]);
    assert.ok(found, "a single link must resolve");
    assert.strictEqual(
      found.range.start.line,
      7,
      "must report the selection range's declaration line, not the item start"
    );
  });

  test("falls back to targetRange when no selection range is present", () => {
    const uri = vscode.Uri.file("/repo/src/user.ts");
    const link = {
      targetUri: uri,
      targetRange: new vscode.Range(7, 0, 12, 1),
    };
    const found = soleLocation([link]);
    assert.ok(found, "a link without a selection range must still resolve");
    assert.strictEqual(found.range.start.line, 7);
  });

  test("still accepts a bare Location", () => {
    const uri = vscode.Uri.file("/repo/src/user.ts");
    const loc = new vscode.Location(uri, new vscode.Range(7, 0, 7, 4));
    const found = soleLocation([loc]);
    assert.ok(found, "a bare Location must resolve");
    assert.strictEqual(found.range.start.line, 7);
  });

  test("confirms executeDefinitionProvider returns a LocationLink we can narrow", async () => {
    const doc = await docWith(
      "/// doc comment\n#[attr]\npub fn classify_intent() {}\n"
    );
    // targetRange covers the whole item from the doc comment (line 0);
    // targetSelectionRange is the name identifier on the declaration line 2.
    const nameStart = doc.getText().indexOf("classify_intent");
    const namePos = doc.positionAt(nameStart);
    const link: vscode.LocationLink = {
      targetUri: doc.uri,
      targetRange: new vscode.Range(0, 0, 2, 28),
      targetSelectionRange: new vscode.Range(
        namePos,
        namePos.translate(0, "classify_intent".length)
      ),
    };
    const reg = vscode.languages.registerDefinitionProvider(
      { language: "plaintext" },
      { provideDefinition: () => [link] }
    );
    try {
      const raw = await vscode.commands.executeCommand<unknown>(
        "vscode.executeDefinitionProvider",
        doc.uri,
        namePos
      );
      const list = raw as vscode.LocationLink[];
      assert.ok(Array.isArray(list) && list.length === 1, "one link back");
      assert.ok(
        list[0].targetSelectionRange,
        "the provider's selection range must survive the command round trip"
      );
      const found = soleLocation(raw);
      assert.ok(found, "must resolve the single link");
      assert.strictEqual(
        found.range.start.line,
        2,
        "the reported line must be the declaration, not the doc comment"
      );
    } finally {
      reg.dispose();
    }
  });
});
