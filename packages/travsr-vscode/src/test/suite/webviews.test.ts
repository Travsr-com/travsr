import * as assert from "assert";
import { buildSynonymsHtml, buildReposHtml, buildStatsHtml, buildLanguagesHtml } from "../../webviews";
import { highlightJson } from "../../webviews";
import type { Diagnostic, LangCount, LangInfo, LogEntry, StatsView } from "../../webviews";

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
    // rust: builtin + installed → active → enabled badge
    assert.ok(html.includes(">enabled<"), "enabled badge for active language");
    // java/scala: not registered → disabled badge
    assert.ok(html.includes(">disabled<"), "disabled badge for inactive language");
    assert.ok(html.includes("Semantic analysis"), "semantic tooltip present");
    assert.ok(html.includes("Semantic"), "Semantic column header present");
  });
  test("detects empty indexed section", () => {
    const html = buildLanguagesHtml([], []);
    assert.ok(html.includes("No language metadata"));
    assert.ok(html.includes('id="initBtn"'), "Initialize button element present in empty state");
    assert.ok(html.includes('onclick="initRepo(this)"'), "Initialize button onclick wired");
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

suite("codicon syntax never reaches webview HTML", () => {
  // `$(icon-name)` is VS Code's codicon syntax. It renders in the status bar,
  // in QuickPick labels and on tree items, and NOT in a webview, where it is
  // shown to the user verbatim: the Graph stats panel read
  // "$(graph) Graph stats" as a literal heading.
  //
  // The hazard is easy to reintroduce, because the same label string is often
  // right for a QuickPick and wrong for a panel built from it. `commands.ts`
  // already strips the prefix for exactly this reason. This asserts the panels
  // never carry it in the first place.
  const CODICON = /\$\([a-z-]+\)/;

  // Field values are arbitrary. Nothing here asserts on them: they exist only
  // to drive each builder down a branch. What matters is that every branch that
  // emits markup is rendered at least once, since a codicon added to a row the
  // fixtures never build is a codicon this test cannot see.
  const LANG: LangInfo = {
    language: "rust",
    package: "@travsr-plugin/rust",
    sandbox: "Standard",
    installed: true,
    registered: true,
    builtin: false,
    needsApproval: false,
    scipInstallType: "GithubBinary",
    installHint: "travsr lang install rust",
    underlyingToolHint: "",
    elevatedHosts: [],
  };

  const STATS: StatsView = {
    nodes: "1",
    edges: "1",
    schemaVersion: "1",
    dbSize: "1 B",
    lastIndexed: "just now",
  };

  // One labelled event, one repeat of it (the collapse path), one warning, and
  // one line that is not an event at all, so every branch of the panel renders.
  const LOG: LogEntry[] = [
    { time: "01:00:00", level: "INFO", target: "daemon", message: "started", event: "daemon.ready", detail: "pid=1", iso: "2026-08-14T01:00:00Z", raw: "{}" },
    { time: "01:00:01", level: "INFO", target: "daemon", message: "indexed", event: "phase_b.indexed", detail: "nodes=1", iso: "2026-08-14T01:00:01Z", raw: "{}" },
    { time: "01:00:02", level: "INFO", target: "daemon", message: "indexed", event: "phase_b.indexed", detail: "nodes=2", iso: "2026-08-14T01:00:02Z", raw: "{}" },
    { time: "01:00:03", level: "WARN", target: "plugin-host", message: "analyzer missing", detail: "lang=go", iso: "2026-08-14T01:00:03Z", raw: "{}" },
    { time: "01:00:04", level: "ERROR", target: "indexer", message: "lsif failed", detail: "code=1", iso: "2026-08-14T01:00:04Z", raw: "{}" },
    { time: "", level: "", target: "", message: "a line from before the log became JSON", detail: "", iso: "", raw: "{}" },
  ];

  const DIAGS: Diagnostic[] = [
    {
      severity: "error",
      title: "semantic analyzer for 'kotlin' crashed",
      hint: "semantic analyzer for 'kotlin' crashed, re-run to retry",
      command: "travsr init --semantic",
    },
    // No command: the card must render without an action row.
    { severity: "warn", title: "index looks stale", hint: "HEAD moved since the last index" },
  ];

  const panels = (): Array<[string, string]> => [
    // Populated and empty are separate code paths in several builders, so both
    // are rendered rather than whichever one the fixture happened to hit.
    ["synonyms, populated", buildSynonymsHtml([{ term: "auth", alias: "login" }])],
    ["synonyms, empty", buildSynonymsHtml([])],
    ["repos, populated", buildReposHtml([{ name: "demo", path: "/tmp/demo/.travsr/graph.db", exists: true }])],
    ["repos, with a stale row", buildReposHtml([{ name: "gone", path: "/tmp/gone/.travsr/graph.db", exists: false }])],
    ["repos, empty", buildReposHtml([])],
    ["stats, offline", buildStatsHtml(STATS)],
    ["stats, all clear", buildStatsHtml(STATS, LOG)],
    ["stats, with diagnostics", buildStatsHtml(STATS, LOG, DIAGS)],
    ["languages, indexed and available", buildLanguagesHtml([{ language: "rust", count: 1 }], [LANG])],
    ["languages, available but not indexed", buildLanguagesHtml([], [LANG])],
    ["languages, empty", buildLanguagesHtml([], [])],
  ];

  test("no panel builder emits a codicon, on any branch", () => {
    for (const [name, html] of panels()) {
      const hit = html.match(CODICON);
      assert.ok(
        hit === null,
        `${name} leaks codicon syntax into HTML: ${hit?.[0]} — drop it, a webview cannot render it`,
      );
    }
  });
});

suite("JSON view", () => {
  // The first version ran chained regexes over the text, and a pass for numbers
  // matched the digits inside an already-marked timestamp string, nesting the
  // spans. Walking the parsed value cannot do that, and this pins it.
  test("tokens are not nested inside one another", () => {
    const line = JSON.stringify({
      timestamp: "2026-08-13T21:12:16.186214Z",
      level: "INFO",
      fields: { message: "semantic indexing complete", nodes: 89, ok: true },
    });
    const html = highlightJson(line);
    assert.ok(
      !/class="j-[a-z]"[^>]*>[^<]*<span/.test(html),
      `a token was rendered inside another token: ${html}`
    );
  });

  test("each kind of value gets its own class, and strings keep their digits", () => {
    const html = highlightJson(JSON.stringify({ n: 89, s: "2026-08-13", b: true, z: null }));
    assert.ok(html.includes('class="j-n">89'), "number");
    assert.ok(html.includes('class="j-s">&quot;2026-08-13&quot;'), "the date stays one string");
    assert.ok(html.includes('class="j-b">true'), "boolean");
    assert.ok(html.includes('class="j-b">null'), "null");
  });

  test("a line that is not JSON is shown as itself", () => {
    // Rotations written before the log became JSON are still on disk.
    const legacy = "2026-08-12T13:31:02Z  WARN travsr_daemon: something happened";
    const html = highlightJson(legacy);
    assert.ok(html.includes("j-raw"), "should fall back rather than throw");
    assert.ok(html.includes("something happened"), "and must not lose the line");
  });

  test("markup in a value cannot escape into the page", () => {
    const html = highlightJson(JSON.stringify({ msg: "<img src=x onerror=alert(1)>" }));
    assert.ok(!html.includes("<img"), `unescaped markup reached the output: ${html}`);
    assert.ok(html.includes("&lt;img"), "the value should still be readable, escaped");
  });
});
