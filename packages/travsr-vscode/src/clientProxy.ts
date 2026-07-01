/**
 * VSCODE-205: MutableMcpClientProxy — wraps a McpClient so it can be
 * hot-swapped after binary installation without reloading the window.
 *
 * All providers hold a reference to the proxy; after swap() they
 * transparently route through the new inner client without knowing it changed.
 */

import type { McpClient } from "./mcp";

export class MutableMcpClientProxy implements McpClient {
  private inner: McpClient;
  private readonly reconnectListeners = new Set<() => void>();
  private onInvoke?: (toolName: string) => void;

  constructor(inner: McpClient) {
    this.inner = inner;
  }

  setOnInvoke(cb: (toolName: string) => void): void {
    this.onInvoke = cb;
  }

  callTool(name: string, args?: Record<string, string>, signal?: AbortSignal): Promise<string> {
    try { this.onInvoke?.(name); } catch { /* telemetry hook must not abort tool calls */ }
    return this.inner.callTool(name, args, signal);
  }

  isConnected(): boolean {
    return this.inner.isConnected();
  }

  dispose(): void {
    this.reconnectListeners.clear();
    this.inner.dispose();
  }

  // Atomically replaces the inner client, then disposes the old one.
  // Fires reconnect listeners between swap and dispose so listeners see
  // the new client as active when they call isConnected() or callTool().
  swapAndDispose(newClient: McpClient): void {
    const old = this.inner;
    this.inner = newClient;
    for (const cb of this.reconnectListeners) cb();
    old.dispose();
  }

  onReconnect(cb: () => void): { dispose(): void } {
    this.reconnectListeners.add(cb);
    return { dispose: () => this.reconnectListeners.delete(cb) };
  }
}
