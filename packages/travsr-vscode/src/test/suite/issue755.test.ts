import * as assert from "assert";
import {
  langContractSkew,
  parseLangList,
  parseAvailableLanguages,
  contractSkewMessage,
  buildLanguageRows,
  parseRepoLanguages,
} from "../../commands";
import {
  buildStatsHtml,
  EMPTY_HEALTH,
  UNKNOWN_INDEX,
  LANG_CONTRACT_FIELDS,
  LANG_CONTRACT_VERSION,
} from "../../webviews";
import type { HealthData, LangInfo, StatsView } from "../../webviews";

/**
 * #755 Part A — the Languages panel rendered a stale binary's `lang list --json`
 * as a table of silently wrong cells: `status` absent fell back to "partial",
 * `repoState` absent interpolated the literal string "undefined", and
 * `prerequisites` absent read as "—" (the value that means "this analyzer needs
 * nothing"). Nothing anywhere said the binary was old.
 *
 * The fixtures below are the real key sets from the issue, so these tests fail
 * for the same reason a user's panel did.
 */

/** Exactly the first-entry keys the stale npm-bundled `1.0.0+8b9af8f` emitted. */
const STALE_ROW = {
  availableOnThisPlatform: true,
  builtin: false,
  elevatedHosts: [],
  installHint: "travsr lang install go",
  installed: true,
  language: "go",
  needsApproval: false,
  package: "@travsr-plugin/go",
  registered: true,
  sandbox: "Standard",
  scipInstallType: "Command",
  unavailableTarget: null,
  underlyingToolHint: "",
};

/** The same row from a current binary: the four extra fields plus the marker. */
const CURRENT_ROW = {
  ...STALE_ROW,
  contract: 1,
  status: "active",
  statusLine: "'go' is active — full cross-file analysis is on.",
  repoState: "enabled",
  prerequisites: "Go toolchain",
};

/** A well-formed row, for the mixed-payload cases. */
const GOOD: LangInfo = CURRENT_ROW as unknown as LangInfo;

const STATS: StatsView = { nodes: "1", edges: "1", schemaVersion: "1", dbSize: "1 B", lastIndexed: "now" };

/** Render the Health page with just these language rows, the surface that
 *  replaced the Languages panel. */
function healthWith(languages: HealthData["languages"], skew?: HealthData["languagesSkew"]): string {
  const health: HealthData = { ...EMPTY_HEALTH, languages, ...(skew ? { languagesSkew: skew } : {}) };
  return buildStatsHtml(STATS, [], [], 500, undefined, 0, UNKNOWN_INDEX, health);
}

suite("#755 Part A: lang list --json shape validation", () => {
  test("a current payload reports no skew and states its contract revision", () => {
    const got = parseLangList(JSON.stringify([CURRENT_ROW]));
    assert.deepStrictEqual(got.missingFields, []);
    assert.strictEqual(got.reportedContract, 1);
    assert.strictEqual(got.langs.length, 1);
  });

  test("the stale npm payload is reported as missing exactly the four panel fields", () => {
    const got = parseLangList(JSON.stringify([STALE_ROW, STALE_ROW]));
    assert.deepStrictEqual(
      got.missingFields.slice().sort(),
      ["prerequisites", "repoState", "status", "statusLine"],
      "these are the four fields the Languages table reads and cannot re-derive"
    );
    assert.strictEqual(
      got.reportedContract,
      undefined,
      "a binary that predates the marker reports no revision at all"
    );
    assert.strictEqual(
      got.langs.length,
      2,
      "the rows are still returned — the caller decides what to do with them"
    );
  });

  test("an empty catalog is not a skew", () => {
    const got = parseLangList("[]");
    assert.deepStrictEqual(got.missingFields, []);
    assert.deepStrictEqual(got.langs, []);
  });

  test("a payload prefixed by a stderr line would parse as no-skew, so stdout is isolated", () => {
    // #755 review: `spawnLangCommandResult` used to fold stderr into the same
    // buffer, so one `tracing` line (RUST_LOG set) landed in front of the JSON.
    // This is what the parser sees in that case — no rows AND no skew, i.e. a
    // clean bill of health for a binary that was never actually checked.
    const polluted = `INFO travsr_cli: resolving analyzers\n${JSON.stringify([STALE_ROW])}`;
    const got = parseLangList(polluted);
    assert.deepStrictEqual(got.langs, [], "the parse fails, as it must");
    assert.deepStrictEqual(
      got.missingFields,
      [],
      "and reports no skew — which is why the caller must pass stdout alone"
    );
    // The same bytes on stdout alone are read correctly, skew and all.
    const clean = parseLangList(JSON.stringify([STALE_ROW]));
    assert.deepStrictEqual(clean.missingFields.slice().sort(), [
      "prerequisites",
      "repoState",
      "status",
      "statusLine",
    ]);
  });

  test("a non-JSON error blob is not a skew", () => {
    // `spawnLangCommand` returns combined stdout+stderr, so a failing binary
    // yields prose. "nothing came back" must not accuse the binary of being old.
    for (const raw of [
      "error: not initialized — run `travsr init`",
      "",
      "   \n  ",
      "{ not json",
    ]) {
      const got = parseLangList(raw);
      assert.deepStrictEqual(got.langs, [], `raw: ${raw}`);
      assert.deepStrictEqual(got.missingFields, [], `raw: ${raw}`);
    }
  });

  test("valid JSON that is not an array yields no rows and no skew", () => {
    for (const raw of ['{"language":"go"}', '"go"', "42", "null"]) {
      const got = parseLangList(raw);
      assert.deepStrictEqual(got.langs, [], `raw: ${raw}`);
      assert.deepStrictEqual(got.missingFields, [], `raw: ${raw}`);
    }
  });

  test("a NEWER binary is never rejected", () => {
    // Extra fields and a higher revision must pass: the gate asks whether the
    // fields the panel needs are present, never whether anything else is.
    const future = { ...CURRENT_ROW, contract: 99, somethingNew: "x" };
    const got = parseLangList(JSON.stringify([future]));
    assert.deepStrictEqual(got.missingFields, []);
    assert.strictEqual(got.reportedContract, 99);
  });

  test("a field present on any row is not counted as missing", () => {
    // One odd row is a data quirk; every row agreeing is a different binary.
    // Counting a single gap as skew would withhold the whole table over one row.
    const got = parseLangList(JSON.stringify([STALE_ROW, CURRENT_ROW]));
    assert.deepStrictEqual(got.missingFields, []);
  });

  test("a field present but null still counts as reported", () => {
    // `unavailableTarget` is legitimately null on every supported platform.
    const withNulls = { ...CURRENT_ROW, unavailableTarget: null, prerequisites: "" };
    assert.deepStrictEqual(langContractSkew([withNulls]), []);
  });

  test("rows that are not objects report every field missing", () => {
    assert.deepStrictEqual(
      langContractSkew([1, "go", null]).slice().sort(),
      [...LANG_CONTRACT_FIELDS].sort(),
      "there is nothing to read out of a non-object row"
    );
  });

  test("a contract marker of the wrong type is treated as absent", () => {
    // Keying on the value's type, not its presence: a string "1" is not a
    // revision this extension can compare against.
    const got = parseLangList(JSON.stringify([{ ...CURRENT_ROW, contract: "1" }]));
    assert.strictEqual(got.reportedContract, undefined);
    assert.deepStrictEqual(got.missingFields, [], "the fields are all still there");
  });

  test("parseAvailableLanguages keeps its old shape-blind behaviour", () => {
    // Kept as a thin wrapper so existing callers are unaffected; only callers
    // that RENDER the rows have to use parseLangList.
    assert.strictEqual(parseAvailableLanguages(JSON.stringify([STALE_ROW])).length, 1);
    assert.deepStrictEqual(parseAvailableLanguages("garbage"), []);
  });

  test("the extension's contract revision is pinned to the CLI's", () => {
    // Moving this requires moving LANG_LIST_CONTRACT in
    // crates/travsr-cli/src/lang.rs in the same commit.
    assert.strictEqual(LANG_CONTRACT_VERSION, 1);
  });

  test("the contract field list covers every field the panel reads", () => {
    for (const f of [
      "language",
      "status",
      "statusLine",
      "repoState",
      "prerequisites",
      "builtin",
      "availableOnThisPlatform",
      "unavailableTarget",
    ]) {
      assert.ok(
        (LANG_CONTRACT_FIELDS as readonly string[]).includes(f),
        `${f} is read by buildLanguageRows and must be in the contract`
      );
    }
  });

  test("fields the panel no longer reads are not demanded", () => {
    // `needsApproval` and `elevatedHosts` are still declared on `LangInfo` (and
    // still emitted) so an older CLI's JSON parses, but nothing renders them now
    // that elevated access is auto-granted for local use. Demanding them would
    // report a skew against a binary that had legitimately dropped them — i.e.
    // against a NEWER binary, which is the one thing this gate must never do.
    for (const f of ["needsApproval", "elevatedHosts"]) {
      assert.ok(
        !(LANG_CONTRACT_FIELDS as readonly string[]).includes(f),
        `${f} is not rendered, so the contract must not require it`
      );
    }
    const withoutThem = { ...CURRENT_ROW } as Record<string, unknown>;
    delete withoutThem["needsApproval"];
    delete withoutThem["elevatedHosts"];
    assert.deepStrictEqual(
      langContractSkew([withoutThem]),
      [],
      "a payload missing only unread fields is not skewed"
    );
  });
});

suite("#755 Part A: the skew message is actionable", () => {
  test("it names the binary, the missing fields, and the revision needed", () => {
    const msg = contractSkewMessage("/usr/local/bin/travsr", ["status", "repoState"], 0);
    assert.ok(msg.includes("/usr/local/bin/travsr"), "the reader must know WHICH binary");
    assert.ok(msg.includes("status") && msg.includes("repoState"));
    assert.ok(msg.includes("revision 0"), `got: ${msg}`);
    assert.ok(msg.includes(`needs ${LANG_CONTRACT_VERSION}`), `got: ${msg}`);
  });

  test("a binary with no marker at all is described as such", () => {
    const msg = contractSkewMessage("travsr", ["status"]);
    assert.ok(
      msg.includes("no lang-list contract revision"),
      `an absent marker is its own fact, not revision 0; got: ${msg}`
    );
  });

  test("it says the rest of the extension still works", () => {
    // The binary is usable for indexing and search — only the panel is held
    // back. A message that reads like a total failure would send the user
    // reinstalling when they did not have to.
    const msg = contractSkewMessage("travsr", ["status"], 0);
    assert.ok(/indexing and search are unaffected/i.test(msg), `got: ${msg}`);
  });
});

suite("#755 Part A: the Health page withholds skewed rows", () => {
  test("a skewed payload shows an actionable banner instead of the rows", () => {
    const html = healthWith(null, {
      missingFields: ["status", "statusLine", "repoState", "prerequisites"],
      binary: "/home/u/.nvm/versions/node/v20/bin/travsr",
    });
    assert.ok(html.includes("older than this extension expects"), "the banner must appear");
    assert.ok(html.includes("/home/u/.nvm/versions/node/v20/bin/travsr"), "name the binary");
    assert.ok(html.includes("repoState") && html.includes("prerequisites"), "name the gaps");
    assert.ok(html.includes("downloadBinary("), "offer the download");
    assert.ok(html.includes("openBinarySetting("), "offer the settings path");
    assert.ok(
      !html.includes("Could not read the language list."),
      "that placeholder claims a read failure, not a stale binary"
    );
    assert.ok(!html.includes("fixLang(this,"), "no action derived from a missing status");
  });

  test("no skew means no banner and normal rows", () => {
    const html = healthWith(buildLanguageRows([GOOD], []));
    assert.ok(!html.includes("older than this extension expects"));
    assert.ok(html.includes("Go toolchain"), "prerequisites render normally");
    assert.ok(html.includes("enabled</td>"), "an enabled language keeps its state");
  });
});

suite("#755 Part A: per-cell guards never render the string 'undefined'", () => {
  /**
   * The single-odd-row case: `langContractSkew` only flags a field when NO row
   * carries it, so a payload where one row is short still reaches the renderer.
   * These are the three lookups that used to produce a confident wrong answer.
   */
  const oddRow = { ...STALE_ROW, language: "php" } as unknown as LangInfo;

  test("an absent status reads as unknown, not as 'partial'", () => {
    const [good, odd] = buildLanguageRows([GOOD, oddRow], []);
    assert.strictEqual(good.analysis, "full");
    assert.strictEqual(odd.analysis, "unknown", "a missing status must say the value is unknown");
  });

  test("an absent repoState never interpolates the literal 'undefined'", () => {
    const rows = buildLanguageRows([GOOD, oddRow], []);
    assert.strictEqual(rows[1].repoState, "unknown");
    const html = healthWith(rows);
    assert.ok(!html.includes(">undefined<"), "this is the exact cell the issue reports as reading 'undefined'");
    assert.ok(!/title="undefined"/.test(html), "nor as a tooltip");
    assert.ok(/did not report per-repository state for php/.test(html), "the tooltip must name the remedy");
  });

  test("an absent prerequisites is not reported as 'no prerequisites'", () => {
    const [row] = buildLanguageRows([oddRow], []);
    assert.strictEqual(row.prerequisites, "unknown", "'' means the analyzer needs nothing, which is a different claim");
    assert.ok(/did not report prerequisites for php/.test(healthWith([row])));
  });

  test("a needs_approval row from an older CLI still renders an action", () => {
    // Elevated access is auto-granted for local use now, so a current CLI never
    // emits `needs_approval`, but an older one does, and the row still has to
    // give that language something to click rather than an empty Action cell.
    const legacy = {
      ...CURRENT_ROW,
      language: "java",
      status: "needs_approval",
      needsApproval: true,
    } as unknown as LangInfo;
    const [row] = buildLanguageRows([legacy], []);
    assert.notStrictEqual(row.fix, "none", "the row must offer an action");
    const html = healthWith([row]);
    assert.ok(html.includes("fixLang(this, 0)"));
    assert.ok(!html.includes("undefined"), "and must not leak a guessed-at value");
  });

  test("the whole page is free of the string 'undefined' for a short row", () => {
    const html = healthWith(buildLanguageRows([oddRow, GOOD], []));
    assert.ok(!html.includes("undefined"), "any 'undefined' reaching the HTML is a value the page guessed at");
  });

  test("a status tag the panel does not know is never asserted as full", () => {
    // Forward compatibility: a newer CLI adding a status must not be read as a
    // live analysis the extension cannot see.
    const future = { ...CURRENT_ROW, status: "brand_new_state" } as unknown as LangInfo;
    const [row] = buildLanguageRows([future], []);
    assert.strictEqual(row.full, false);
    assert.ok(!healthWith([row]).includes(">partial<"), "and must not be asserted as partial");
  });

  test("a repoState tag the panel does not know reads as unknown", () => {
    const future = { ...CURRENT_ROW, repoState: "brand_new_state" } as unknown as LangInfo;
    const [row] = buildLanguageRows([future], []);
    assert.strictEqual(row.repoState, "unknown");
    const html = healthWith([row]);
    assert.ok(!html.includes("undefined"));
    assert.ok(html.includes("unknown</td>"), "the cell says the value is unknown");
  });
});

suite("Health Languages: installed on this machine versus enabled for this repo", () => {
  const base = { ...CURRENT_ROW, contract: 1 };
  const row = (patch: Record<string, unknown>): LangInfo => ({ ...base, ...patch }) as unknown as LangInfo;

  test("installed but off for this repo offers the enable, not an install", () => {
    const [r] = buildLanguageRows([row({ language: "kotlin", status: "partial", repoState: "not_enabled", installed: true })], []);
    assert.strictEqual(r.installed, true);
    assert.strictEqual(r.repoState, "not_enabled");
    assert.strictEqual(r.fix, "enable");
    const html = healthWith([r]);
    assert.ok(html.includes("Enable for this repo"));
    assert.ok(!html.includes("Install analyzer"));
  });

  test("not installed offers the install, whatever the repo state says", () => {
    const [r] = buildLanguageRows([row({ language: "go", status: "partial", repoState: "not_enabled", installed: false })], []);
    assert.strictEqual(r.fix, "install");
  });

  test("a builtin missing its analyzer offers the install, never a green always-on", () => {
    const [r] = buildLanguageRows([row({ language: "rust", status: "partial", repoState: "needs_analyzer", installed: false, builtin: true })], []);
    assert.strictEqual(r.fix, "install");
    assert.ok(healthWith([r]).includes("no analyzer"));
  });

  test("an unavailable language gets no action and says why", () => {
    const [r] = buildLanguageRows([row({ language: "ruby", status: "unsupported", availableOnThisPlatform: false, unavailableTarget: "windows", installed: false })], []);
    assert.strictEqual(r.availableHere, false);
    assert.strictEqual(r.fix, "none");
    const html = healthWith([r]);
    assert.ok(html.includes("not available on Windows"));
    assert.ok(!html.includes("fixLang(this,"));
  });

  test("an enabled non-builtin can be disabled; a builtin cannot", () => {
    const rows = buildLanguageRows(
      [
        row({ language: "java", status: "active", repoState: "enabled", builtin: false }),
        row({ language: "python", status: "active", repoState: "always_on", builtin: true }),
      ],
      []
    );
    assert.strictEqual(rows[0].canDisable, true);
    assert.strictEqual(rows[1].canDisable, false);
    const html = healthWith(rows);
    assert.ok(html.includes("disableLang(this, 0)"));
    assert.ok(!html.includes("disableLang(this, 1)"));
  });

  test("a language the repo does not contain is listed but never offered an enable or install", () => {
    const detected = new Set(["typescript", "java"]);
    const rows = buildLanguageRows(
      [
        row({ language: "java", status: "active", repoState: "enabled" }),
        row({ language: "kotlin", status: "partial", repoState: "not_enabled", installed: true }),
        row({ language: "go", status: "partial", repoState: "not_enabled", installed: false }),
      ],
      [],
      detected
    );
    assert.strictEqual(rows[0].inRepo, true);
    assert.strictEqual(rows[1].inRepo, false);
    assert.strictEqual(rows[1].fix, "none", "installed elsewhere, but this repo has no kotlin: nothing to enable");
    assert.strictEqual(rows[2].fix, "none", "and nothing to install for a language the repo does not use");
    const html = healthWith(rows);
    assert.ok(html.includes("<td>java"), "the language the repo has is listed");
    assert.ok(!html.includes("<td>kotlin") && !html.includes("<td>go"), "the ones it does not have are not rendered at all");
    assert.ok(!html.includes("Enable for this repo") && !html.includes("Install analyzer"));
    // The index a button posts is the position in the full list, not in the
    // rendered subset, since that is what the extension looks a click up in.
    assert.ok(html.includes("disableLang(this, 0)"));
  });

  test("a repo with none of the catalog's languages says so instead of showing an empty table", () => {
    const rows = buildLanguageRows([row({ language: "go", status: "partial" })], [], new Set(["cobol"]));
    assert.ok(healthWith(rows).includes("no supported languages in this repository"));
  });

  test("when the graph cannot say which languages the repo has, nothing is gated", () => {
    const [r] = buildLanguageRows([row({ language: "kotlin", status: "partial", repoState: "not_enabled", installed: true })], [], new Set());
    assert.strictEqual(r.inRepo, true);
    assert.strictEqual(r.fix, "enable");
  });

  test("repo_languages output parses to a set of canonical names", () => {
    const set = parseRepoLanguages("<travsr-data>\ntypescript\t3200\nRust\t840\n\n</travsr-data>");
    assert.deepStrictEqual([...set].sort(), ["rust", "typescript"]);
    assert.strictEqual(parseRepoLanguages("").size, 0);
  });

  test("a warning on the page turns an active row into a semantic re-run", () => {
    const [r] = buildLanguageRows(
      [row({ language: "java", status: "active", repoState: "enabled" })],
      [{ severity: "warn", title: "'java' analysis ran but found no symbols", hint: "re-run travsr init --semantic --force", command: "travsr init --semantic --force" }]
    );
    assert.strictEqual(r.flagged, true);
    assert.strictEqual(r.fix, "semantic");
  });
});
