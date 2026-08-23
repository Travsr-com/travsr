import * as assert from "assert";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import {
  readDaemonLogTail,
  readDaemonLogFile,
  daemonLogFileList,
  logFileRelativeDay,
  LOG_MAX_BYTES_PER_FILE,
  LOG_OVERSIZED_PREVIEW_CHARS,
} from "../../commands";
import {
  buildStatsHtml,
  formatLogSize,
  LOG_MAX_LINES,
  LOG_MAX_FILES_LISTED,
  LOG_AUTO_SECONDS,
  buildLogRowsHtml,
} from "../../webviews";
import type { LogEntry, LogFileInfo, StatsView } from "../../webviews";

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
 *
 * The panel then went further and stopped spanning at all: a File control picks
 * one rotated file, defaulting to the newest, because a continuous stream gave
 * no way to ask for a particular day. `readDaemonLogTail` stays under test as
 * the port of `backfill`, which is still what the CLI does; the suites below it
 * cover the reader the panel actually calls.
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

  test("a multi-byte character straddling a chunk boundary survives intact", () => {
    // The scan reads backwards in 64 KB chunks. Decoding each chunk separately
    // splits any UTF-8 sequence that crosses a boundary into two replacement
    // characters and the character is gone, while `travsr daemon logs` prints
    // it correctly. Bytes are accumulated and decoded once to prevent that.
    //
    // Built to straddle deliberately rather than hoping: the accented byte pair
    // is placed so it spans the `size - 65536` boundary exactly.
    const root = tempRepo();
    const file = path.join(root, ".travsr", "daemon.log.2026-08-12");
    const CHUNK = 64 * 1024;

    /** A valid daemon JSON line of exactly `n` bytes, including its newline. */
    const lineOfBytes = (n: number, ts: string): string => {
      const base = Buffer.byteLength(
        JSON.stringify({
          timestamp: ts,
          level: "INFO",
          target: "daemon",
          fields: { message: "" },
        }) + "\n",
        "utf8"
      );
      return (
        JSON.stringify({
          timestamp: ts,
          level: "INFO",
          target: "daemon",
          fields: { message: "p".repeat(Math.max(n - base, 0)) },
        }) + "\n"
      );
    };

    const marker = "travsr-café/src/lib.rs";
    const markerLine =
      JSON.stringify({
        timestamp: "2026-08-12T00:00:01Z",
        level: "INFO",
        target: "daemon",
        fields: { message: marker },
      }) + "\n";
    // Byte offset of the first of the two bytes of `é` within the line.
    const eAt = Buffer.byteLength(markerLine.slice(0, markerLine.indexOf("é")), "utf8");

    // The first chunk read is [size - CHUNK, size), so the boundary is
    // `size - CHUNK`. For it to fall between the two bytes of `é` we need
    //     size - CHUNK == eStart + 1
    // and since the marker sits at the front, eStart is just eAt. Solve for the
    // total size and pad the tail to exactly that.
    const head = "";
    const eStart = Buffer.byteLength(head, "utf8") + eAt;
    const wantSize = eStart + 1 + CHUNK;
    const written = Buffer.byteLength(head + markerLine, "utf8");
    const tail = lineOfBytes(wantSize - written, "2026-08-12T00:00:02Z");
    fs.writeFileSync(file, head + markerLine + tail, "utf8");

    const size = fs.statSync(file).size;
    const boundary = size - CHUNK;
    const bytes = fs.readFileSync(file);
    assert.strictEqual(
      boundary,
      eStart + 1,
      `fixture must straddle: e at ${eStart}..${eStart + 1}, boundary ${boundary}`
    );
    assert.strictEqual(bytes[eStart], 0xc3, "first byte of the two-byte sequence");
    assert.strictEqual(bytes[eStart + 1], 0xa9, "second byte, on the far side of the boundary");

    const got = readDaemonLogTail(root, 45);
    const hit = got.find((e) => e.message.includes("travsr-caf"));
    assert.ok(hit, "the line spanning the boundary must be returned");
    assert.strictEqual(hit.message, marker, "the accented character must survive intact");
    assert.ok(!hit.message.includes("�"), "no replacement characters");
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

suite("#log-rotation: the File control reads one rotated file", () => {
  test("the named file is read, not the newest and not a span", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-21", ["a1", "a2", "a3"]);
    writeLog(root, "daemon.log.2026-08-22", ["b1", "b2"]);
    writeLog(root, "daemon.log.2026-08-23", ["c1"]);

    assert.deepStrictEqual(messages(readDaemonLogFile(root, "daemon.log.2026-08-22", 500)), [
      "b1",
      "b2",
    ]);
    assert.deepStrictEqual(messages(readDaemonLogFile(root, "daemon.log.2026-08-23", 500)), ["c1"]);
    // Same fixture through the spanning reader: the two answers differ now, on
    // purpose, and both are still asserted so neither drifts unnoticed.
    assert.deepStrictEqual(messages(readDaemonLogTail(root, 500)), [
      "a1",
      "a2",
      "a3",
      "b1",
      "b2",
      "c1",
    ]);
  });

  test("a window smaller than the file takes the tail of it", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-22", ["b1", "b2", "b3", "b4"]);
    assert.deepStrictEqual(messages(readDaemonLogFile(root, "daemon.log.2026-08-22", 2)), [
      "b3",
      "b4",
    ]);
  });

  test("over-requesting does not pad from a neighbouring file", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-21", ["a1", "a2", "a3"]);
    writeLog(root, "daemon.log.2026-08-22", ["b1"]);
    // 500 asked for, 1 in that file. The older file must not fill the gap:
    // not filling it is the entire point of picking a file.
    assert.deepStrictEqual(messages(readDaemonLogFile(root, "daemon.log.2026-08-22", 500)), ["b1"]);
  });

  test("a name the directory does not list reads as empty, not as a path", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-22", ["b1"]);
    fs.writeFileSync(path.join(root, "secret.txt"), "not a log\n");
    // The name arrives from the webview, so anything the listing does not
    // report must read empty whatever shape it arrives in.
    for (const name of [
      "../secret.txt",
      "..\\secret.txt",
      "daemon.log.2026-08-22/../../secret.txt",
      "/etc/passwd",
      "daemon.log.2026-08-99",
      "",
    ]) {
      assert.deepStrictEqual(readDaemonLogFile(root, name, 500), [], name + " must read empty");
    }
  });

  test("every entry carries the day of the file it came from", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-21", ["a1", "a2"]);
    const got = readDaemonLogFile(root, "daemon.log.2026-08-21", 500);
    assert.deepStrictEqual(
      got.map((e) => e.day),
      ["2026-08-21", "2026-08-21"]
    );
  });

  test("a zero window reads nothing", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-22", ["b1"]);
    assert.deepStrictEqual(readDaemonLogFile(root, "daemon.log.2026-08-22", 0), []);
  });

  test("a repo with no .travsr reads as empty", () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "travsr-nolog-"));
    assert.deepStrictEqual(readDaemonLogFile(root, "daemon.log.2026-08-22", 500), []);
  });
});

suite("#log-rotation: the File control lists what is on disk", () => {
  test("newest first, so the default sits at the top", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-21", ["a"]);
    writeLog(root, "daemon.log.2026-08-22", ["b"]);
    writeLog(root, "daemon.log.2026-08-23", ["c"]);
    const { files, onDisk } = daemonLogFileList(root, "2026-08-23");
    assert.deepStrictEqual(
      files.map((f) => f.day),
      ["2026-08-23", "2026-08-22", "2026-08-21"]
    );
    assert.strictEqual(onDisk, 3);
  });

  test("the list is capped and reports how many are really there", () => {
    const root = tempRepo();
    // More than the daemon's own cap on purpose: MAX_LOG_FILES is enforced by
    // prune, so a restored .travsr can hold more, and the dropdown must not
    // grow to fit whatever it finds.
    for (let d = 1; d <= 12; d++) {
      writeLog(root, "daemon.log.2026-08-" + String(d).padStart(2, "0"), ["x"]);
    }
    const { files, onDisk } = daemonLogFileList(root, "2026-08-12");
    assert.strictEqual(files.length, LOG_MAX_FILES_LISTED);
    assert.strictEqual(onDisk, 12);
    assert.strictEqual(files[0].day, "2026-08-12", "the newest must survive the cap");
  });

  test("today and yesterday are labelled, older files are not", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-21", ["a"]);
    writeLog(root, "daemon.log.2026-08-22", ["b"]);
    writeLog(root, "daemon.log.2026-08-23", ["c"]);
    const { files } = daemonLogFileList(root, "2026-08-23");
    assert.deepStrictEqual(
      files.map((f) => f.rel),
      ["today", "yesterday", undefined]
    );
  });

  test("sizes are reported and line counts are never asked for", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-22", ["b1", "b2"]);
    const { files } = daemonLogFileList(root, "2026-08-22");
    assert.ok(files[0].size > 0, "size is free from statSync");
    assert.ok(
      !("lines" in files[0]),
      "a line count would cost a full read of every file on every redraw"
    );
  });

  test("a directory named like a log is not a log", () => {
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-22", ["b"]);
    fs.mkdirSync(path.join(root, ".travsr", "daemon.log.2026-08-23"));
    const { files, onDisk } = daemonLogFileList(root, "2026-08-23");
    assert.deepStrictEqual(
      files.map((f) => f.day),
      ["2026-08-22"]
    );
    assert.strictEqual(onDisk, 1);
  });
});

suite("#log-rotation: relative day labels", () => {
  test("today, yesterday, and nothing else", () => {
    assert.strictEqual(logFileRelativeDay("2026-08-23", "2026-08-23"), "today");
    assert.strictEqual(logFileRelativeDay("2026-08-22", "2026-08-23"), "yesterday");
    assert.strictEqual(logFileRelativeDay("2026-08-21", "2026-08-23"), undefined);
    // Seven files can span months, so a counted label would be wrong more
    // often than it would help.
    assert.strictEqual(logFileRelativeDay("2026-06-30", "2026-08-23"), undefined);
  });

  test("month and year ends are real dates, not string arithmetic", () => {
    assert.strictEqual(logFileRelativeDay("2026-07-31", "2026-08-01"), "yesterday");
    assert.strictEqual(logFileRelativeDay("2025-12-31", "2026-01-01"), "yesterday");
    assert.strictEqual(logFileRelativeDay("2024-02-29", "2024-03-01"), "yesterday");
  });

  test("a file whose suffix is not a date gets no label", () => {
    // logFileDay falls back to the whole name, which must not be relabelled.
    assert.strictEqual(logFileRelativeDay("daemon.log", "2026-08-23"), undefined);
    assert.strictEqual(logFileRelativeDay("2026-8-3", "2026-08-23"), undefined);
  });
});

suite("#log-rotation: the File control renders", () => {
  const FILES: LogFileInfo[] = [
    { name: "daemon.log.2026-08-23", day: "2026-08-23", size: 142, rel: "today" },
    { name: "daemon.log.2026-08-22", day: "2026-08-22", size: 61896, rel: "yesterday" },
    { name: "daemon.log.2026-08-21", day: "2026-08-21", size: 61896 },
  ];
  const withFiles = (onDisk: number, selected: string): string =>
    buildStatsHtml(STATS, [entry("a", "2026-08-23")], [], 500, { files: FILES, onDisk, selected });

  test("every file is offered, valued by name, with the showing one selected", () => {
    const html = withFiles(3, "daemon.log.2026-08-22");
    assert.ok(html.includes('id="logFile"'));
    assert.ok(html.includes("onLogFileChange()"), "the change handler must be wired");
    assert.ok(html.includes("setLogFile"), "picking a file must post back to the extension");
    for (const f of FILES) {
      assert.ok(html.includes('value="' + f.name + '"'), f.name + " must be offered");
    }
    assert.ok(
      html.includes('value="daemon.log.2026-08-22" selected'),
      "the file being shown must be the selected option"
    );
  });

  test("the label carries the day, the relative word and the size", () => {
    const html = withFiles(3, "daemon.log.2026-08-23");
    assert.ok(html.includes("2026-08-23 · today · 142 B"));
    assert.ok(html.includes("2026-08-22 · yesterday · 60 KB"));
    // No relative word on the oldest, and no empty separator left behind.
    assert.ok(html.includes("2026-08-21 · 60 KB"));
    assert.ok(!html.includes("2026-08-21 ·  ·"));
  });

  test("the day boundary is named as UTC", () => {
    // rolling::daily rotates on the UTC date while rows show local times, so at
    // UTC+5:30 the file called 2026-08-22 holds part of the 23rd. Saying so is
    // the difference between a quirk and a bug report.
    assert.ok(withFiles(3, "daemon.log.2026-08-23").includes("UTC days"));
  });

  test("a capped list says how many files are really on disk", () => {
    assert.ok(
      withFiles(12, "daemon.log.2026-08-23").includes("3 of 12 files"),
      "a silent truncation reads as the whole history"
    );
  });

  test("an uncapped list says nothing about counts", () => {
    assert.ok(!/of 3 files/.test(withFiles(3, "daemon.log.2026-08-23")));
  });

  test("no file list renders no File control at all", () => {
    // Callers that pass four arguments still get a working panel; an empty
    // dropdown would be worse than none.
    const html = buildStatsHtml(STATS, [entry("a", "2026-08-22")], [], 500);
    assert.ok(!html.includes('id="logFile"'));
    assert.ok(html.includes('id="logLines"'), "the rest of the toolbar must survive");
  });

  test("a hostile file name is escaped", () => {
    const html = buildStatsHtml(STATS, [], [], 500, {
      files: [{ name: 'daemon.log."><script>x</script>', day: '"><script>x</script>', size: 1 }],
      onDisk: 1,
      selected: "none",
    });
    assert.ok(!html.includes("<script>x</script>"), "a file name is not markup");
  });
});

suite("#log-rotation: file sizes read as sizes", () => {
  test("bytes, whole KB, one decimal MB", () => {
    assert.strictEqual(formatLogSize(0), "0 B");
    assert.strictEqual(formatLogSize(142), "142 B");
    assert.strictEqual(formatLogSize(1023), "1023 B");
    assert.strictEqual(formatLogSize(1024), "1 KB");
    assert.strictEqual(formatLogSize(61896), "60 KB");
    assert.strictEqual(formatLogSize(1024 * 1024), "1.0 MB");
    assert.strictEqual(formatLogSize(52 * 1024 * 1024), "52.0 MB");
  });
});

suite("#log-rotation: auto-refresh on a chosen interval", () => {
  /** Just the Auto `<select>`, because the toolbar has four of them and Since
   *  offers `value="0"`, `value="5"` and `value="60"` of its own. Asserting on
   *  the whole page passes for the wrong reason. */
  const autoBlock = (html: string): string => {
    const at = html.indexOf('id="logAuto"');
    assert.ok(at >= 0, "the Auto control must be there");
    return html.slice(at, html.indexOf("</select>", at));
  };

  test("the intervals are offered, Off is the default", () => {
    const html = buildStatsHtml(STATS, [entry("a", "2026-08-22")], [], 500);
    const auto = autoBlock(html);
    assert.ok(html.includes("onLogAutoChange()"), "the change handler must be wired");
    assert.ok(html.includes("setLogAuto"), "the choice must reach the extension");
    for (const s of LOG_AUTO_SECONDS) {
      assert.ok(auto.includes(`value="${s}"`), `${s} must be offered`);
      assert.ok(auto.includes(s === 0 ? ">Off<" : `>${s}s<`), `${s} must be labelled`);
    }
    // Off by default: a panel that starts polling because it was opened is a
    // surprise, and the reader has not asked for anything yet.
    assert.ok(/value="0" selected/.test(auto), "Off is the default");
  });

  test("a running interval survives a full redraw", () => {
    // The point of threading autoSeconds through: a redraw replaces the
    // document, so without this the control would come back reading Off while
    // the extension-side timer kept ticking, which is the Follow bug inverted.
    const auto = autoBlock(buildStatsHtml(STATS, [entry("a", "2026-08-22")], [], 500, undefined, 15));
    assert.ok(/value="15" selected/.test(auto));
    assert.ok(!/value="0" selected/.test(auto), "Off must not also be selected");
    assert.strictEqual(auto.match(/selected/g)?.length, 1, "exactly one option selected");
  });

  test("a tick replaces only the rows, and reapplies the filters to them", () => {
    const html = buildStatsHtml(STATS, [entry("a", "2026-08-22")], [], 500);
    assert.ok(html.includes("setLogRows"), "the tick message must be handled");
    // The handler has to re-run filterLog, or the new rows arrive ignoring the
    // search box and the severity chip that the old rows were obeying.
    const handler = html.slice(html.indexOf("setLogRows"));
    assert.ok(handler.includes("filterLog()"), "filters must be reapplied");
    assert.ok(handler.includes("logEmpty"), "the empty-state node must be rebuilt");
    assert.ok(handler.includes("scrollTop"), "scroll position must be handled");
  });

  test("Follow is gone, and so is the message only it sent", () => {
    // Follow meant to poll the log the way `travsr daemon logs --follow` does,
    // but it set its interval in the webview. A refresh assigns
    // panel.webview.html wholesale, so the document the timer lived in was
    // replaced by the very tick that triggered it, and the re-rendered checkbox
    // carried no `checked` attribute. It fired once, cleared itself about three
    // seconds after being ticked, and never polled again.
    //
    // The Auto control above is the replacement, with the timer on the extension
    // side where a redraw cannot reach it.
    const html = buildStatsHtml(STATS, [entry("a", "2026-08-22")], [], 500);
    assert.ok(!html.includes("logFollow"), "no Follow checkbox");
    assert.ok(!html.includes("toggleFollow"), "no Follow handler");
    assert.ok(!html.includes("refreshLog"), "its only message goes with it");
    assert.ok(html.includes('id="refreshBtn"'), "manual Refresh must survive");
    // UTC and JSON are local filters over rows already in the DOM, so they
    // never depended on a redraw and are unaffected.
    assert.ok(html.includes('id="logUtc"'));
    assert.ok(html.includes('id="logJson"'));
  });
});

suite("#log-rotation: the rows a tick sends are the rows a render builds", () => {
  test("buildLogRowsHtml output is what the full page embeds", () => {
    // Two paths render the same log now: a full render and an auto tick. If they
    // ever diverge, a polled panel drifts away from a refreshed one, so the page
    // is asserted to contain exactly what the tick would send.
    const log = [entry("first", "2026-08-22"), entry("second", "2026-08-22")];
    const rows = buildLogRowsHtml(log);
    assert.ok(rows.includes("first") && rows.includes("second"));
    assert.ok(buildStatsHtml(STATS, log, [], 500).includes(rows));
  });

  test("an empty log says how to start the daemon, on both paths", () => {
    const rows = buildLogRowsHtml([]);
    assert.ok(rows.includes("No daemon log yet"));
    assert.ok(rows.includes("travsr daemon start"));
    assert.ok(buildStatsHtml(STATS, [], [], 500).includes(rows));
  });

  test("rows run newest first, and the caller's array is not reordered", () => {
    const log = [entry("older", "2026-08-22"), entry("newer", "2026-08-22")];
    const rows = buildLogRowsHtml(log);
    assert.ok(rows.indexOf("newer") < rows.indexOf("older"), "newest first");
    assert.strictEqual(log[0].message, "older", "reverse() must not hit the caller");
  });
});

suite("#log-rotation: the per-file byte ceiling and the partial-line drop", () => {
  /** Comfortably past the ceiling, so the scan is guaranteed to stop inside it. */
  const OVERSIZE = LOG_MAX_BYTES_PER_FILE + 1024 * 1024;

  /** Write raw bytes, bypassing writeLog's JSON shaping. */
  function writeRaw(root: string, name: string, body: string): void {
    fs.writeFileSync(path.join(root, ".travsr", name), body);
  }

  test("an oversized line does not make a file with readable lines read as empty", () => {
    // The reported case. Two good entries at offset 0, then one line bigger than
    // the whole scan budget. The only newline inside the window is the one
    // closing that line, so dropping the partial first line left nothing, and
    // the panel rendered "No daemon log yet" over a file with real entries in
    // it: the exact symptom this reader exists to remove, by another door.
    const root = tempRepo();
    writeRaw(root, "daemon.log.2026-08-22", '{"a":1}\n{"a":2}\n' + "Z".repeat(OVERSIZE) + "\n");

    const got = readDaemonLogFile(root, "daemon.log.2026-08-22", 500);
    assert.strictEqual(got.length, 1, "one accounted row, never an empty log");
    assert.ok(
      got[0].message.startsWith("[travsr]"),
      `expected a panel notice, got: ${got[0].message.slice(0, 120)}`
    );
    assert.ok(got[0].message.includes("MB per-file scan"), "the limit must be named");
    assert.ok(
      got[0].message.includes("travsr daemon logs"),
      "and the way to read the file without it"
    );
  });

  test("a window with no boundary at all is bounded, not an 8 MB entry", () => {
    // Nothing but one unterminated line. The window held no newline, `indexOf`
    // returned -1, and `slice(-1 + 1)` kept the whole fragment: one 8 MB
    // "entry", which buildLogRowsHtml embeds four times over (data-json,
    // data-raw, the message text and the JSON view) for ~32 MB of HTML from a
    // single row, against the 8.69 MB that LOG_MAX_LINES budgets for 5000.
    const root = tempRepo();
    writeRaw(root, "daemon.log.2026-08-22", "x".repeat(OVERSIZE));

    const got = readDaemonLogFile(root, "daemon.log.2026-08-22", 500);
    assert.strictEqual(got.length, 1);
    assert.ok(
      got[0].raw.length < LOG_OVERSIZED_PREVIEW_CHARS * 2,
      `row is ${got[0].raw.length} chars, ceiling is ${LOG_MAX_BYTES_PER_FILE}`
    );
    // The cost that actually mattered, asserted where it lands.
    const html = buildLogRowsHtml(got);
    assert.ok(html.length < 100_000, `one row produced ${html.length} chars of HTML`);
  });

  test("a file under the ceiling is untouched by any of this", () => {
    // The ceiling must not fire on ordinary files, and the partial-line drop
    // must keep working when the window does hold a boundary.
    const root = tempRepo();
    writeLog(root, "daemon.log.2026-08-22", ["a", "b", "c"]);
    assert.deepStrictEqual(
      messages(readDaemonLogFile(root, "daemon.log.2026-08-22", 500)),
      ["a", "b", "c"]
    );
  });

  test("an oversized line does not hide complete lines that follow it", () => {
    // The other side of the same file: when the oversized line is at the START
    // and good lines come after it, those are inside the window and must be
    // returned normally, with the fragment dropped and no notice.
    const root = tempRepo();
    writeRaw(root, "daemon.log.2026-08-22", "Z".repeat(OVERSIZE) + '\n{"a":1}\n{"a":2}\n');

    const got = readDaemonLogFile(root, "daemon.log.2026-08-22", 500);
    assert.strictEqual(got.length, 2, "both trailing entries must survive");
    assert.ok(
      !got.some((e) => e.message.startsWith("[travsr]")),
      "no notice when complete lines were in reach"
    );
    assert.ok(
      got.every((e) => e.raw.length < 100),
      "and no fragment glued onto one of them"
    );
  });
});
