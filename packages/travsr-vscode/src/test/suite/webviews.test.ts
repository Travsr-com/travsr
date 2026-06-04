import * as assert from "assert";
import { buildSynonymsHtml, buildReposHtml, buildStatsHtml, buildLanguagesHtml } from "../../webviews";
import type { LangCount, LangInfo } from "../../webviews";

suite("VSCODE-247: buildSynonymsHtml", () => {
  test("groups aliases by term and renders chips + staged multi-add row", () => {
    const html = buildSynonymsHtml([
      { term: "auth", alias: "login" },
      { term: "auth", alias: "session" },
      { term: "db", alias: "store" },
    ]);
    assert.ok(html.includes("acquireVsCodeApi"));
    assert.ok(html.includes("login") && html.includes("session") && html.includes("store"));
    assert.ok(html.includes("commitAdd"), "must wire the staged-add action");
    assert.ok(html.includes("addBatch"), "must post addBatch message");
    assert.ok(html.includes("removePair") && html.includes("removeTerm"));
    assert.ok(html.includes("Reset to defaults"));
    assert.ok(html.includes("chip-area"), "staging area for chips must exist");
    // auth appears once as a grouped row, not once per alias
    assert.strictEqual((html.match(/>auth</g) ?? []).length, 1);
  });
  test("empty list shows a placeholder", () => {
    const html = buildSynonymsHtml([]);
    assert.ok(html.includes("No synonyms defined"));
  });
  test("escapes HTML in term/alias", () => {
    const html = buildSynonymsHtml([{ term: "<x>", alias: "a&b" }]);
    assert.ok(html.includes("&lt;x&gt;") && html.includes("a&amp;b"));
    assert.ok(!html.includes("<x>"));
  });
});

suite("VSCODE-247: buildReposHtml", () => {
  test("renders status badges, prune button with count, and remove", () => {
    const html = buildReposHtml([
      { name: "live", path: "/a/.travsr/graph.db", exists: true },
      { name: "dead", path: "/tmp/x/.travsr/graph.db", exists: false },
    ]);
    assert.ok(html.includes("Prune stale (1)"), "stale count in prune button");
    assert.ok(html.includes("badge ok") && html.includes("badge stale"));
    assert.ok(html.includes("removeRepo") && html.includes("function prune"));
    assert.ok(html.includes("live") && html.includes("dead"));
  });
  test("empty registry shows a placeholder and zero prune count", () => {
    const html = buildReposHtml([]);
    assert.ok(html.includes("No repos registered"));
    assert.ok(html.includes("Prune stale (0)"));
  });
});

suite("VSCODE-247: buildLanguagesHtml", () => {
  const indexed: LangCount[] = [
    { language: "typescript", count: 3200 },
    { language: "rust", count: 840 },
  ];
  const available: LangInfo[] = [
    {
      language: "rust", package: "scip-rust", sandbox: "Standard",
      installed: true, registered: true, builtin: true, needsApproval: false,
      scipInstallType: "Command", installHint: "travsr lang install rust",
      underlyingToolHint: "", elevatedHosts: [],
    },
    {
      language: "java", package: "scip-java", sandbox: "Elevated",
      installed: false, registered: false, builtin: false, needsApproval: true,
      scipInstallType: "GithubBinary", installHint: "travsr lang install java",
      underlyingToolHint: "", elevatedHosts: ["repo1.maven.org"],
    },
    {
      language: "scala", package: "scip-scala", sandbox: "Elevated",
      installed: false, registered: false, builtin: false, needsApproval: false,
      scipInstallType: "Manual", installHint: "travsr lang install scala",
      underlyingToolHint: "https://docs.scala-lang.org/scip", elevatedHosts: [],
    },
  ];

  test("renders indexed section with node counts", () => {
    const html = buildLanguagesHtml(indexed, []);
    assert.ok(html.includes("typescript") && html.includes("rust"));
    assert.ok(html.includes("3,200") || html.includes("3200"), "node count visible");
    assert.ok(html.includes("Indexed in this repo"));
  });
  test("renders available tools with correct action cells", () => {
    const html = buildLanguagesHtml([], available);
    // rust: registered+installed+builtin → Built-in badge (no Disable button for builtins)
    assert.ok(html.includes("Built-in"), "builtin shows Built-in badge");
    assert.ok(!html.includes('onclick="removeLang'), "builtin has no Disable onclick");
    // java: needsApproval → consent form (inside not-here disclosure when undetected)
    assert.ok(html.includes("approveLang") && html.includes("Grant"));
    assert.ok(html.includes("repo1.maven.org"), "elevated hosts pre-filled");
    // scala: Manual → install guide link (inside not-here disclosure when undetected)
    assert.ok(html.includes("docs.scala-lang.org") || html.includes("Install guide"));
  });
  test("undetected+inactive languages are gated behind not-here disclosure", () => {
    // java and scala are not in indexed → get <details class="not-here"> wrapper
    const html = buildLanguagesHtml([], available);
    assert.ok(html.includes('<details class="not-here">'), "disclosure element present for undetected languages");
  });
  test("detected language gets direct action, not gated", () => {
    // rust is in indexed → Install button should appear directly, no not-here disclosure
    const indexedWithRust: LangCount[] = [{ language: "rust", count: 10 }];
    const uninstalledRust: LangInfo[] = [{
      language: "rust", package: "scip-rust", sandbox: "Standard",
      installed: false, registered: false, builtin: false, needsApproval: false,
      scipInstallType: "Command", installHint: "travsr lang install rust",
      underlyingToolHint: "", elevatedHosts: [],
    }];
    const html = buildLanguagesHtml(indexedWithRust, uninstalledRust);
    assert.ok(html.includes("installLang") && html.includes("Install"));
    assert.ok(!html.includes('<details class="not-here">'), "detected language skips the disclosure gate");
  });
  test("semantic badge reflects Phase B registration state", () => {
    const html = buildLanguagesHtml(indexed, available);
    // rust: active → enabled badge
    assert.ok(html.includes(">enabled<"), "enabled badge for active language");
    // java/scala: not registered → disabled badge
    assert.ok(html.includes(">disabled<"), "disabled badge for inactive language");
    assert.ok(html.includes("Semantic analysis"), "semantic tooltip present");
    assert.ok(html.includes("Semantic"), "Semantic column header present");
  });
  test("detects empty indexed section", () => {
    const html = buildLanguagesHtml([], []);
    assert.ok(html.includes("No language metadata"));
  });
  test("detect and refresh buttons present", () => {
    const html = buildLanguagesHtml(indexed, available);
    assert.ok(html.includes("detectLangs") && html.includes("Detect"));
    assert.ok(html.includes("doRefresh") && html.includes("Refresh"));
  });
  test("acquireVsCodeApi bridge wired", () => {
    const html = buildLanguagesHtml(indexed, available);
    assert.ok(html.includes("acquireVsCodeApi"));
  });
});

suite("VSCODE-247: buildStatsHtml", () => {
  test("renders metric cards", () => {
    const html = buildStatsHtml({
      nodes: "3,623",
      edges: "3,691",
      schemaVersion: "11",
      dbSize: "9.1 MB",
      lastIndexed: "2m ago",
    });
    assert.ok(html.includes("3,623") && html.includes("3,691"));
    assert.ok(html.includes("11") && html.includes("9.1 MB") && html.includes("2m ago"));
    assert.ok(html.includes("Schema") && html.includes("Last indexed"));
    assert.ok(html.includes("doRefresh"));
  });
});
