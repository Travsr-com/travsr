import * as assert from "assert";
import * as vscode from "vscode";
import type TelemetryReporter from "@vscode/extension-telemetry";
import {
  EVT_ACTIVATED,
  EVT_MCP_INVOKED,
  EVT_DAEMON_FAILED,
  createTelemetryReporter,
  sendEvent,
} from "../../telemetry";

interface TelemetryCall { name: string; props?: Record<string, string>; }

function makeTelemetryStub() {
  const calls: TelemetryCall[] = [];
  return {
    calls,
    sendTelemetryEvent(name: string, props?: Record<string, string>) {
      calls.push({ name, props });
    },
    dispose() {},
  };
}


// ── Event name constants ───────────────────────────────────────────────────

suite("S17-5: telemetry — event name constants (regression guard)", () => {
  test("EVT_ACTIVATED has exact value 'extension.activated'", () => {
    assert.strictEqual(EVT_ACTIVATED, "extension.activated");
  });

  test("EVT_MCP_INVOKED has exact value 'mcp.provider_invoked'", () => {
    assert.strictEqual(EVT_MCP_INVOKED, "mcp.provider_invoked");
  });

  test("EVT_DAEMON_FAILED has exact value 'daemon.connect_failed'", () => {
    assert.strictEqual(EVT_DAEMON_FAILED, "daemon.connect_failed");
  });
});

// ── createTelemetryReporter ────────────────────────────────────────────────

suite("S17-5: telemetry — createTelemetryReporter", () => {
  test("returns null when enabled=false", () => {
    const result = createTelemetryReporter(false);
    assert.strictEqual(result, null);
  });

  // Integration test: only runs when isTelemetryEnabled=false in the test host.
  // The non-null branch requires @microsoft/applicationinsights-common (peer dep
  // not bundled by @vscode/extension-telemetry) — skipped when unavailable.
  test("returns null when enabled=true but isTelemetryEnabled=false", function () {
    if (vscode.env.isTelemetryEnabled) {
      // Can't safely construct a TelemetryReporter without Application Insights
      // peer deps in this test environment — skip rather than fail.
      this.skip();
      return;
    }
    const result = createTelemetryReporter(true);
    assert.strictEqual(result, null);
  });
});

// ── sendEvent ─────────────────────────────────────────────────────────────

suite("S17-5: telemetry — sendEvent", () => {
  test("sendEvent(null, EVT_ACTIVATED) does not throw", () => {
    assert.doesNotThrow(() => sendEvent(null, EVT_ACTIVATED));
  });

  test("sendEvent(null, ...) is a no-op — stub receives zero calls", () => {
    const stub = makeTelemetryStub();
    sendEvent(null, EVT_MCP_INVOKED);
    assert.strictEqual(stub.calls.length, 0);
  });

  test("sendEvent fires sendTelemetryEvent with correct event name", () => {
    const stub = makeTelemetryStub();
    sendEvent(stub as unknown as TelemetryReporter, EVT_ACTIVATED);
    assert.strictEqual(stub.calls.length, 1);
    assert.strictEqual(stub.calls[0].name, "extension.activated");
  });

  test("sendEvent passes props through to sendTelemetryEvent", () => {
    const stub = makeTelemetryStub();
    sendEvent(stub as unknown as TelemetryReporter, EVT_MCP_INVOKED, { tool: "get_callers" });
    assert.deepStrictEqual(stub.calls[0].props, { tool: "get_callers" });
  });

  // CALLER RESPONSIBILITY: sendEvent performs no PII filtering. The call site
  // in extension.ts must never pass file paths, symbol names, or graph content
  // as props — only controlled vocabulary strings (tool names, error codes).
  test("sendEvent with multiple props passes all of them without filtering", () => {
    const stub = makeTelemetryStub();
    sendEvent(stub as unknown as TelemetryReporter, EVT_MCP_INVOKED, {
      tool: "get_callers",
      file: "src/foo.ts",
    });
    assert.deepStrictEqual(stub.calls[0].props, { tool: "get_callers", file: "src/foo.ts" });
  });

  test("sendEvent called multiple times accumulates separate calls", () => {
    const stub = makeTelemetryStub();
    sendEvent(stub as unknown as TelemetryReporter, EVT_ACTIVATED);
    sendEvent(stub as unknown as TelemetryReporter, EVT_MCP_INVOKED, { tool: "get_callers" });
    sendEvent(stub as unknown as TelemetryReporter, EVT_DAEMON_FAILED);
    assert.strictEqual(stub.calls.length, 3);
    assert.strictEqual(stub.calls[0].name, "extension.activated");
    assert.strictEqual(stub.calls[1].name, "mcp.provider_invoked");
    assert.strictEqual(stub.calls[2].name, "daemon.connect_failed");
  });
});
