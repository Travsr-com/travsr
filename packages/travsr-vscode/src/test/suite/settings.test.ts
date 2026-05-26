/**
 * S17-5: Package.json settings schema regression tests.
 *
 * These guard against silent privacy regressions (e.g. telemetry defaulting
 * to true) and type changes that would break the extension's startup reads.
 */

import * as assert from "assert";
import * as vscode from "vscode";

suite("S17-5: settings — schema defaults (regression guard)", () => {
  test("travsr.telemetry.enabled defaults to false (opt-in, not opt-out)", () => {
    const cfg = vscode.workspace.getConfiguration("travsr");
    const meta = cfg.inspect<boolean>("telemetry.enabled");
    assert.strictEqual(
      meta?.defaultValue,
      false,
      "telemetry must be opt-in (default false) — changing to true is a privacy regression"
    );
  });

  test("travsr.statusBarPosition defaults to 'left'", () => {
    const cfg = vscode.workspace.getConfiguration("travsr");
    const meta = cfg.inspect<string>("statusBarPosition");
    assert.strictEqual(meta?.defaultValue, "left");
  });

  test("travsr.cloudEndpoint defaults to empty string", () => {
    const cfg = vscode.workspace.getConfiguration("travsr");
    const meta = cfg.inspect<string>("cloudEndpoint");
    assert.strictEqual(meta?.defaultValue, "");
  });

  test("travsr.binaryPath defaults to empty string", () => {
    const cfg = vscode.workspace.getConfiguration("travsr");
    const meta = cfg.inspect<string>("binaryPath");
    assert.strictEqual(meta?.defaultValue, "");
  });
});
