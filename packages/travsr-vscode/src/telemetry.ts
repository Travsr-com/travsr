import * as vscode from "vscode";
// Type-only import: erased at compile time, so the SDK is NOT loaded at startup.
// The require() below is lazy — only executed when telemetry is actually enabled.
import type TelemetryReporter from "@vscode/extension-telemetry";

// Full connection string format required by @vscode/extension-telemetry v0.9+.
// Substituted at publish time via CI secret APPLICATIONINSIGHTS_CONNECTION_STRING.
// Never commit a real connection string here.
const CONNECTION_STRING =
  process.env["APPLICATIONINSIGHTS_CONNECTION_STRING"] ??
  "InstrumentationKey=00000000-0000-0000-0000-000000000000";

export const EVT_ACTIVATED = "extension.activated";
export const EVT_MCP_INVOKED = "mcp.provider_invoked";
export const EVT_DAEMON_FAILED = "daemon.connect_failed";

export function createTelemetryReporter(
  enabled: boolean
): TelemetryReporter | null {
  if (!enabled || !vscode.env.isTelemetryEnabled) return null;
  // Lazy require so Application Insights is never loaded when disabled.
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const Ctor = (require("@vscode/extension-telemetry") as { default: typeof TelemetryReporter }).default;
  return new Ctor(CONNECTION_STRING);
}

export function sendEvent(
  reporter: TelemetryReporter | null,
  name: string,
  props?: Record<string, string>
): void {
  if (reporter === null) return;
  reporter.sendTelemetryEvent(name, props);
}
