import * as assert from "assert";
import {
  buildSynonymsHtml,
  buildReposHtml,
  buildStatsHtml,
  highlightJson,
  looksLikeSourceRef,
  renderDetail,
} from "../../webviews";
import type { Diagnostic, LogEntry, StatsView } from "../../webviews";

// Shared fixtures. Values are arbitrary: they exist to drive each builder
// down a branch, and nothing asserts on them.
const STATS: StatsView = {
  nodes: "1",
  edges: "1",
  schemaVersion: "1",
  dbSize: "1 B",
  lastIndexed: "just now",
};
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


  // One labelled event, one repeat of it (the collapse path), one warning, and
  // one line that is not an event at all, so every branch of the panel renders.


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
  ];

  test("no panel builder emits a codicon, on any branch", () => {
    for (const [name, html] of panels()) {
      const hit = html.match(CODICON);
      assert.ok(
        hit === null,
        `${name} leaks codicon syntax into HTML: ${hit?.[0]}, drop it, a webview cannot render it`,
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
    // The quotes here are delimiters the highlighter writes itself, not user
    // data, so they stay literal. What matters is that the date is one string
    // token and did not shred into number spans; escaping of actual values is
    // pinned by the payload test below.
    assert.ok(html.includes('class="j-s">"2026-08-13"<'), "the date stays one string");
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

suite("every panel renders", () => {
  // The cheapest test here, and the one that would have caught the worst bug so
  // far. A stray backtick in a CSS comment closed the stylesheet template early,
  // the rest parsed as JavaScript, and webviewShell threw at render time: the
  // panel came back blank. It was valid TypeScript, so the typecheck passed and
  // nothing else noticed until it was installed and opened.
  //
  // Asserting only that each builder returns markup is enough to catch that
  // whole class, because the failure is a throw rather than a wrong string.
  test("no builder throws, and each returns a document", () => {
    const cases: Array<[string, () => string]> = [
      ["stats, offline", () => buildStatsHtml(STATS)],
      ["stats, all clear", () => buildStatsHtml(STATS, LOG)],
      ["stats, with diagnostics", () => buildStatsHtml(STATS, LOG, DIAGS)],
      ["synonyms", () => buildSynonymsHtml([{ term: "auth", alias: "login" }])],
      ["repos", () => buildReposHtml([{ name: "demo", path: "/tmp/d", exists: true }])],
    ];
    for (const [name, build] of cases) {
      let html = "";
      assert.doesNotThrow(() => {
        html = build();
      }, `${name} threw while rendering`);
      assert.ok(html.includes("<body>"), `${name} produced no document`);
      assert.ok(html.length > 1000, `${name} produced a suspiciously short document`);
    }
  });
});

suite("clickable file references", () => {
  // The log carries plenty of paths that lead nowhere: the repo root on every
  // repo-scoped line, a unix socket, a model directory. Linking those is a
  // cursor that changes shape and then does nothing, so only a `path` or `file`
  // field pointing at something with an extension is offered.
  test("only real source refs are offered", () => {
    const yes: Array<[string, string]> = [
      ["path", "src/generated/proto.ts"],
      ["path", "/Users/me/app/src/legacy.py"],
      ["file", "crates/travsr-daemon/src/lib.rs"],
    ];
    const no: Array<[string, string]> = [
      ["repo", "/Users/me/work/travsr"],
      ["sock", "/tmp/daemon-abc.sock"],
      ["model_dir", "/Users/me/.travsr/models/rerank"],
      ["path", "/Users/me/some/dir/"],
      ["commit", "b2a0013"],
    ];
    for (const [k, v] of yes) {
      assert.ok(looksLikeSourceRef(k, v), `${k}=${v} should be openable`);
    }
    for (const [k, v] of no) {
      assert.ok(!looksLikeSourceRef(k, v), `${k}=${v} should not be offered`);
    }
  });

  test("only the ref is marked up, and values stay escaped", () => {
    const html = renderDetail('path=src/legacy.py err="<b>SyntaxError</b>" repo=/Users/me/app');
    assert.ok(html.includes('data-path="src/legacy.py"'), "the source file is openable");
    assert.ok(!html.includes('data-path="/Users/me/app"'), "the repo root is not");
    assert.ok(!html.includes("<b>"), `error text must stay escaped: ${html}`);
  });
});

suite("activity feed colouring", () => {
  const evt = (event: string, level: string, detail: string): LogEntry => ({
    time: "10:00:01",
    level,
    target: "travsr_daemon",
    message: event,
    event,
    detail,
    iso: "2026-08-14T10:00:01Z",
    raw: "{}",
  });
  const STATS = {
    nodes: "4,102",
    edges: "9,881",
    schemaVersion: "22",
    dbSize: "12 MB",
    lastIndexed: "just now",
  };
  /** Families on the rows only. The CSS carries the same attribute. */
  const rowFamilies = (html: string): string[] =>
    [...html.matchAll(/<tr class="lvl-[A-Z]+" data-fam="([a-z]+)"/g)].map((m) => m[1]);

  test("each lifecycle event lands in the family that emits it", () => {
    const html = buildStatsHtml(STATS, [
      evt("daemon.session.start", "INFO", "version=0.7.0"),
      evt("head.drift.detected", "INFO", "from=b2a0013"),
      evt("phase_b.start", "INFO", "langs=4"),
      evt("embed.text.updated", "INFO", "written=18"),
      evt("kcore.updated", "INFO", ""),
      evt("query.failed", "ERROR", 'err="no such symbol"'),
    ]);
    // Newest first, so the feed reads back to front.
    assert.deepStrictEqual(rowFamilies(html), [
      "query",
      "index",
      "search",
      "index",
      "git",
      "daemon",
    ]);
  });

  test("an event with no family still renders, rather than dropping out", () => {
    // Only labelled events reach the feed, so this pins the fallback for a
    // label added without a matching family.
    const html = buildStatsHtml(STATS, [evt("lsif.spawn", "INFO", "lang=rust")]);
    const fams = rowFamilies(html);
    assert.strictEqual(fams.length, 1, `one row expected, got ${fams.length}`);
    assert.ok(fams[0].length > 0, "the row carries some family");
  });

  test("severity beats family, and comes after it so the cascade agrees", () => {
    const html = buildStatsHtml(STATS, [evt("phase_b.complete", "ERROR", "")]);
    const family = html.indexOf('.activity tr[data-fam="query"]');
    const severity = html.indexOf(".activity tr.lvl-ERROR .fam-dot");
    assert.ok(family > 0 && severity > 0, "both rules are present");
    // Equal specificity (0,2,1), so only source order decides the winner.
    assert.ok(
      severity > family,
      "severity must be declared after family or a failing row keeps its family colour"
    );
  });

  test("a run of one event collapses into a count, not repeated rows", () => {
    const html = buildStatsHtml(STATS, [
      evt("embed.text.updated", "INFO", "written=18"),
      evt("embed.text.updated", "INFO", "written=4"),
      evt("embed.text.updated", "INFO", "written=2"),
    ]);
    assert.strictEqual(rowFamilies(html).length, 1, "three ticks are one row");
    assert.ok(html.includes('<span class="run">&times;3</span>'), "and it says three");
  });

  test("the light palette follows the editor theme, not the desktop", () => {
    // Reported from a machine with a light Windows and a dark VS Code theme: the
    // panel rendered linen inside a dark editor and switching themes did
    // nothing. The palette was behind `@media (prefers-color-scheme: light)`,
    // which in an Electron window resolves from the OS appearance rather than
    // from the editor theme, so the panel was reporting the desktop.
    //
    // Asserted on the rule rather than the string, because the comment that
    // explains this ships inside the stylesheet and names it.
    const html = buildStatsHtml(STATS, []);
    assert.ok(
      !/@media\s*\(\s*prefers-color-scheme/.test(html),
      "the palette must not be keyed on the OS appearance"
    );
    assert.ok(html.includes("body.vscode-light"), "keyed on the editor theme kind");
    assert.ok(
      html.includes("body.vscode-high-contrast-light"),
      "high-contrast light is a light kind of its own and vscode-light misses it"
    );
    // prefers-reduced-motion is genuinely an OS preference and stays one.
    assert.ok(/@media\s*\(\s*prefers-reduced-motion/.test(html));
  });

  test("family hues are defined for both themes", () => {
    const html = buildStatsHtml(STATS, [evt("phase_b.start", "INFO", "")]);
    for (const token of ["--green", "--orange", "--gold"]) {
      const defined = (html.match(new RegExp(`${token}:`, "g")) ?? []).length;
      assert.ok(defined >= 2, `${token} must be defined in light and dark, saw ${defined}`);
    }
  });

  test("a path in an activity detail is openable too", () => {
    const html = buildStatsHtml(STATS, [
      evt("phase_b.complete", "WARN", 'path=src/legacy.py err="parse error"'),
    ]);
    assert.ok(html.includes('data-path="src/legacy.py"'), "the file is offered");
  });
});

suite("log panel counts what it renders", () => {
  const line = (i: number, level = "INFO"): LogEntry => ({
    time: "10:00:00",
    level,
    target: "travsr_daemon",
    message: `line ${i}`,
    detail: "",
    iso: "2026-08-14T10:00:00Z",
    raw: "{}",
  });
  const STATS = {
    nodes: "1",
    edges: "1",
    schemaVersion: "22",
    dbSize: "1 MB",
    lastIndexed: "now",
  };

  test("a log past the old 200 cap renders every line it counts", () => {
    // The reader returns up to 500. The panel used to render 200 while the
    // header counted the whole array, so it claimed 342 lines with 200 in the
    // DOM, and the 500 option could never show more than 200.
    const log = Array.from({ length: 342 }, (_, i) => line(i));
    const html = buildStatsHtml(STATS, log);
    const rows = (html.match(/class="log-line /g) ?? []).length;
    assert.strictEqual(rows, 342, `header says 342, DOM has ${rows}`);
    assert.ok(html.includes("342 lines"), "and the header agrees");
  });

  test("severity chips count the same lines that are rendered", () => {
    const log = [
      ...Array.from({ length: 250 }, (_, i) => line(i)),
      line(998, "WARN"),
      line(999, "ERROR"),
    ];
    const html = buildStatsHtml(STATS, log);
    const rows = (html.match(/class="log-line /g) ?? []).length;
    assert.strictEqual(rows, 252);
    // Both the warn and the error line are past the old cap, so under the old
    // behaviour the chips promised rows the DOM did not have.
    assert.ok(html.includes('data-rank="3"'), "the warning row is present");
    assert.ok(html.includes('data-rank="4"'), "the error row is present");
  });

  test("rendering does not reorder the caller's array", () => {
    const log = [line(1), line(2), line(3)];
    const before = log.map((e) => e.message);
    buildStatsHtml(STATS, log);
    assert.deepStrictEqual(
      log.map((e) => e.message),
      before,
      "reverse() must run on a copy"
    );
  });
});
