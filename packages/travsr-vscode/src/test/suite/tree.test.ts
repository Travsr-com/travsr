import * as assert from "assert";
import * as vscode from "vscode";
import { TravsrTreeDataProvider } from "../../tree";

// Minimal ExtensionContext stub — only the fields tree.ts uses.
function makeContext(): vscode.ExtensionContext {
  return {
    subscriptions: [],
    globalState: {
      get: () => undefined,
      update: () => Promise.resolve(),
      keys: () => [],
      setKeysForSync: () => undefined,
    },
  } as unknown as vscode.ExtensionContext;
}

// Controlled McpClient stub.
function makeMcp(
  responses: Record<string, string> = {}
): import("../../mcp").McpClient {
  return {
    callTool: async (name: string) => responses[name] ?? "",
    isConnected: () => true,
    dispose: () => undefined,
  };
}

// Helper: set private field for symbol (simulates cursor move).
function setSymbol(
  provider: TravsrTreeDataProvider,
  symbol: string | undefined
): void {
  (provider as unknown as Record<string, unknown>).activeSymbol = symbol;
  (provider as unknown as Record<string, unknown>).callerCache = undefined;
}

// Helper: set private field for active file.
function setFile(
  provider: TravsrTreeDataProvider,
  file: string | undefined
): void {
  (provider as unknown as Record<string, unknown>).activeFile = file;
  (provider as unknown as Record<string, unknown>).depCache = undefined;
}

// ── suite ──────────────────────────────────────────────────────────────────

suite("VSCODE-204: TravsrTreeDataProvider — root", () => {
  test("getChildren(undefined) returns deps and callers sections", async () => {
    const provider = new TravsrTreeDataProvider(makeMcp(), makeContext());
    const children = await provider.getChildren(undefined);
    assert.strictEqual(children.length, 2);
    const labels = children.map((c) => (c.label as string));
    assert.ok(labels.includes("Dependencies"));
    assert.ok(labels.includes("Callers"));
  });

  test("sections are expanded by default", async () => {
    const provider = new TravsrTreeDataProvider(makeMcp(), makeContext());
    const [deps, callers] = await provider.getChildren(undefined);
    assert.strictEqual(
      deps.collapsibleState,
      vscode.TreeItemCollapsibleState.Expanded
    );
    assert.strictEqual(
      callers.collapsibleState,
      vscode.TreeItemCollapsibleState.Expanded
    );
  });
});

suite("VSCODE-204: TravsrTreeDataProvider — Dependencies section", () => {
  test("returns placeholder when no active file", async () => {
    const provider = new TravsrTreeDataProvider(makeMcp(), makeContext());
    setFile(provider, undefined);
    const [depsSection] = await provider.getChildren(undefined);
    const children = await provider.getChildren(depsSection);
    assert.strictEqual(children.length, 1);
    assert.ok((children[0].label as string).includes("No active file"));
  });

  test("returns placeholder when get_dependencies returns empty", async () => {
    const provider = new TravsrTreeDataProvider(
      makeMcp({ get_dependencies: "" }),
      makeContext()
    );
    setFile(provider, "src/foo.ts");
    const [depsSection] = await provider.getChildren(undefined);
    const children = await provider.getChildren(depsSection);
    assert.strictEqual(children.length, 1);
    assert.ok((children[0].label as string).includes("No dependencies found"));
  });

  test("parses import:./mcp style lines into entry nodes", async () => {
    const provider = new TravsrTreeDataProvider(
      makeMcp({ get_dependencies: "import:./mcp\nimport:./status\n" }),
      makeContext()
    );
    setFile(provider, "src/extension.ts");
    const [depsSection] = await provider.getChildren(undefined);
    const children = await provider.getChildren(depsSection);
    assert.strictEqual(children.length, 2);
    assert.strictEqual(children[0].label, "./mcp");
    assert.strictEqual(children[0].description, "import");
    assert.strictEqual(children[1].label, "./status");
  });

  test("local dep entry has open-file command", async () => {
    const provider = new TravsrTreeDataProvider(
      makeMcp({ get_dependencies: "import:./mcp\n" }),
      makeContext()
    );
    setFile(provider, "src/extension.ts");
    const [depsSection] = await provider.getChildren(undefined);
    const [entry] = await provider.getChildren(depsSection);
    assert.ok(entry.command !== undefined, "local dep must have a command");
    assert.strictEqual(entry.command!.command, "vscode.open");
  });

  test("external dep (vscode, node module) has no open-file command", async () => {
    const provider = new TravsrTreeDataProvider(
      makeMcp({ get_dependencies: "import:vscode\n" }),
      makeContext()
    );
    setFile(provider, "src/extension.ts");
    const [depsSection] = await provider.getChildren(undefined);
    const [entry] = await provider.getChildren(depsSection);
    assert.strictEqual(entry.command, undefined);
  });

  test("dep cache is reused on second getChildren call", async () => {
    let callCount = 0;
    const mcp: import("../../mcp").McpClient = {
      callTool: async () => { callCount++; return "import:./mcp\n"; },
      isConnected: () => true,
      dispose: () => undefined,
    };
    const provider = new TravsrTreeDataProvider(mcp, makeContext());
    setFile(provider, "src/extension.ts");
    const [depsSection] = await provider.getChildren(undefined);
    await provider.getChildren(depsSection);
    await provider.getChildren(depsSection); // second call must use cache
    assert.strictEqual(callCount, 1, "MCP must be called exactly once");
  });

  test("refresh() clears dep cache so next call re-fetches", async () => {
    let callCount = 0;
    const mcp: import("../../mcp").McpClient = {
      callTool: async () => { callCount++; return "import:./mcp\n"; },
      isConnected: () => true,
      dispose: () => undefined,
    };
    const provider = new TravsrTreeDataProvider(mcp, makeContext());
    setFile(provider, "src/extension.ts");
    const [depsSection] = await provider.getChildren(undefined);
    await provider.getChildren(depsSection);
    provider.refresh();
    await provider.getChildren(depsSection);
    assert.strictEqual(callCount, 2, "MCP must be called again after refresh");
  });
});

suite("VSCODE-204: TravsrTreeDataProvider — Callers section", () => {
  test("returns placeholder when no active symbol", async () => {
    const provider = new TravsrTreeDataProvider(makeMcp(), makeContext());
    setSymbol(provider, undefined);
    const [, callersSection] = await provider.getChildren(undefined);
    const children = await provider.getChildren(callersSection);
    assert.strictEqual(children.length, 1);
    assert.ok(
      (children[0].label as string).includes("Move cursor to a symbol")
    );
  });

  test("returns placeholder when get_callers returns empty", async () => {
    const provider = new TravsrTreeDataProvider(
      makeMcp({ get_callers: "" }),
      makeContext()
    );
    setSymbol(provider, "foo");
    const [, callersSection] = await provider.getChildren(undefined);
    const children = await provider.getChildren(callersSection);
    assert.strictEqual(children.length, 1);
    assert.ok((children[0].label as string).includes("No callers found"));
  });

  test("parses [call] fn:bar — src/bar.ts format correctly", async () => {
    const provider = new TravsrTreeDataProvider(
      makeMcp({
        get_callers:
          "[call] fn:bar (function) — src/bar.ts\n[structural] class:Foo (class) — src/foo.ts\n",
      }),
      makeContext()
    );
    setSymbol(provider, "myFn");
    const [, callersSection] = await provider.getChildren(undefined);
    const children = await provider.getChildren(callersSection);
    assert.strictEqual(children.length, 2);
    assert.strictEqual(children[0].label, "fn:bar");
    assert.strictEqual(children[1].label, "class:Foo");
  });

  test("unrecognised caller line falls back to raw label", async () => {
    const provider = new TravsrTreeDataProvider(
      makeMcp({ get_callers: "some unexpected format\n" }),
      makeContext()
    );
    setSymbol(provider, "foo");
    const [, callersSection] = await provider.getChildren(undefined);
    const [entry] = await provider.getChildren(callersSection);
    assert.strictEqual(entry.label, "some unexpected format");
  });

  test("caller entries with a file path have open-file command", async () => {
    const provider = new TravsrTreeDataProvider(
      makeMcp({ get_callers: "[call] fn:bar (function) — src/bar.ts\n" }),
      makeContext()
    );
    setSymbol(provider, "foo");
    const [, callersSection] = await provider.getChildren(undefined);
    const [entry] = await provider.getChildren(callersSection);
    assert.ok(entry.command !== undefined);
    assert.strictEqual(entry.command!.command, "vscode.open");
  });

  test("caller cache is reused on second getChildren call", async () => {
    let callCount = 0;
    const mcp: import("../../mcp").McpClient = {
      callTool: async () => { callCount++; return "[call] fn:x (function) — src/x.ts\n"; },
      isConnected: () => true,
      dispose: () => undefined,
    };
    const provider = new TravsrTreeDataProvider(mcp, makeContext());
    setSymbol(provider, "foo");
    const [, callersSection] = await provider.getChildren(undefined);
    await provider.getChildren(callersSection);
    await provider.getChildren(callersSection);
    assert.strictEqual(callCount, 1, "MCP must be called exactly once");
  });
});

suite("VSCODE-204: TravsrTreeDataProvider — edge cases", () => {
  test("dep line with space after colon still gets open-file command (dep is trimmed)", async () => {
    const provider = new TravsrTreeDataProvider(
      makeMcp({ get_dependencies: "import: ./mcp\n" }),
      makeContext()
    );
    setFile(provider, "src/extension.ts");
    const [depsSection] = await provider.getChildren(undefined);
    const [entry] = await provider.getChildren(depsSection);
    assert.strictEqual(entry.label, "./mcp", "dep must be trimmed");
    assert.ok(entry.command !== undefined, "trimmed local dep must still get open-file command");
  });

  test("dep line with scoped package kind is parsed correctly", async () => {
    const provider = new TravsrTreeDataProvider(
      makeMcp({ get_dependencies: "import:@scope/package\n" }),
      makeContext()
    );
    setFile(provider, "src/extension.ts");
    const [depsSection] = await provider.getChildren(undefined);
    const [entry] = await provider.getChildren(depsSection);
    assert.strictEqual(entry.label, "@scope/package");
    assert.strictEqual(entry.description, "import");
    assert.strictEqual(entry.command, undefined, "scoped package must not get open-file command");
  });

  test("MCP throwing on get_dependencies returns placeholder, not crash", async () => {
    const mcp: import("../../mcp").McpClient = {
      callTool: async () => { throw new Error("daemon offline"); },
      isConnected: () => false,
      dispose: () => undefined,
    };
    const provider = new TravsrTreeDataProvider(mcp, makeContext());
    setFile(provider, "src/extension.ts");
    const [depsSection] = await provider.getChildren(undefined);
    const children = await provider.getChildren(depsSection);
    assert.strictEqual(children.length, 1);
    assert.ok((children[0].label as string).includes("No dependencies found"));
  });

  test("MCP throwing on get_callers returns placeholder, not crash", async () => {
    const mcp: import("../../mcp").McpClient = {
      callTool: async () => { throw new Error("daemon offline"); },
      isConnected: () => false,
      dispose: () => undefined,
    };
    const provider = new TravsrTreeDataProvider(mcp, makeContext());
    setSymbol(provider, "myFn");
    const [, callersSection] = await provider.getChildren(undefined);
    const children = await provider.getChildren(callersSection);
    assert.strictEqual(children.length, 1);
    assert.ok((children[0].label as string).includes("No callers found"));
  });
});

suite("VSCODE-204: TravsrTreeDataProvider — refresh", () => {
  test("refresh() fires onDidChangeTreeData", (done) => {
    const provider = new TravsrTreeDataProvider(makeMcp(), makeContext());
    provider.onDidChangeTreeData(() => done());
    provider.refresh();
  });

  test("getTreeItem returns the element unchanged", async () => {
    const provider = new TravsrTreeDataProvider(makeMcp(), makeContext());
    const [section] = await provider.getChildren(undefined);
    assert.strictEqual(provider.getTreeItem(section), section);
  });
});
