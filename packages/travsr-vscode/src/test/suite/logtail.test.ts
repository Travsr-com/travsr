import * as assert from "assert";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { readDaemonLogTail } from "../../commands";
import { buildStatsHtml, LOG_MAX_LINES } from "../../webviews";
import type { LogEntry, StatsView } from "../../webviews";

/**
 * The daemon log rotates daily (`daemon.log.<YYYY-MM-DD>`, seven files kept),
 * and the panel's reader used to open only the newest file. "The last 500
 * lines" is rarely 500 lines of one file: shortly after 00:00 UTC today's file
 * holds a handful and the rest of the answer is in yesterday's, so the panel
 * went near-empty across midnight while the daemon was healthy.
 *
 * These mirror the Rust regression tests for `LogTail::backfill`
 * (`crates/travsr-daemon/src/logfile.rs`), which is the reader
 * `travsr daemon logs` already uses correctly, so the two surfaces cannot drift
 * apart again.
 */

/** A temp repo root containing a `.travsr` dir; removed on process exit. */
function tempRepo(): string {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "travsr-logtail-"));
  fs.mkdirSync(path.join(root, ".travsr"));
  return root;
}

/** Write a log file whose lines are valid daemon JSON carrying `message`. */
function writeLog(root: string, name: string, messages: string[]): void {
  const body = messages
    .map(
      (m, i) =>
        JSON.stringify({
          timestamp: `2026-08-10T00:00:${String(i % 60).padStart(2, "0")}Z`,
          level: "INFO",
          target: "daemon",
          fields: { message: m },
        }) + "\n"
    )
    .join("");
  fs.writeFileSync(path.join(root, ".travsr", name), body);
}

/** The `message` of each returned entry, oldest first. */
function messages(entries: LogEntry[]): string[] {
  return entries.map((e) => e.message);
}

suite("#log-rotation: readDaemonLogTail spans rotated files", () => {
  test("the window is filled from older files when the newest is short", () => {
    // The exact fixture the Rust test uses, so the two readers stay comparable.
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-10", ["a1", "a2", "a3", "a4"]);
    writeLog(root, "daemon.log.2026-08-11", ["b1", "b2"]);
    writeLog(root, "daemon.log.2026-08-12", ["c1"]);

    // 1 from today, 2 from yesterday, 1 from the day before: chronological.
    assert.deepStrictEqual(messages(readDaemonLogTail(root, 4)), ["a4", "b1", "b2", "c1"]);
  });

  test("the newest file alone satisfies a small request", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-10", ["a1", "a2"]);
    writeLog(root, "daemon.log.2026-08-12", ["c1"]);
    assert.deepStrictEqual(messages(readDaemonLogTail(root, 1)), ["c1"]);
  });

  test("more lines requested than exist returns everything, not an error", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-10", ["a1", "a2", "a3", "a4"]);
    writeLog(root, "daemon.log.2026-08-11", ["b1", "b2"]);
    writeLog(root, "daemon.log.2026-08-12", ["c1"]);
    assert.deepStrictEqual(messages(readDaemonLogTail(root, 10_000)), [
      "a1",
      "a2",
      "a3",
      "a4",
      "b1",
      "b2",
      "c1",
    ]);
  });

  test("the reported case: today near-empty, the window still fills", () => {
    // Exactly midnight-rollover shape: yesterday full, today one line.
    //
    // The boundary is pinned against the CLI, which already reads this
    // correctly: on this same 601-line fixture,
    //   travsr daemon logs --lines 500
    // returns `yesterday-102` through `today-1`. The panel must agree, because
    // "the last 500 lines" should not mean two different things depending on
    // which surface you ask.
    const root = tempRepo();
    const yesterday = Array.from({ length: 600 }, (_, i) => `yesterday-${i + 1}`);
    writeLog(root, "daemon.log.2026-08-21", yesterday);
    writeLog(root, "daemon.log.2026-08-22", ["today-1"]);

    const got = readDaemonLogTail(root, 500);
    assert.strictEqual(got.length, 500, "a 500-line request must return 500 lines");
    assert.strictEqual(got[0].message, "yesterday-102", "same first line as the CLI");
    assert.strictEqual(got[got.length - 1].message, "today-1", "same last line as the CLI");
    // Before the fix this returned exactly one line: today's file, in full.
    assert.ok(
      got.filter((e) => e.day === "2026-08-21").length === 499,
      "499 of the 500 come from yesterday's rotated file"
    );
  });

  test("a torn rotated file never glues its last entry onto the next file", () => {
    // A daemon killed mid-write leaves no trailing newline. Concatenating text
    // across files would fuse two entries into one unparseable line.
    const root = tempRepo();
    fs.writeFileSync(
      path.join(root, ".travsr", "daemon.log.2026-08-11"),
      '{"timestamp":"2026-08-11T00:00:00Z","level":"INFO","target":"t","fields":{"message":"first"}}\n' +
        '{"timestamp":"2026-08-11T00:00:01Z","level":"INFO","target":"t","fields":{"message":"torn"}}'
    );
    writeLog(root, "daemon.log.2026-08-12", ["third"]);

    const got = readDaemonLogTail(root, 100);
    assert.deepStrictEqual(messages(got), ["first", "torn", "third"]);
    assert.strictEqual(got.length, 3, "three entries stay three entries");
  });

  test("each entry is tagged with the file it came from", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-11", ["b1"]);
    writeLog(root, "daemon.log.2026-08-12", ["c1"]);
    const got = readDaemonLogTail(root, 10);
    assert.deepStrictEqual(
      got.map((e) => [e.message, e.day]),
      [
        ["b1", "2026-08-11"],
        ["c1", "2026-08-12"],
      ]
    );
  });

  test("a window wider than the old 512 KB single read is fully returned", () => {
    // The previous reader took a fixed 512 KB slice, which cannot hold the
    // larger windows the Lines control now offers.
    const root = tempRepo();
    const many = Array.from({ length: 4000 }, (_, i) => `line-${i}-${"x".repeat(200)}`);
    writeLog(root, "daemon.log.2026-08-12", many);
    const got = readDaemonLogTail(root, 3000);
    assert.strictEqual(got.length, 3000);
    assert.strictEqual(got[got.length - 1].message, many[many.length - 1]);
  });

  test("the window is capped so a huge request cannot wedge the webview", () => {
    const root = tempRepo();
    writeLog(
      root,
      "daemon.log.2026-08-12",
      Array.from({ length: LOG_MAX_LINES + 500 }, (_, i) => `m${i}`)
    );
    assert.strictEqual(readDaemonLogTail(root, 1_000_000).length, LOG_MAX_LINES);
  });

  test("a partial leading line from a mid-file scan is dropped", () => {
    // The scan seeks backwards and can land mid-line; half an entry is not an
    // entry. Forced by making one file far larger than the chunk size.
    const root = tempRepo();
    const many = Array.from({ length: 20_000 }, (_, i) => `m${i}`);
    writeLog(root, "daemon.log.2026-08-12", many);
    const got = readDaemonLogTail(root, 50);
    assert.strictEqual(got.length, 50);
    // Every returned line parsed as real JSON, so none is a truncated fragment.
    for (const e of got) {
      assert.ok(e.message.startsWith("m"), `truncated entry leaked through: ${e.raw}`);
      assert.strictEqual(e.level, "INFO");
    }
  });

  test("missing, empty, and non-file cases return nothing rather than throwing", () => {
    assert.deepStrictEqual(readDaemonLogTail(path.join(os.tmpdir(), "no-such-repo-xyz")), []);

    const empty = tempRepo();
    assert.deepStrictEqual(readDaemonLogTail(empty), []);

    // A directory whose name looks like a log file must be skipped, matching
    // the Rust reader's is_file() guard.
    const dirCase = tempRepo();
    fs.mkdirSync(path.join(dirCase, ".travsr", "daemon.log.2026-08-12"));
    assert.deepStrictEqual(readDaemonLogTail(dirCase), []);

    // A zero-byte file is not an error either.
    const zero = tempRepo();
    fs.writeFileSync(path.join(zero, ".travsr", "daemon.log.2026-08-12"), "");
    assert.deepStrictEqual(readDaemonLogTail(zero), []);
  });

  test("unrelated files in .travsr are not read as logs", () => {
    const root = tempRepo();
    fs.writeFileSync(path.join(root, ".travsr", "daemon.lock"), "12345");
    fs.writeFileSync(path.join(root, ".travsr", "registry.lock"), "x");
    writeLog(root, "daemon.log.2026-08-12", ["only-this"]);
    assert.deepStrictEqual(messages(readDaemonLogTail(root, 10)), ["only-this"]);
  });

  test("a zero request reads nothing at all", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-12", ["a"]);
    assert.deepStrictEqual(readDaemonLogTail(root, 0), []);
  });
});

const STATS: StatsView = {
  nodes: "1",
  edges: "1",
  schemaVersion: "1",
  dbSize: "1 B",
  lastIndexed: "just now",
};

/** A log entry carrying just the fields the divider logic reads. */
function entry(message: string, day?: string): LogEntry {
  return {
    time: "01:00:00",
    level: "INFO",
    target: "daemon",
    message,
    detail: "",
    iso: "2026-08-12T01:00:00Z",
    raw: "{}",
    ...(day !== undefined ? { day } : {}),
  };
}

suite("#log-rotation: the panel marks where one day's file ends", () => {
  test("a divider appears exactly at the day change", () => {
    const html = buildStatsHtml(STATS, [
      entry("yesterday", "2026-08-21"),
      entry("today", "2026-08-22"),
    ]);
    assert.strictEqual(
      (html.match(/class="log-day"/g) ?? []).length,
      1,
      "two days means exactly one divider"
    );
    assert.ok(html.includes(">2026-08-21<"), "the divider names the older day it heads");
  });

  test("a single day renders no divider", () => {
    const html = buildStatsHtml(STATS, [
      entry("one", "2026-08-22"),
      entry("two", "2026-08-22"),
      entry("three", "2026-08-22"),
    ]);
    // `class="log-day"`, not bare "log-day": the stylesheet always defines the
    // rule, so the loose check would pass no matter what the renderer did.
    assert.ok(!html.includes('class="log-day"'), "one file needs no boundary marker");
  });

  test("entries with no day attribution render as before", () => {
    // `day` is optional, so a caller that never touched the reader (or an older
    // cached entry) must not start emitting dividers.
    const html = buildStatsHtml(STATS, [entry("a"), entry("b")]);
    assert.ok(!html.includes('class="log-day"'));
    assert.strictEqual((html.match(/class="log-line/g) ?? []).length, 2);
  });

  test("dividers are not log lines, so the count stays honest", () => {
    const html = buildStatsHtml(STATS, [
      entry("a", "2026-08-20"),
      entry("b", "2026-08-21"),
      entry("c", "2026-08-22"),
    ]);
    assert.strictEqual((html.match(/class="log-line/g) ?? []).length, 3, "three entries");
    assert.strictEqual((html.match(/class="log-day"/g) ?? []).length, 2, "two boundaries");
    assert.ok(html.includes("3 lines"), "the header counts entries, not dividers");
  });

  test("the divider is escaped like every other CLI-supplied value", () => {
    // The hostile name goes on the OLDER entry: rows render newest-first, so a
    // divider heads the older block and only that day's text becomes markup.
    const html = buildStatsHtml(STATS, [
      entry("a", '"><script>x</script>'),
      entry("b", "2026-08-22"),
    ]);
    // The shell legitimately contains its own <script> tags, so assert on the
    // injected payload specifically: it must appear escaped and never verbatim.
    assert.ok(
      !html.includes('"><script>x</script>'),
      "a hostile file name must not reach the DOM raw"
    );
    assert.ok(
      html.includes("&lt;script&gt;x&lt;/script&gt;"),
      "it must still be rendered, escaped"
    );
  });
});

suite("#log-rotation: the Lines control can widen the window", () => {
  test("it offers windows above the old 500 ceiling, with the cap named", () => {
    const html = buildStatsHtml(STATS, [entry("a", "2026-08-22")], [], 500);
    assert.ok(html.includes('value="2000"'), "2000 must be offered");
    assert.ok(
      html.includes(`All (max ${LOG_MAX_LINES})`),
      "the ceiling is stated in the label, not discovered later"
    );
  });

  test("the selected option reflects the window actually loaded", () => {
    const wide = buildStatsHtml(STATS, [entry("a", "2026-08-22")], [], 2000);
    assert.ok(
      /value="2000" selected/.test(wide),
      "reopening after a widen must not snap back to 200"
    );
    const narrow = buildStatsHtml(STATS, [entry("a", "2026-08-22")], [], 200);
    assert.ok(/value="200" selected/.test(narrow));
  });

  test("the control reports what is loaded so a widen can trigger a re-read", () => {
    const html = buildStatsHtml(STATS, [entry("a", "2026-08-22")], [], 500);
    assert.ok(html.includes('data-loaded="500"'), "the webview needs the loaded window");
    assert.ok(html.includes("onLogLinesChange()"), "the change handler must be wired");
    assert.ok(html.includes("setLogLines"), "widening must post back to the extension");
  });
});
