/**
 * VSCODE-247 — CLI↔UI parity commands.
 *
 * Surfaces CLI-only features in the extension:
 *   travsr.askSymbol         live ranked symbol search (Quick Pick)
 *   travsr.manageSynonyms    synonym editor webview (multi-chip add)
 *   travsr.showDependencies  direct + transitive imports, click-navigable
 *   travsr.showExecutionPath lowest-cost path between two symbols, rendered in the graph
 *   travsr.showRepos         registry manager webview
 *   travsr.showGraphStats    Health panel: verdict, metrics, daemon, index, languages
 *                            (installed on this machine vs enabled for this repo)
 *   travsr.showLanguages     alias of showGraphStats (the Languages panel was folded in)
 *
 * Pure helpers (stripEnvelope, parsers, openAtLine) are exported for unit tests.
 */

import * as cp from "child_process";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import type { McpClient } from "./mcp";
import { ActiveRepo } from "./activeRepo";
import { GraphPanel, type GraphData, type GraphNode } from "./graph";
import {
  buildSynonymsHtml,
  buildReposHtml,
  buildStatsHtml,
  buildPanelLoadingHtml,
  LOG_MAX_LINES,
  LOG_MAX_FILES_LISTED,
  LOG_AUTO_SECONDS,
  UNKNOWN_INDEX,
  formatLogSize,
  EMPTY_HEALTH,
  LANG_CONTRACT_FIELDS,
  LANG_CONTRACT_VERSION,
  type RepoRow,
  type StatsView,
  type LangInfo,
  type LangContractSkew,
} from "./webviews";
import type { Diagnostic, LogEntry, LogFileInfo } from "./webviews";
import type { IndexHealth } from "./webviews";
import type {
  HealthData,
  SidecarRow,
  AgentRow,
  IntegrityView,
  LanguageRow,
} from "./webviews";
import { resolveAgentTargets } from "./mcpRegister";
import { runTravsrCommand, parseTravsrInvocation } from "./terminal";

// ── Pure helpers (unit-testable) ────────────────────────────────────────────

/**
 * Strip the `<travsr-data>…</travsr-data>` envelope the MCP server wraps around
 * repo-derived tool output (SEC-001). Returns the inner text, or the input
 * unchanged when no envelope is present. An empty envelope yields "".
 */
export function stripEnvelope(raw: string): string {
  const m = /^<travsr-data>\n?([\s\S]*?)\n?<\/travsr-data>$/.exec(raw.trim());
  return m ? m[1] : raw;
}

/** A ranked symbol search result row. */
export interface SymbolItem extends vscode.QuickPickItem {
  path: string;
  line?: number;
}

/** Map a graph node `kind` to a VS Code codicon id. */
export function kindCodicon(kind: string): string {
  switch (kind) {
    case "function":
    case "method":
      return "symbol-method";
    case "class":
      return "symbol-class";
    case "interface":
      return "symbol-interface";
    case "struct":
      return "symbol-structure";
    case "enum":
      return "symbol-enum";
    case "var":
    case "variable":
      return "symbol-variable";
    case "file":
      return "symbol-file";
    default:
      return "symbol-misc";
  }
}

/**
 * Parse a `get_graph_json` payload into ranked Quick Pick items. Non-symbol
 * (file) nodes are dropped so the search returns navigable definitions. Returns
 * an empty array on malformed JSON — never throws.
 */
export function parseGraphSymbols(raw: string): SymbolItem[] {
  if (!raw) return [];
  let data: GraphData;
  try {
    data = JSON.parse(raw) as GraphData;
  } catch {
    return [];
  }
  if (!Array.isArray(data.nodes)) return [];
  return data.nodes
    .filter((n) => n.kind !== "file")
    .map((n) => ({
      label: `$(${kindCodicon(n.kind)}) ${n.label}`,
      description: n.path,
      detail: typeof n.score === "number" ? `score ${n.score.toFixed(3)}` : undefined,
      path: n.path,
      line: n.line,
    }));
}

/** A single synonym pair. */
export interface SynonymPair {
  term: string;
  alias: string;
}

/** Parse the `synonym_list` output (`term => alias` per line) into pairs. */
export function parseSynonymList(raw: string): SynonymPair[] {
  const inner = stripEnvelope(raw);
  return inner
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => {
      const idx = l.indexOf(" => ");
      if (idx < 0) return null;
      return { term: l.slice(0, idx), alias: l.slice(idx + 4) };
    })
    .filter((p): p is SynonymPair => p !== null);
}

/**
 * Parse `get_execution_path` prose (`signature (kind) — path`, one node per
 * line) into a synthetic GraphData: a node per line flagged `root` (so the graph
 * highlights it) chained source→sink by `flows` edges.
 */
export function parseExecutionPath(raw: string): GraphData {
  const inner = stripEnvelope(raw);
  // PROTOCOL, not prose: `get_callers` and friends print
  // `<sig> (<kind>) — <path>` and this splits on that exact
  // separator, so a punctuation sweep that reaches it breaks it.
  const lineRe = /^(?:\[[^\]]+\]\s*)?(.+?)\s+\((\w+)\)\s+—\s+(.+)$/;
  const nodes: GraphNode[] = [];
  for (const line of inner.split("\n")) {
    const t = line.trim();
    if (!t) continue;
    const m = lineRe.exec(t);
    const node: GraphNode = m
      ? { id: m[1], label: m[1], kind: m[2], path: m[3], package: "", score: 0, root: true }
      : { id: t, label: t, kind: "symbol", path: "", package: "", score: 0, root: true };
    nodes.push(node);
  }
  const edges = nodes.slice(1).map((n, i) => ({
    source: nodes[i].id,
    target: n.id,
    kind: "flows",
  }));
  return { nodes, edges };
}

/** Parse `repos_list` TSV output (`name\tdb_path\t{0|1}`) into rows. */
export function parseReposList(raw: string): RepoRow[] {
  const inner = stripEnvelope(raw);
  return inner
    .split("\n")
    .map((l) => l.replace(/\r$/, ""))
    .filter((l) => l.trim())
    .map((l) => {
      const parts = l.split("\t");
      return { name: parts[0] ?? "", path: parts[1] ?? "", exists: parts[2] === "1" };
    });
}

/** The outcome of reading a `travsr lang list --json` payload.
 *
 *  #755: parsing used to be a bare `JSON.parse(...) as LangInfo[]`, so a binary
 *  whose rows predate the fields the panel reads produced a full table of
 *  silently wrong cells rather than an error. The rows and the verdict on their
 *  shape are returned together so a caller cannot use one without the other. */
export interface LangListParse {
  /** The rows, exactly as the binary sent them. Empty on a parse failure. */
  langs: LangInfo[];
  /** Contract fields absent from the rows, or `[]` when the shape is current.
   *  A field counts as missing only when NO row carries it: one odd row is a
   *  data quirk, all rows agreeing is a different binary. */
  missingFields: string[];
  /** The contract revision the binary reported, when it reported one. */
  reportedContract?: number;
}

/**
 * Validate the shape of `travsr lang list --json` rows against the contract the
 * Health page's Languages section renders (#755).
 *
 * Keys on field presence, not on the version string: an npm-bundled build and a
 * current one both self-report `1.0.0` while emitting different shapes, so a
 * version comparison cannot see the skew. A field present but `null` still
 * counts as reported — `unavailableTarget` is legitimately null.
 *
 * Extra fields a NEWER binary sends are not an error: this checks that the
 * fields this panel needs are there, never that nothing else is.
 */
export function langContractSkew(rows: unknown[]): string[] {
  if (rows.length === 0) return [];
  const objects = rows.filter(
    (r): r is Record<string, unknown> => typeof r === "object" && r !== null
  );
  if (objects.length === 0) return [...LANG_CONTRACT_FIELDS];
  return LANG_CONTRACT_FIELDS.filter((f) => !objects.some((r) => f in r));
}

/** Parse `travsr lang list --json` into rows plus a verdict on their shape.
 *  Tolerates empty output and a non-JSON error blob (both yield no rows and no
 *  skew — "nothing came back" is not the same finding as "a stale binary
 *  answered", and only the latter should accuse a binary of being old). */
export function parseLangList(raw: string): LangListParse {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw.trim() || "[]");
  } catch {
    return { langs: [], missingFields: [] };
  }
  if (!Array.isArray(parsed)) return { langs: [], missingFields: [] };
  // The marker is per-row (the payload is a bare array, so there is no envelope
  // to put it in) and identical on every row. Read it off the first row that is
  // actually an object, so a malformed leading element does not hide it.
  const firstObject = (parsed as unknown[]).find(
    (r): r is Record<string, unknown> => typeof r === "object" && r !== null
  );
  const contract = firstObject?.["contract"];
  return {
    langs: parsed as LangInfo[],
    missingFields: langContractSkew(parsed),
    ...(typeof contract === "number" ? { reportedContract: contract } : {}),
  };
}

/** Parse `travsr lang list --json` output into LangInfo rows. Tolerates empty/error.
 *  Shape-blind by design — callers that render the rows must use
 *  [`parseLangList`] so they see the skew verdict too. */
export function parseAvailableLanguages(raw: string): LangInfo[] {
  return parseLangList(raw).langs;
}

/**
 * Wall-clock budget for `travsr lang list --json`.
 *
 * Deliberately far above the 4 s the other read-only `lang` calls use. This one
 * resolves every catalog entry's analyzer, which is a PATH sweep per language —
 * measured at ~17 s on Windows with a cold filesystem cache. Under the shared
 * short timeout the command was killed mid-flight and its partial output parsed
 * as "no languages available", so the panel reported an empty catalog on exactly
 * the machines slow enough to need the information (#755). It also silently
 * defeated the contract check below, whose one wrong answer is a false "current".
 */
const LANG_LIST_TIMEOUT_MS = 60_000;

/**
 * #755: ask a resolved binary whether its `lang list --json` speaks the contract
 * this extension renders.
 *
 * Runs the same read-only command the Health page runs, with no cwd — the
 * per-repo column is irrelevant to a shape check, and a repo-less probe works
 * before any workspace is chosen. Returns `[]` for "current" and the missing
 * field names otherwise.
 *
 * A binary that fails to spawn, times out, or prints an error blob returns `[]`:
 * that is a different problem with its own recovery path (`assertExecutableBinary`,
 * the download flow), and accusing it of being stale would misdirect the user.
 */
export async function probeLangListContract(
  binary: string
): Promise<{ missingFields: string[]; reportedContract?: number }> {
  const { stdout, code } = await spawnLangCommandResult(
    binary,
    ["lang", "list", "--json"],
    undefined,
    LANG_LIST_TIMEOUT_MS
  );
  if (code !== 0) return { missingFields: [] };
  // stdout only: a stray stderr line would fail the parse and be read as "no
  // skew", i.e. as a clean bill of health for a binary that never got checked.
  const parsed = parseLangList(stdout);
  if (parsed.langs.length === 0) return { missingFields: [] };
  return {
    missingFields: parsed.missingFields,
    ...(parsed.reportedContract !== undefined
      ? { reportedContract: parsed.reportedContract }
      : {}),
  };
}

/** #755: the one wording for a contract-skewed binary, shared by the activation
 *  gate and anything else that has to say it. Names the binary, what it failed to
 *  report, and that the panel needs a newer one — never a bare "update travsr",
 *  because both builds self-report the same version string and the user has no
 *  way to tell them apart from that alone. */
export function contractSkewMessage(
  binary: string,
  missingFields: string[],
  reportedContract?: number
): string {
  const rev =
    reportedContract === undefined
      ? `reports no lang-list contract revision (this extension needs ${LANG_CONTRACT_VERSION})`
      : `reports lang-list contract revision ${reportedContract}, but this extension needs ${LANG_CONTRACT_VERSION}`;
  return (
    `Travsr: the travsr binary at ${binary} is older than this extension expects; it ${rev}. ` +
    `Missing: ${missingFields.join(", ")}. The Languages section of the Health panel is held back until a current ` +
    `binary is resolved; indexing and search are unaffected.`
  );
}

/** Human-readable "time ago" for a timestamp in ms. */
export function timeAgo(ms: number): string {
  const secs = Math.floor(ms / 1000);
  if (secs < 60) return `${secs}s ago`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  return `${Math.floor(hrs / 24)}d ago`;
}

/** Bytes read per backward step when scanning a log file from its end. */
const LOG_CHUNK_BYTES = 64 * 1024;

/** Hard ceiling on how far back a single file is scanned.
 *
 *  The backward scan normally stops as soon as it has enough newlines, so this
 *  only binds on a pathological file: one enormous line, or a file with no
 *  newlines at all, where "scan until enough lines" would otherwise read all
 *  50 MB of a rotated log into a webview.
 *
 *  It bounds the READ. What the read produces needs bounding separately, which
 *  is what `LOG_OVERSIZED_PREVIEW_CHARS` is for: when the ceiling lands inside a
 *  line, the window holds no complete entry, and handing the fragment on as one
 *  cost about 32 MB of HTML from a single row.
 *
 *  This is the one place the panel's reader diverges from `append_file_tail` by
 *  design. The Rust has no ceiling, so `line_start_from_end` falls through to 0
 *  and `travsr daemon logs` reads such a file from byte zero. A terminal can
 *  afford that and a webview cannot. */
export const LOG_MAX_BYTES_PER_FILE = 8 * 1024 * 1024;

/** How much of an oversized line is shown when the ceiling leaves no complete
 *  line in reach. One bounded row, rather than the two wrong answers: nothing
 *  at all, which renders "No daemon log yet" over a file with readable lines
 *  earlier in it, or the whole window as a single entry. */
export const LOG_OVERSIZED_PREVIEW_CHARS = 2000;

/** The one row a file gets when the byte ceiling leaves no complete line in it.
 *
 *  Deliberately NOT valid daemon JSON. This is the panel accounting for itself,
 *  not something the daemon wrote, and `parseLogLine` renders an unparseable
 *  line as its own message with no severity pill, which is exactly how it should
 *  read. */
function oversizedLineNotice(scanned: string): string {
  const mb = LOG_MAX_BYTES_PER_FILE / (1024 * 1024);
  return (
    `[travsr] a single log line is longer than this panel's ${mb} MB per-file scan, ` +
    `so no complete entry was within reach. Run \`travsr daemon logs\` to read the ` +
    `file without that limit. First ${LOG_OVERSIZED_PREVIEW_CHARS} characters: ` +
    scanned.slice(0, LOG_OVERSIZED_PREVIEW_CHARS)
  );
}

/** Every rotated log file in `dir`, oldest first.
 *
 *  `tracing-appender` suffixes with an ISO date, which sorts lexicographically
 *  in chronological order, so no date parsing is required. Mirrors
 *  `log_files` in `crates/travsr-daemon/src/logfile.rs`, including its
 *  files-only guard: a directory called `daemon.log.something` is not a log. */
function daemonLogFiles(dir: string): string[] {
  try {
    return fs
      .readdirSync(dir, { withFileTypes: true })
      .filter((e) => e.isFile() && e.name.startsWith("daemon.log"))
      .map((e) => e.name)
      .sort();
  } catch {
    return [];
  }
}

/** The last `maxLines` complete lines of one log file, oldest first.
 *
 *  Scans backwards in chunks and stops as soon as enough newlines are in hand,
 *  so cost is proportional to the tail read rather than to file size: a 400 MB
 *  log tails as fast as a 4 KB one. Ports `line_start_from_end` /
 *  `append_file_tail` from `logfile.rs`.
 *
 *  A scan that starts mid-file drops everything before the first newline,
 *  because a seek lands in the middle of a line and half an entry is not an
 *  entry. */
function readFileTailLines(file: string, maxLines: number): string[] {
  let fd: number;
  let size: number;
  try {
    size = fs.statSync(file).size;
    if (size === 0) return [];
    fd = fs.openSync(file, "r");
  } catch {
    return [];
  }
  try {
    let start = size;
    // Bytes, not strings. Decoding each chunk on its own would split any UTF-8
    // sequence that straddles a chunk boundary into two replacement characters,
    // destroying the character: a path like `travsr-café` in a log line would
    // render as `caf??` in the panel while `travsr daemon logs` printed it
    // correctly, which is exactly the divergence this reader exists to avoid.
    // `append_file_tail` accumulates bytes and decodes once for the same
    // reason, and its lossy decode is there for a torn write, not for a split
    // we inflicted on ourselves.
    //
    // Counting newlines per chunk rather than rescanning the accumulated text
    // keeps this linear. Rescanning made the scan quadratic in chunk count on
    // the one path the byte ceiling exists to bound (a file with no newlines),
    // which cost about a second of synchronous extension-host work at the 8 MB
    // limit.
    const parts: Buffer[] = [];
    let newlines = 0;
    // Which exit the loop took matters: "I found enough lines" and "the ceiling
    // stopped me" leave the window in different states, and treating them as the
    // same exit is what broke the partial-line drop below.
    let ceilingHit = false;
    for (;;) {
      const next = Math.max(0, start - LOG_CHUNK_BYTES);
      const len = start - next;
      if (len <= 0) break;
      const buf = Buffer.alloc(len);
      fs.readSync(fd, buf, 0, len, next);
      parts.push(buf);
      for (let i = 0; i < len; i++) {
        if (buf[i] === 0x0a) newlines++;
      }
      start = next;
      if (start === 0) break;
      if (size - start >= LOG_MAX_BYTES_PER_FILE) {
        ceilingHit = true;
        break;
      }
      // Strictly greater: the extra newline is the one terminating the line
      // *before* the first one we want, which is what proves it is whole.
      if (newlines > maxLines) break;
    }
    parts.reverse();
    const scanned = Buffer.concat(parts).toString("utf8");

    // A scan that starts mid-file drops everything before the first newline,
    // because a seek lands in the middle of a line and half an entry is not an
    // entry. `indexOf` returning -1 does NOT mean "cut at zero": it means the
    // window holds no boundary at all, which happens only when the ceiling
    // landed inside one line. `slice(indexOf + 1)` read that as cut-at-zero and
    // kept the fragment, handing an 8 MB partial line back as an entry.
    let body = scanned;
    if (start > 0) {
      const nl = body.indexOf("\n");
      body = nl === -1 ? "" : body.slice(nl + 1);
    }

    const lines = body.split("\n").filter((l) => l.trim() !== "");
    if (lines.length === 0 && ceilingHit) {
      // The ceiling stopped the scan inside a line longer than the whole budget,
      // so there is no complete entry in the window: either no boundary at all,
      // or only the newline closing that one line. Reporting nothing renders
      // "No daemon log yet" over a file whose earlier lines are perfectly
      // readable, which is the symptom this reader exists to remove arriving by
      // another route. One bounded, marked row instead.
      return [oversizedLineNotice(scanned)];
    }
    return lines.slice(-maxLines);
  } catch {
    return [];
  } finally {
    try {
      fs.closeSync(fd);
    } catch {
      /* already closed */
    }
  }
}

/**
 * Read the tail of the daemon log, across rotated files.
 *
 * `daemon.log.<UTC-DATE>` is JSON lines, one object per line, rotated daily
 * with seven files kept. "The last 500 lines" is therefore rarely 500 lines of
 * one file: shortly after 00:00 UTC today's file holds a handful and the rest
 * of the answer sits in yesterday's. Reading only the newest file returned
 * short without saying so, so a healthy daemon read as one that had logged
 * almost nothing.
 *
 * So older files are walked, newest first, each supplying only what the newer
 * ones could not, stopping as soon as the request is satisfied. This mirrors
 * `LogTail::backfill` in `crates/travsr-daemon/src/logfile.rs`, which is what
 * `travsr daemon logs --lines N` already does correctly; the panel had its own
 * reader and never got the fix.
 *
 * Lines are returned oldest first, each tagged with the date of the file it
 * came from so the panel can show where one day ends and the next begins.
 *
 * One deliberate divergence from `backfill`: there, `lines == 0` means "the
 * whole retained history", which is a reasonable thing to pipe to a terminal
 * and not a reasonable thing to build a DOM out of. Here 0 reads nothing, and
 * the panel's widest option resolves to `LOG_MAX_LINES` instead. Unreachable
 * from the UI either way, since the smallest option is 100 and `setLogLines`
 * clamps to at least 1.
 *
 * No longer the panel's reader. The File control reads one file at a time via
 * `readDaemonLogFile`, because a stream spanning rotations gave no way to ask
 * for a particular day. This stays as the tested port of `backfill`, which is
 * what `travsr daemon logs` still does, and its tests hold the two readers to
 * the same answer on the same fixture.
 */
export function readDaemonLogTail(repoRoot: string, maxLines = 500): LogEntry[] {
  const dir = path.join(repoRoot, ".travsr");
  const files = daemonLogFiles(dir);
  if (files.length === 0) return [];

  const want = Math.min(Math.max(maxLines, 0), LOG_MAX_LINES);
  if (want === 0) return [];

  // Newest first, each older file supplying only the shortfall. Older files are
  // never opened once the newest satisfies the request.
  const chunks: LogEntry[][] = [];
  let needed = want;
  for (let i = files.length - 1; i >= 0 && needed > 0; i--) {
    const name = files[i];
    const lines = readFileTailLines(path.join(dir, name), needed);
    needed -= lines.length;
    // Concatenating arrays rather than text is deliberate: a rotated file whose
    // last write was torn has no trailing newline, and gluing its text onto the
    // next file's would fuse two entries into one line.
    chunks.push(lines.map((l) => ({ ...parseLogLine(l), day: logFileDay(name) })));
  }
  chunks.reverse();
  return chunks.flat();
}

/** The date a rotated log file covers, from its `daemon.log.<DATE>` name.
 *  Falls back to the whole name so a file with an unexpected suffix still
 *  groups deterministically instead of collapsing into its neighbours. */
function logFileDay(fileName: string): string {
  const suffix = fileName.slice("daemon.log".length).replace(/^\./, "");
  return suffix || fileName;
}

/** `today` or `yesterday` for a log file's date, `undefined` for anything else.
 *
 *  Both dates are UTC, because `rolling::daily` names files by the UTC date.
 *  That is worth saying out loud: at UTC+5:30 the file called `2026-08-22`
 *  holds 05:30 on the 22nd through 05:29 on the 23rd local, so "today" here
 *  means the current UTC day and not the reader's. The File control says
 *  "UTC days" beside itself for exactly this reason.
 *
 *  Only those two labels. Files are named for days the daemon ran rather than
 *  for consecutive days, so seven files can span months and a counted label
 *  ("3 days ago") on the third entry would usually be wrong. Everything older
 *  shows its date and nothing else.
 *
 *  `todayUtc` is a parameter rather than a call to the clock so this is testable
 *  without freezing time. */
export function logFileRelativeDay(day: string, todayUtc: string): string | undefined {
  const ISO_DAY = /^\d{4}-\d{2}-\d{2}$/;
  if (!ISO_DAY.test(day) || !ISO_DAY.test(todayUtc)) return undefined;
  if (day === todayUtc) return "today";
  // Date arithmetic rather than string maths, so month and year ends work.
  const prev = new Date(`${todayUtc}T00:00:00Z`);
  prev.setUTCDate(prev.getUTCDate() - 1);
  return prev.toISOString().slice(0, 10) === day ? "yesterday" : undefined;
}

/** The rotated log files the panel's File control should offer, newest first.
 *
 *  Sizes come from `statSync`, which is metadata and costs nothing. Line counts
 *  deliberately do not: there is no way to know how many lines a file holds
 *  without reading all of it, so labelling every file with a count would open
 *  every file on every redraw. That is the opposite of what the tail reader is
 *  built to do, and `refreshOpenPanels` fires on `dbWatcher.onDidChange`, so it
 *  would run again on every reindex while the panel is open.
 *
 *  `onDisk` is the number of files actually present, so a capped list can say
 *  it is capped. It can exceed the returned length: `MAX_LOG_FILES` is enforced
 *  by the daemon's `prune`, not by the directory. */
export function daemonLogFileList(
  repoRoot: string,
  todayUtc: string = new Date().toISOString().slice(0, 10)
): { files: LogFileInfo[]; onDisk: number } {
  const dir = path.join(repoRoot, ".travsr");
  const names = daemonLogFiles(dir);
  // `daemonLogFiles` is oldest first; the newest file is the default selection
  // and belongs at the top of the list.
  const files = names
    .slice()
    .reverse()
    .slice(0, LOG_MAX_FILES_LISTED)
    .map((name) => {
      let size = 0;
      try {
        size = fs.statSync(path.join(dir, name)).size;
      } catch {
        // Raced with the daemon's prune between the listing and the stat. Zero
        // reads as "nothing in here", which is true enough of a file being
        // deleted, and is better than dropping the entry and renumbering the
        // list under the user's cursor.
      }
      const day = logFileDay(name);
      const rel = logFileRelativeDay(day, todayUtc);
      return rel === undefined ? { name, day, size } : { name, day, size, rel };
    });
  return { files, onDisk: names.length };
}

/** The last `maxLines` lines of one rotated log file, oldest first.
 *
 *  The panel's reader. Sits on the same backward chunked scan
 *  `readDaemonLogTail` uses, so cost is proportional to the tail taken rather
 *  than to file size, and the same per-file byte ceiling applies. Reading one
 *  file is strictly less work than spanning: the other files are never opened.
 *
 *  `fileName` is a bare name and is checked against the directory listing, not
 *  sanitised. It arrives from the webview, and an allowlist of names the
 *  directory actually reports cannot be walked out of, while a `..` test on a
 *  joined path has to be right about what the platform accepts in a name. An
 *  unknown name reads as empty. */
export function readDaemonLogFile(
  repoRoot: string,
  fileName: string,
  maxLines = 500
): LogEntry[] {
  const dir = path.join(repoRoot, ".travsr");
  if (!daemonLogFiles(dir).includes(fileName)) return [];
  const want = Math.min(Math.max(maxLines, 0), LOG_MAX_LINES);
  if (want === 0) return [];
  const day = logFileDay(fileName);
  return readFileTailLines(path.join(dir, fileName), want).map((l) => ({
    ...parseLogLine(l),
    day,
  }));
}

/**
 * One log line as the panel needs it.
 *
 * Rotated files written before the log became JSON are still on disk and are
 * still the only record of what happened then, so a line that does not parse is
 * carried through as its own text rather than dropped.
 */
export function parseLogLine(line: string): LogEntry {
  try {
    const e = JSON.parse(line) as {
      timestamp?: string;
      level?: string;
      target?: string;
      fields?: Record<string, unknown>;
    };
    if (typeof e.timestamp === "string" && typeof e.level === "string") {
      const fields = e.fields ?? {};
      // `repo` is dropped for the same reason the CLI renderer drops it: the
      // panel belongs to one repo and the reader opened it from inside that
      // repo, so restating the path on every line is spent width.
      const { message, event, repo: _repo, ...rest } = fields as Record<string, unknown>;
      return {
        // 24-hour, fixed width. `toLocaleTimeString` defaults to 12-hour with a
        // meridiem in most locales, which is wider and does not sort by eye.
        time: new Date(e.timestamp).toTimeString().slice(0, 8),
        level: e.level,
        target: shortTarget(e.target ?? ""),
        message: typeof message === "string" ? message : "",
        event: typeof event === "string" ? event : undefined,
        detail: Object.entries(rest)
          .map(([k, v]) => `${k}=${String(v)}`)
          .join(" "),
        iso: e.timestamp,
        raw: line,
      };
    }
  } catch {
    // fall through
  }
  return { time: "", level: "", target: "", message: line, detail: "", iso: "", raw: line };
}

/** `travsr_plugin_host::registry` is 29 characters that say "plugin host". */
function shortTarget(target: string): string {
  return target.split("::")[0].replace(/^travsr_/, "").replace(/_/g, "-");
}

/** Build the stats dashboard view from `get_graph_stats` + local graph.db. */
/** Parse the `get_index_status` envelope. Anything unparseable is reported as
 *  unavailable rather than as a healthy default. */
export function parseIndexHealth(raw: string): IndexHealth {
  const body = stripEnvelope(raw).trim();
  if (body === "") return UNKNOWN_INDEX;
  try {
    const p = JSON.parse(body) as {
      indexed_commit?: unknown;
      head_commit?: unknown;
      staleness?: { behind_by?: unknown; is_stale?: unknown; working_tree_dirty?: unknown };
      phase_a?: { state?: unknown };
    };
    const s = p.staleness ?? {};
    const str = (v: unknown): string => (typeof v === "string" ? v : "");
    const num = (v: unknown): number | null => (typeof v === "number" ? v : null);
    const bool = (v: unknown): boolean | null => (typeof v === "boolean" ? v : null);
    return {
      isStale: bool(s.is_stale),
      behindBy: num(s.behind_by),
      indexedCommit: str(p.indexed_commit),
      headCommit: str(p.head_commit),
      phaseA: str(p.phase_a?.state),
      workingTreeDirty: bool(s.working_tree_dirty),
      available: true,
    };
  } catch {
    return UNKNOWN_INDEX;
  }
}

/** Map `travsr lang list --json` rows to the Health page's Languages table.
 *
 *  Pure, so every branch of the table can be tested without spawning the CLI.
 *  Two facts stay separate: `installed` (the analyzer is on this machine) and
 *  `repoState` (full analysis is on for this repository). They used to be
 *  collapsed into one column, and a language that was installed everywhere but
 *  off for the repo was offered an install it did not need.
 *
 *  #755: every row is `JSON.parse` of another process's stdout, so at runtime a
 *  field can be absent whatever `LangInfo` declares. Absent values read as
 *  "unknown", never as a value that looks like a real answer. */
export function parseRepoLanguages(raw: string): Set<string> {
  // `repo_languages` prints one `language\tcount` line per language the graph
  // holds nodes for. Names are the same canonical tags the catalog uses.
  const out = new Set<string>();
  for (const line of stripEnvelope(raw).split("\n")) {
    const lang = line.split("\t")[0]?.trim().toLowerCase();
    if (lang) out.add(lang);
  }
  return out;
}

export function buildLanguageRows(
  langs: LangInfo[],
  diags: Diagnostic[],
  /** Languages the graph found in this repo (`repo_languages`). Empty means the
   *  graph could not say, and then nothing is gated: hiding every action on a
   *  repo that has not been indexed yet would leave the user with no way in. */
  detected: ReadonlySet<string> = new Set()
): LanguageRow[] {
  const REPO_STATES: ReadonlySet<string> = new Set([
    "always_on", "enabled", "needs_analyzer", "not_enabled", "no_repo",
  ]);
  const OS_NAMES: Record<string, string> = { windows: "Windows", macos: "macOS", linux: "Linux" };
  return langs.map((l) => {
    const sent = l as unknown as Record<string, unknown>;
    const statusKnown = typeof sent["status"] === "string";
    const full = l.status === "active";
    // No build for this OS: full analysis can never run here, so the row never
    // offers an install. `status` and `availableOnThisPlatform` come from one CLI
    // predicate and always agree; check both defensively.
    const availableHere = l.status !== "unsupported" && sent["availableOnThisPlatform"] !== false;
    const osName = OS_NAMES[l.unavailableTarget ?? ""] ?? "";
    const repoState: LanguageRow["repoState"] = REPO_STATES.has(String(sent["repoState"]))
      ? l.repoState
      : "unknown";
    const installed = sent["installed"] === true;
    const builtin = sent["builtin"] === true;
    // Present in this repo, as the graph sees it. A language the repo does not
    // contain is not shown: turning on an analyzer for files that do not exist
    // changes nothing, and sixteen rows for a three-language repo is noise.
    const inRepo = detected.size === 0 || detected.has(l.language.toLowerCase());
    // Cross-referenced against the warnings on this same page. `lang list` can
    // call a language active while `travsr status` warns that it ran and
    // resolved nothing; both are true, and the row has to say so rather than
    // showing a bare tick beside a contradicting card.
    const flagged = diags.some((d) =>
      new RegExp(`\\b${l.language.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\b`, "i").test(
        `${d.title} ${d.hint}`
      )
    );
    const analysis = !statusKnown ? "unknown" : full ? "full" : "structural only";
    // The CLI's own sentence. `lang list` never counts symbols, so anything
    // numeric here would be invented.
    const statusLine =
      typeof sent["statusLine"] === "string" && l.statusLine !== ""
        ? l.statusLine
        : full
          ? "installed and enabled"
          : analysis;
    const prerequisites =
      sent["prerequisites"] === undefined || sent["prerequisites"] === null
        ? "unknown"
        : l.prerequisites && l.prerequisites !== "none"
          ? l.prerequisites
          : "";
    // One action per row, and it has to be the right one: an install where the
    // analyzer is already on the machine downloads nothing and changes nothing.
    let fix: LanguageRow["fix"] = "none";
    if (!availableHere || !inRepo) fix = "none";
    else if (l.status === "needs_consent") fix = "permission";
    else if (!installed || repoState === "needs_analyzer") fix = "install";
    else if (repoState === "not_enabled") fix = "enable";
    else if (!full || flagged) fix = "semantic";
    return {
      language: l.language,
      analysis,
      full,
      statusLine,
      flagged,
      installed,
      repoState,
      availableHere,
      osName,
      prerequisites,
      builtin,
      inRepo,
      fix,
      canDisable: availableHere && full && !builtin,
    };
  });
}

/** Gather everything the Health page's sections need.
 *
 *  Each source is independent and every one of them can fail on its own: the
 *  daemon can be down while the CLI still answers, a binary can be too old for
 *  a tool, a config file can be missing. So each lands in its own field and a
 *  failure becomes `null`, which the page renders as "could not read this"
 *  rather than as a clean bill of health. Nothing here throws.
 *
 *  The spawns run together rather than in sequence: `lang list` alone takes
 *  about 17 seconds on Windows because it sweeps PATH per catalog language, and
 *  serialising four of those would make opening the panel feel broken.
 */
export async function gatherHealth(
  client: McpClient,
  binary: string,
  root: string | undefined,
  log: LogEntry[],
  dbSize: string,
  logFiles: { files: LogFileInfo[]; onDisk: number },
  activeRepoName: string,
  diags: Diagnostic[] = []
): Promise<HealthData> {
  // Whether the extension can query, which is not the same question as whether
  // the daemon is up: the extension's own `travsr mcp --stdio` child opens the
  // database directly and keeps answering with no daemon anywhere.
  const mcpConnected = client.isConnected();

  // Read from the log, because it is the one source that still answers after
  // the process has gone, which is exactly when this page gets opened.
  const findEvent = (re: RegExp): string => {
    for (const e of log) {
      const m = re.exec(e.message ?? "");
      if (m) return m[1] ?? "";
    }
    return "";
  };
  const daemonPid = findEvent(/\bpid[= ](\d+)/);
  const daemonStopped = (() => {
    const stop = log.find((e) => /daemon stopped|shutting down/i.test(e.message ?? ""));
    if (stop === undefined) return "";
    // Date and time, local. A bare clock time reads as "today" on a panel that
    // is often opened days after the daemon last ran; the log's own RFC3339
    // stamp carries the day, so use it and fall back to the clock only when the
    // stamp does not parse.
    const d = new Date(stop.iso);
    if (Number.isNaN(d.getTime())) return stop.time;
    const p = (n: number): string => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
  })();
  const lastEditor = (() => {
    const ed = log.find((e) => /editor (attached|detached)/i.test(e.message ?? ""));
    if (ed === undefined) return "";
    const sess = /session=([\w-]+)/.exec(ed.message ?? "");
    const verb = /detached/i.test(ed.message ?? "") ? "detached" : "attached";
    return `${sess ? sess[1] : "editor"} ${verb} ${ed.time ?? ""}`.trim();
  })();

  const settle = async <T>(p: Promise<T>, fallback: T): Promise<T> => {
    try {
      return await p;
    } catch {
      return fallback;
    }
  };

  const [versionOut, daemonOut, embedOut, langOut, healthRaw, reposRaw, repoLangsRaw] = await Promise.all([
    settle(spawnLangCommand(binary, ["--version"], root ?? process.cwd()), ""),
    settle(spawnLangCommand(binary, ["daemon", "status"], root ?? process.cwd(), 8_000), ""),
    settle(spawnLangCommand(binary, ["embed", "list", "--json"], root ?? process.cwd()), ""),
    settle(spawnLangCommand(binary, ["lang", "list", "--json"], root ?? process.cwd(), 60_000), ""),
    settle(client.callTool("get_graph_health"), ""),
    settle(client.callTool("repos_list"), ""),
    // Which languages the graph actually found in this repo, so the Languages
    // table only offers to install or enable an analyzer the repo can use.
    settle(client.callTool("repo_languages"), ""),
  ]);

  const binaryVersion = (/(\d+\.\d+\.\d+[^\s]*)/.exec(versionOut) ?? [])[1] ?? "";

  // The daemon's own answer, which is the same one `travsr daemon status`
  // gives in a terminal. "starting" counts as up: the control socket is not
  // ready yet but the process is alive and will start watching.
  const daemonLine = (/^daemon:\s*(.+)$/m.exec(daemonOut) ?? [])[1]?.trim() ?? "";
  const daemonRunning = /^running|^starting/.test(daemonLine);
  const daemonDetail = daemonLine;

  // Sidecars. `embed list --json` reports every model in the catalog; the panel
  // only needs whether the active one is on disk.
  let sidecars: SidecarRow[] | null = null;
  if (embedOut.trim() !== "") {
    try {
      const models = JSON.parse(embedOut) as Array<{ installed?: boolean; active?: boolean; id?: string }>;
      const active = models.find((m) => m.active === true);
      const anyInstalled = models.some((m) => m.installed === true);
      const embedOk = active?.installed === true;
      sidecars = [
        {
          name: "embed",
          state: embedOk
            ? `${active?.id ?? "model"}, ready`
            : anyInstalled
              ? "installed but not active"
              : "not installed, semantic search off",
          ok: embedOk,
          action: embedOk ? "restartEmbed" : "installEmbed",
        },
      ];
    } catch {
      sidecars = null;
    }
  }

  // Languages. The same JSON `travsr lang list` prints, mapped by
  // `buildLanguageRows`, so the table can never disagree with the terminal. A
  // payload whose rows predate the fields read here (#755) is withheld and
  // reported as a skew rather than rendered as guesses.
  let languages: LanguageRow[] | null = null;
  let languagesSkew: LangContractSkew | undefined;
  if (langOut.trim() !== "") {
    const parsed = parseLangList(langOut);
    if (parsed.missingFields.length > 0) {
      languagesSkew = {
        missingFields: parsed.missingFields,
        binary,
        ...(parsed.reportedContract !== undefined ? { reportedContract: parsed.reportedContract } : {}),
      };
    } else if (parsed.langs.length > 0) {
      languages = buildLanguageRows(parsed.langs, diags, parseRepoLanguages(repoLangsRaw));
    }
  }

  // Integrity, from the daemon's own report.
  let integrity: IntegrityView | null = null;
  const healthBody = stripEnvelope(healthRaw).trim();
  if (healthBody !== "") {
    try {
      const h = JSON.parse(healthBody) as {
        healthy?: boolean;
        ghost_paths?: { count?: number; sample?: string[] };
        lexical_index_parity?: { ok?: boolean };
      };
      integrity = {
        healthy: typeof h.healthy === "boolean" ? h.healthy : null,
        ghostCount: typeof h.ghost_paths?.count === "number" ? h.ghost_paths.count : null,
        ghostSample: Array.isArray(h.ghost_paths?.sample) ? h.ghost_paths.sample.slice(0, 3) : [],
        lexicalOk: typeof h.lexical_index_parity?.ok === "boolean" ? h.lexical_index_parity.ok : null,
        dbSize,
        logSize: formatLogSize(logFiles.files.reduce((a, f) => a + f.size, 0)),
        logFiles: logFiles.files.length,
      };
    } catch {
      integrity = null;
    }
  }

  const repos = reposRaw.trim() === "" ? null : parseReposList(reposRaw);

  // Agents. A missing config file and a config with no travsr entry are
  // different answers, so they are reported differently.
  let agents: AgentRow[] | null = null;
  try {
    agents = resolveAgentTargets(root).map((t) => {
      if (!fs.existsSync(t.configPath)) {
        return { name: t.name, registered: false, detail: "config file not found" };
      }
      try {
        const txt = fs.readFileSync(t.configPath, "utf8");
        const registered = /"travsr"\s*:/.test(txt);
        return {
          name: t.name,
          registered,
          detail: registered ? "registered" : "not registered",
        };
      } catch {
        return { name: t.name, registered: false, detail: "config not readable" };
      }
    });
  } catch {
    agents = null;
  }

  // The hook is a file, so this answers whether commits actually refresh the
  // graph rather than whether the feature exists.
  let commitHook: boolean | null = null;
  if (root !== undefined) {
    try {
      const hook = path.join(root, ".git", "hooks", "post-commit");
      commitHook = fs.existsSync(hook) && /travsr/.test(fs.readFileSync(hook, "utf8"));
    } catch {
      commitHook = null;
    }
  }

  const newest = logFiles.files[0];
  return {
    daemonRunning,
    daemonDetail,
    mcpConnected,
    daemonPid,
    daemonStopped,
    lastEditor,
    binaryVersion,
    logFileName: newest?.name ?? "",
    logFileSize: newest ? formatLogSize(newest.size) : "",
    commitHook,
    sidecars,
    agents,
    repos,
    activeRepo: activeRepoName,
    languages,
    ...(languagesSkew !== undefined ? { languagesSkew } : {}),
    integrity,
  };
}

export function buildStatsView(raw: string): StatsView {
  const lines = stripEnvelope(raw).split("\n");
  const field = (key: string): string => {
    for (const l of lines) {
      const m = new RegExp(`^${key}:\\s*(.+)$`).exec(l.trim());
      if (m) return m[1];
    }
    return "—";
  };
  let dbSize = "—";
  let lastIndexed = "—";
  const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  if (root) {
    try {
      const st = fs.statSync(path.join(root, ".travsr", "graph.db"));
      dbSize = `${(st.size / 1_048_576).toFixed(1)} MB`;
      lastIndexed = timeAgo(Date.now() - st.mtimeMs);
    } catch {
      // graph.db absent — leave dashes.
    }
  }
  return {
    nodes: field("nodes"),
    edges: field("edges"),
    schemaVersion: field("schema_version"),
    dbSize,
    lastIndexed,
  };
}

/** Open a file (repo-relative or absolute) at an optional 1-based line. */
export async function openAtLine(filePath: string, line?: number): Promise<void> {
  const root = vscode.workspace.workspaceFolders?.[0]?.uri;
  const uri = filePath.startsWith("/")
    ? vscode.Uri.file(filePath)
    : root
      ? vscode.Uri.joinPath(root, filePath)
      : vscode.Uri.file(filePath);
  if (line != null) {
    const doc = await vscode.workspace.openTextDocument(uri);
    const lineIdx = Math.max(0, line - 1);
    await vscode.window.showTextDocument(doc, {
      selection: new vscode.Range(lineIdx, 0, lineIdx, 0),
    });
  } else {
    await vscode.commands.executeCommand("vscode.open", uri);
  }
}

function escHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

/**
 * Resolve an import specifier string from `get_dependencies` output to a
 * workspace-relative file path, or `undefined` when not resolvable (external
 * package, stdlib, crate, or local-but-missing).
 *
 * Spec format: `kind:specifier` (e.g. `import:./status`, `use:std::io::Write`).
 * Only `.`-relative and `/`-absolute specifiers are attempted — everything else
 * is an external dependency that has no local file to open.
 *
 * `existsCheck` is injectable so this function can be unit-tested without disk.
 */
export function resolveDepSpec(
  spec: string,
  sourceAbsPath: string,
  existsCheck: (p: string) => boolean = fs.existsSync
): string | undefined {
  const colonIdx = spec.indexOf(":");
  const raw = colonIdx >= 0 ? spec.slice(colonIdx + 1) : spec;
  if (!raw.startsWith(".") && !raw.startsWith("/")) return undefined;

  const dir = path.dirname(sourceAbsPath);
  const candidates = [
    "",
    ".ts",
    ".tsx",
    ".js",
    ".jsx",
    "/index.ts",
    "/index.tsx",
    "/index.js",
    "/index.jsx",
  ];
  for (const ext of candidates) {
    const abs = path.resolve(dir, raw + ext);
    if (existsCheck(abs)) {
      const wsRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      return wsRoot ? path.relative(wsRoot, abs) : abs;
    }
  }
  return undefined;
}

/** An entry in the dep list webview, resolved path is clickable, absent = dimmed. */
export interface DepEntry {
  display: string;
  path?: string;
}

/**
 * Build a clickable dep list webview with resolved paths. Entries with a `path`
 * are clickable; entries without are shown dimmed (external/stdlib/crate deps).
 * `transitive` entries are collapsed under a `<details>` summary.
 */
export function buildDepListHtml(
  title: string,
  direct: DepEntry[],
  transitive: DepEntry[]
): string {
  const li = (e: DepEntry): string => {
    if (e.path) {
      return `<li class="dep" data-path="${escHtml(e.path)}">${escHtml(e.display)}</li>`;
    }
    return `<li class="dep-ext" title="External / stdlib, no local file">${escHtml(e.display)}</li>`;
  };
  const directRows = direct.map(li).join("\n") || "<li><em>none</em></li>";
  const transitiveBlock = transitive.length
    ? `<details><summary>Transitive (${transitive.length})</summary>
<ul class="deps">${transitive.map(li).join("\n")}</ul></details>`
    : "";
  return `<!DOCTYPE html><html><head>
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
  :root { --bg:#141414; --bg-elev:#1a1a1a; --border:#4d4d4d; --fg:#f6f1ed; --fg-muted:#c8b7ab; --green:#86df86; }
  /* Keyed on the theme kind VS Code stamps on the body, not on
     prefers-color-scheme: that resolves from the OS appearance in an Electron
     window, so a light desktop with a dark editor theme drew this light. See the
     longer note in webviewShell. */
  body.vscode-light, body.vscode-high-contrast-light { --bg:#f6f1ed; --bg-elev:#fbfaf9; --border:#e2d4ca; --fg:#1a1a1a; --fg-muted:#705f54; --green:#429429; }
  body { font-family: var(--vscode-font-family); padding: 16px; color: var(--fg); background: var(--bg); }
  h3 { margin: 0 0 12px; font-size: 14px; }
  ul.deps { list-style: none; margin: 0; padding: 0; }
  li.dep { font-family: var(--vscode-editor-font-family, monospace); padding: 3px 6px;
    cursor: pointer; border-radius: 4px; font-size: 12px; }
  li.dep:hover { background: var(--bg-elev); color: var(--green); }
  li.dep-ext { font-family: var(--vscode-editor-font-family, monospace); padding: 3px 6px;
    border-radius: 4px; font-size: 12px; color: var(--fg-muted); cursor: default; }
  summary { cursor: pointer; margin: 12px 0 6px; font-weight: 600; font-size: 12px; color: var(--fg-muted); }
</style></head><body>
<h3>${title}</h3>
<ul class="deps">${directRows}</ul>
${transitiveBlock}
<script>
  const vscode = acquireVsCodeApi();
  document.querySelectorAll('li.dep').forEach(function(el){
    el.addEventListener('click', function(){
      vscode.postMessage({ command: 'open', path: el.getAttribute('data-path') });
    });
  });
</script>
</body></html>`;
}

/**
 * Build a clickable file-list webview (used by blast radius etc.). Each entry
 * posts `{command:'open',path}` back to the extension.
 */
export function buildClickableFileListHtml(
  title: string,
  direct: string[],
  transitive: string[]
): string {
  const li = (f: string): string => {
    const clean = f.replace(/^\s*↳\s*/, "").trim();
    return `<li class="dep" data-path="${escHtml(clean)}">${escHtml(clean)}</li>`;
  };
  const directRows = direct.map(li).join("\n") || "<li><em>none</em></li>";
  const transitiveBlock = transitive.length
    ? `<details><summary>Transitive (${transitive.length})</summary>
<ul class="deps">${transitive.map(li).join("\n")}</ul></details>`
    : "";
  return `<!DOCTYPE html><html><head>
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<style>
  body { font-family: var(--vscode-font-family); padding: 16px; color: var(--vscode-foreground); }
  h3 { margin: 0 0 12px; }
  ul.deps { list-style: none; margin: 0; padding: 0; }
  li.dep { font-family: var(--vscode-editor-font-family, monospace); padding: 3px 6px; cursor: pointer; border-radius: 4px; }
  li.dep:hover { background: var(--vscode-list-hoverBackground); }
  summary { cursor: pointer; margin: 12px 0 6px; font-weight: 600; }
</style></head><body>
<h3>${title}</h3>
<ul class="deps">${directRows}</ul>
${transitiveBlock}
<script>
  const vscode = acquireVsCodeApi();
  document.querySelectorAll('li.dep').forEach(function(el){
    el.addEventListener('click', function(){
      vscode.postMessage({ command: 'open', path: el.getAttribute('data-path') });
    });
  });
</script>
</body></html>`;
}

// ── Command registrations ───────────────────────────────────────────────────

// Language tokens to strip from symbol queries so `fn foo` → `foo` matches backend inference.
// Mirrors the set in travsr-mcp's infer_language_from_query (tools.rs).
const LANG_TOKENS = new Set([
  "fn","func","function","def","class","struct","trait","impl","interface",
  "type","const","let","var","mod","module","pub","pub(crate)","async","static",
]);

/** Strip leading language tokens from a query string (client-side ranking parity). */
export function stripLangTokens(query: string): string {
  const words = query.trim().split(/\s+/);
  const stripped = words.filter((w) => !LANG_TOKENS.has(w.toLowerCase()));
  return (stripped.length > 0 ? stripped : words).join(" ");
}

// Session-level rate-limit for synonym suggestions: one per (query, symbolName) pair.
const synonymPromptedPairs = new Set<string>();

/**
 * travsr.askSymbol — live ranked symbol search. Reuses `get_graph_json` (its
 * nodes already carry path + line), debounced 250ms, with a stale-response
 * guard so out-of-order daemon replies never clobber newer input.
 *
 * Applies client-side language-token stripping (ITEM 4) before the query so
 * `fn foo` ranks the same as `foo` (mirrors travsr-mcp's infer_language_from_query).
 *
 * On accept: if the typed token differs from the selected symbol name, offers
 * a one-click synonym add (ITEM 5) gated by travsr.suggestSynonyms.
 */
export function registerAskSymbol(client: McpClient): vscode.Disposable {
  return vscode.commands.registerCommand("travsr.askSymbol", () => {
    const qp = vscode.window.createQuickPick<SymbolItem>();
    qp.placeholder = "Search symbols by name or natural language…";
    qp.matchOnDescription = true;
    let debounce: ReturnType<typeof setTimeout> | undefined;
    let queryAbort: AbortController | undefined;

    const run = (value: string): void => {
      if (!value.trim()) {
        qp.items = [];
        return;
      }
      queryAbort?.abort();
      queryAbort = new AbortController();
      const signal = queryAbort.signal;
      const normalised = stripLangTokens(value);
      qp.busy = true;
      void client
        .callTool(
          "get_graph_json",
          { query: normalised, direction: "both", depth: "1", kind_filter: "" },
          signal
        )
        .then((raw) => {
          if (signal.aborted || qp.value !== value) return;
          qp.items = parseGraphSymbols(raw);
          qp.busy = false;
        });
    };

    qp.onDidChangeValue((value) => {
      clearTimeout(debounce);
      debounce = setTimeout(() => run(value), 250);
    });

    qp.onDidAccept(() => {
      const sel = qp.selectedItems[0];
      if (!sel) { qp.hide(); return; }
      void openAtLine(sel.path, sel.line);
      qp.hide();

      // ITEM 5: synonym learning — offer to teach the backend when query ≠ selected name.
      const cfg = vscode.workspace.getConfiguration("travsr");
      if (!cfg.get<boolean>("suggestSynonyms", true)) return;
      const typedToken = stripLangTokens(qp.value).split(/\s+/)[0] ?? "";
      // Strip codicon prefix from label (e.g. "$(symbol-method) barFn" → "barFn")
      const selectedName = sel.label.replace(/^\$\([^)]+\)\s*/, "").trim();
      if (!typedToken || typedToken === selectedName) return;
      const pairKey = `${typedToken}\x00${selectedName}`;
      if (synonymPromptedPairs.has(pairKey)) return;
      synonymPromptedPairs.add(pairKey);
      void vscode.window
        .showInformationMessage(
          `Add synonym: "${typedToken}" → "${selectedName}"?`,
          "Add",
          "Skip"
        )
        .then((choice) => {
          if (choice === "Add") {
            void client.callTool("synonym_add", { term: typedToken, alias: selectedName });
          }
        });
    });

    qp.onDidHide(() => {
      clearTimeout(debounce);
      queryAbort?.abort();
      qp.dispose();
    });
    qp.show();
  });
}

// ── Managed webview panels (singleton per viewType) ─────────────────────────

/** Messages posted from the management webviews back to the extension. */
type PanelMessage =
  | { command: "add"; term: string; alias: string }
  | { command: "addBatch"; term: string; aliases: string[] }
  | { command: "removePair"; term: string; alias: string }
  | { command: "removeTerm"; term: string }
  | { command: "reset" }
  | { command: "prune" }
  | { command: "remove"; name: string }
  | { command: "installLang"; language: string }
  | { command: "removeLang"; language: string }
  | { command: "enableWithPermission"; language: string }
  | { command: "detectLangs" }
  | { command: "reloadAvailable" }
  | { command: "downloadBinary" }
  | { command: "openBinarySetting" }
  | { command: "pickRepo" }
  | { command: "initRepo" }
  | { command: "openFile"; path: string }

  | { command: "setLogLines"; lines: number }
  | { command: "setLogFile"; file: string }
  | { command: "setLogAuto"; seconds: number }
  | { command: "startDaemon" }
  | { command: "restartDaemon" }
  | { command: "reindex" }
  | { command: "fullRebuild" }
  | { command: "reindexSemantic" }
  | { command: "installHook" }
  | { command: "installEmbed" }
  | { command: "restartEmbed" }
  | { command: "runFsck" }
  | { command: "compact" }
  | { command: "registerMcp" }
  | { command: "fixLang"; index: number }
  | { command: "disableLang"; index: number }
  | { command: "runFix"; index: number }
  | { command: "copyFix"; index: number }
  | { command: "refresh" };

const managedPanels = new Map<string, { panel: vscode.WebviewPanel; refresh: () => Promise<void> }>();

/** Re-render every open managed panel, call after an external `travsr init` updates graph.db. */
export function refreshOpenPanels(): void {
  for (const { refresh } of managedPanels.values()) {
    void refresh();
  }
}

/**
 * Open (or reveal) a singleton management webview. `render` produces the HTML;
 * `handle` reacts to a posted message and may call the provided `refresh`.
 */
type RefreshFn = (override?: string) => Promise<void>;
/** Posts a status bar update to the active webview (`type` defaults to `'loading'`). */
type PostStatus = (text: string, type?: 'loading' | 'error' | 'ok') => void;

function openManagedPanel(
  viewType: string,
  title: string,
  render: () => Promise<string>,
  handle: (msg: PanelMessage, refresh: RefreshFn, postStatus: PostStatus) => Promise<void>
): void {
  const existing = managedPanels.get(viewType);
  if (existing) {
    existing.panel.reveal(vscode.ViewColumn.Active);
    existing.panel.webview.html = buildPanelLoadingHtml(title);
    void existing.refresh();
    return;
  }
  const panel = vscode.window.createWebviewPanel(viewType, title, vscode.ViewColumn.Active, {
    enableScripts: true,
    localResourceRoots: [],
  });
  const refresh: RefreshFn = async (override?: string): Promise<void> => {
    try {
      panel.webview.html = override ?? await render();
    } catch {
      // Render failed — show an error state so the status bar is never orphaned.
      panel.webview.html = buildPanelLoadingHtml(`${title} (error; try reopening)`);
    }
  };
  // Sends a status update into the live webview HTML (cleared on next full re-render).
  const postStatus: PostStatus = (text, type = 'loading') =>
    void panel.webview.postMessage({ command: 'setStatus', text, type });

  managedPanels.set(viewType, { panel, refresh: () => refresh() });
  panel.webview.onDidReceiveMessage((msg: PanelMessage) => {
    // Unlock all button spinners IMMEDIATELY — before any async work.
    void panel.webview.postMessage({ command: 'unlockButtons' });
    void handle(msg, refresh, postStatus);
  });
  panel.onDidDispose(() => managedPanels.delete(viewType));
  panel.webview.html = buildPanelLoadingHtml(title);
  void refresh();
}

/**
 * travsr.manageSynonyms — interactive synonym editor webview backed by the
 * synonym_* MCP tools. Supports multi-chip staged batch add.
 */
export function registerManageSynonyms(client: McpClient): vscode.Disposable {
  const render = async (): Promise<string> =>
    buildSynonymsHtml(parseSynonymList(await client.callTool("synonym_list")));

  const warnIfError = (result: string): void => {
    const trimmed = result.trim();
    if (trimmed && trimmed !== "ok") {
      void vscode.window.showWarningMessage(`Travsr: ${trimmed}`);
    }
  };

  const handle = async (msg: PanelMessage, refresh: RefreshFn, _postStatus: PostStatus): Promise<void> => {
    switch (msg.command) {
      case "add":
        warnIfError(await client.callTool("synonym_add", { term: msg.term, alias: msg.alias }));
        break;
      case "addBatch":
        // synonym_set is atomic: replaces all aliases for the term in one write.
        warnIfError(await client.callTool("synonym_set", { term: msg.term, aliases: msg.aliases.join(",") }));
        break;
      case "removePair":
        await client.callTool("synonym_remove", { term: msg.term, alias: msg.alias });
        break;
      case "removeTerm":
        await client.callTool("synonym_remove_term", { term: msg.term });
        break;
      case "reset": {
        const confirm = await vscode.window.showWarningMessage(
          "Reset all synonyms to the built-in defaults? Custom entries will be lost.",
          { modal: true },
          "Reset"
        );
        if (confirm !== "Reset") return;
        warnIfError(await client.callTool("synonym_reset"));
        break;
      }
      default:
        break;
    }
    await refresh();
  };

  return vscode.commands.registerCommand("travsr.manageSynonyms", () =>
    openManagedPanel("travsrSynonyms", "Travsr: Synonyms", render, handle)
  );
}

/**
 * travsr.showRepos — registry manager webview (status badges, prune, remove)
 * backed by the repos_* MCP tools.
 */
export function registerShowRepos(client: McpClient): vscode.Disposable {
  const render = async (): Promise<string> =>
    buildReposHtml(parseReposList(await client.callTool("repos_list")));

  const handle = async (msg: PanelMessage, refresh: RefreshFn, _postStatus: PostStatus): Promise<void> => {
    if (msg.command === "prune") {
      const result = stripEnvelope(await client.callTool("repos_prune"));
      const m = /^pruned:\s*(\d+)/.exec(result.trim());
      void vscode.window.showInformationMessage(
        `Pruned ${m ? m[1] : "0"} stale repo(s).`
      );
    } else if (msg.command === "remove") {
      await client.callTool("repos_remove", { name: (msg as { command: "remove"; name: string }).name });
    }
    await refresh();
  };

  return vscode.commands.registerCommand("travsr.showRepos", () =>
    openManagedPanel("travsrRepos", "Travsr: Repos", render, handle)
  );
}

/**
 * travsr.showGraphStats — the Health panel: verdict, metrics, daemon and index
 * state, languages (installed on this machine versus enabled for this repo,
 * with install, enable and disable actions), diagnostics and the daemon log.
 */
export function registerShowGraphStats(
  client: McpClient,
  binary: string,
  activeRepo: ActiveRepo
): vscode.Disposable {
  // Read the configured binary path at call time so we always use the value
  // written by checkBinaryAndPrompt, which runs async after activation.
  const getBinary = (): string =>
    vscode.workspace.getConfiguration("travsr").get<string>("binaryPath") || binary;
  // Kept so a log-only refresh can redraw without re-running the two expensive
  // halves of a render, which is what changing Lines or File does. Undefined
  // until the first full pass, so a log-only refresh before then falls back to
  // doing the work.
  let lastStats: StatsView | undefined;
  let lastDiags: Diagnostic[] = [];
  let lastIndex: IndexHealth = UNKNOWN_INDEX;
  let lastHealth: HealthData = EMPTY_HEALTH;
  let logOnly = false;
  // How many lines the reader is asked for. The panel's Lines control raises
  // this when the user picks a window wider than what is already loaded, which
  // is the only way to show more: the dropdown's other job is a local hide over
  // rows that are already in the DOM.
  let logLines = 500;
  // Which rotated file is showing. `undefined` means "whatever the newest is",
  // which is the default and the only state that survives a rotation: an
  // explicit pick is a pick of that day and stays put when the day rolls, while
  // the default follows the daemon onto the new file.
  let logFile: string | undefined;
  // Auto-refresh interval in seconds, 0 for off.
  //
  // The timer lives here rather than in the webview, and that is the whole
  // difference from the Follow toggle this replaces. `refresh()` assigns
  // `panel.webview.html` wholesale, so a `setInterval` inside the document dies
  // with the first tick it triggers: Follow fired once, cleared its own
  // checkbox, and never polled again.
  let logAuto = 0;
  let autoTimer: ReturnType<typeof setInterval> | undefined;

  const stopAuto = (): void => {
    if (autoTimer !== undefined) {
      clearInterval(autoTimer);
      autoTimer = undefined;
    }
  };

  /** One auto-refresh tick: new log rows into the live document, nothing else.
   *
   *  Deliberately not `refresh()`. A full render on a timer would discard the
   *  search box, the severity chip, the toggles, the scroll position and every
   *  expanded row on every tick, which is #767, and inflicting it every few
   *  seconds is worse than not polling at all. Replacing the rows leaves all of
   *  that standing.
   *
   *  What that trades away: the metric cards and the health banner do not move
   *  on a tick, so a daemon that dies while you watch is reported by its own log
   *  lines arriving (or stopping) rather than by the banner. The Refresh button
   *  moves everything. Stated in the control's tooltip so it is not a surprise.
   */
  const autoTick = (): void => {
    const entry = managedPanels.get("travsrStats");
    if (entry === undefined) {
      // The panel was closed. Nothing else clears this, and posting into a
      // disposed webview is pointless, so the timer retires itself. Costs at
      // most one dead tick after a close.
      stopAuto();
      return;
    }
    // A full redraw, which is what the control now offers. It costs the panel
    // state a redraw always costs (#767): the log filter, the severity chip,
    // the toggles, the scroll position and any expanded row. That is stated in
    // the control's own tooltip rather than left to be discovered, and the
    // default is still Off.
    void entry.refresh();
  };

  const render = async (): Promise<string> => {
    const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const reuse = logOnly && lastStats !== undefined;
    const stats = reuse ? (lastStats as StatsView) : buildStatsView(await client.callTool("get_graph_stats"));
    // Read straight from the log file rather than asking the daemon: it works
    // after a crash, which is when the panel is worth opening. This is the
    // cheap half, and the only half a log-only refresh needs.
    // Re-listed every render rather than cached: rotation and the daemon's
    // prune both change the directory under an open panel.
    const logFiles = root ? daemonLogFileList(root) : { files: [], onDisk: 0 };
    // A pinned file that is gone (rotated past the cap, or pruned while the
    // panel sat open) falls back to the newest, rather than rendering an empty
    // log against a filename nothing can satisfy.
    const selected =
      logFile !== undefined && logFiles.files.some((f) => f.name === logFile)
        ? logFile
        : (logFiles.files[0]?.name ?? "");
    const log = root && selected !== "" ? readDaemonLogFile(root, selected, logLines) : [];
    const bin = vscode.workspace.getConfiguration("travsr").get<string>("binaryPath") || "travsr";
    // readDiagnostics spawns `travsr status`.
    const diags = reuse ? lastDiags : root ? await readDiagnostics(bin, root) : [];
    // Drift, straight from the daemon. An older binary does not serve this tool
    // and the client answers "" for an unknown tool, which parseIndexHealth
    // reports as unavailable rather than as fresh.
    const index = reuse
      ? lastIndex
      : parseIndexHealth(await client.callTool("get_index_status"));
    // The sections. Skipped on a log-only refresh, which is what changing Lines
    // or File does: those spawns are the expensive half and nothing above the
    // log has changed.
    const health = reuse
      ? lastHealth
      : await gatherHealth(client, bin, root, log, stats.dbSize, logFiles, path.basename(root ?? ""), diags);
    lastStats = stats;
    lastDiags = diags;
    lastIndex = index;
    lastHealth = health;
    // A panel that was closed and reopened renders the stored interval as
    // selected, so the timer has to come back with it: otherwise the control
    // claims to be polling while nothing is.
    if (logAuto > 0 && autoTimer === undefined) {
      autoTimer = setInterval(autoTick, logAuto * 1000);
    }
    return buildStatsHtml(
      stats, log, diags, logLines, { ...logFiles, selected }, logAuto, index, health,
      path.basename(root ?? ""), root ?? ""
    );
  };

  const handle = async (msg: PanelMessage, refresh: RefreshFn, _postStatus: PostStatus): Promise<void> => {
    const repoRoot = (): string | undefined => vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;

    // Commands that run in the shared terminal. Each argv is a literal here,
    // never assembled from anything the webview sent, so a panel that rendered
    // hostile text still cannot choose what runs.
    const TERMINAL_ACTIONS: Partial<Record<PanelMessage["command"], string[]>> = {
      startDaemon: ["daemon", "start"],
      restartDaemon: ["daemon", "restart"],
      fullRebuild: ["init", "--force"],
      reindexSemantic: ["init", "--semantic", "--force"],
      // Plain `init`, not `--force`. It is incremental, skips when the graph is
      // already up to date, and installs the hook as part of setup, which is
      // all this row is asking for. `--force` is the same argv `fullRebuild`
      // uses behind a modal, so sharing it here meant "Install" beside a
      // missing commit hook silently discarded and rebuilt the whole index.
      installHook: ["init"],
      installEmbed: ["embed", "init"],
      restartEmbed: ["embed", "init", "--reinstall"],
      runFsck: ["fsck"],
      compact: ["fsck", "--fix"],
      // Without `--yes` this prompts per language. That is deliberate: it runs
      // in a real terminal where the prompt works, and `--yes` would have the
      // panel download several analyzers off one click.
      detectLangs: ["lang", "detect"],
    };
    const argv = TERMINAL_ACTIONS[msg.command];
    if (argv !== undefined) {
      // The destructive ones say what they will do before doing it. Reindexing
      // is not destructive; rebuilding and repairing both discard state.
      const DESTRUCTIVE: Partial<Record<string, string>> = {
        fullRebuild: "Rebuild the graph from scratch? The current index is discarded and rebuilt.",
        compact: "Repair the index? Entries pointing at files that no longer exist are removed.",
      };
      const warning = DESTRUCTIVE[msg.command];
      if (warning !== undefined) {
        const go = await vscode.window.showWarningMessage(warning, { modal: true }, "Run");
        if (go !== "Run") return;
      }
      runTravsrCommand(argv, repoRoot());
      return;
    }
    if (msg.command === "reindex") {
      await vscode.commands.executeCommand("travsr.reindexNow");
      await refresh();
      return;
    }
    if (msg.command === "initRepo") {
      // The unindexed verdict's primary action. Without this branch it fell
      // through to the trailing refresh and re-rendered the same empty page,
      // so the one button offered on a never-indexed repo did nothing.
      //
      // Runs under cancellable progress rather than in the terminal, matching
      // the Languages panel: init on a large repo is long, and the panel has to
      // redraw when it finishes so the page stops saying there is no graph.
      const repo = repoRoot();
      if (repo === undefined) {
        void vscode.window.showWarningMessage("Travsr: open a folder to index it.");
        return;
      }
      const binary =
        vscode.workspace.getConfiguration("travsr").get<string>("binaryPath") || "travsr";
      const { cancelled, code } = await spawnManagedInstall(
        binary,
        ["init"],
        repo,
        "Travsr: indexing this repository…"
      );
      if (cancelled) return;
      if (code !== 0) {
        void vscode.window.showErrorMessage(
          `Travsr: indexing failed (exit ${code ?? "unknown"}). See the daemon log.`
        );
      }
      await refresh();
      return;
    }
    if (msg.command === "registerMcp") {
      await vscode.commands.executeCommand("travsr.registerMcpServer");
      await refresh();
      return;
    }
    // Registry edits. These existed only in the Repos panel's own handler, so
    // the same buttons on this page posted a message nothing listened for and
    // silently did nothing.
    if (msg.command === "prune") {
      const stale = (lastHealth.repos ?? []).filter((r) => !r.exists).length;
      const go = await vscode.window.showWarningMessage(
        stale > 0
          ? `Remove ${stale} registry entr${stale === 1 ? "y" : "ies"} whose database is missing? The repositories themselves are not touched.`
          : "Prune stale registry entries?",
        { modal: true },
        "Prune"
      );
      if (go !== "Prune") return;
      const result = stripEnvelope(await client.callTool("repos_prune"));
      const m = /^pruned:\s*(\d+)/.exec(result.trim());
      void vscode.window.showInformationMessage(`Travsr: pruned ${m ? m[1] : "0"} stale repo(s).`);
      await refresh();
      return;
    }
    if (msg.command === "remove") {
      await client.callTool("repos_remove", { name: msg.name });
      await refresh();
      return;
    }
    if (msg.command === "fixLang" || msg.command === "disableLang") {
      // An index into the languages this render produced, so the webview never
      // chooses the language name that reaches the command line.
      const lang = lastHealth.languages?.[msg.index];
      if (lang === undefined) return;
      const bin = getBinary();
      // The table was computed for the first workspace folder (that is the cwd
      // `lang list` ran in), so actions target the same repo; the active-repo
      // picker is the fallback when there is no folder.
      const repo = repoRoot() ?? (await activeRepo.ensureChosen());
      if (!repo) return;
      const name = lang.language;
      if (msg.command === "disableLang") {
        if (!lang.canDisable) return;
        const r = await spawnLangCommandResult(bin, ["lang", "remove", name], repo);
        await refresh();
        if (r.code === 0) {
          void vscode.window.showInformationMessage(`Travsr: full analysis for ${name} is off.`);
        } else {
          void vscode.window.showErrorMessage(
            `Travsr: could not disable ${name}. ${lastLine(r.out) || `Run \`travsr lang remove ${name}\` in a terminal for details.`}`
          );
        }
        return;
      }
      switch (lang.fix) {
        case "install": {
          // Downloads the analyzer and, because cwd is the repo, turns full
          // analysis on for it. Cancellable, no wall-clock kill: a slow download
          // must never be cut off and reported as a false success.
          const { out, cancelled, code } = await spawnManagedInstall(
            bin,
            ["lang", "install", name, "--no-interactive", "--yes"],
            repo,
            `Installing ${name}…`
          );
          await refresh();
          if (cancelled) {
            void vscode.window.showWarningMessage(
              `Install of ${name} was cancelled; it may be partly done. Re-run, or run \`travsr lang install ${name}\` in a terminal.`
            );
          } else if (code === 2) {
            // Set up, but the build tool the project needs is not installed, so
            // full analysis cannot run yet. The CLI already phrases this.
            const needTxt = lang.prerequisites && lang.prerequisites !== "unknown" ? ` (${lang.prerequisites})` : "";
            void vscode.window.showWarningMessage(
              lastLine(out) ||
                `${name} is set up, but the build tool it needs${needTxt} was not found. Install it, then Refresh to get full analysis.`
            );
          } else if (code !== 0) {
            void vscode.window.showErrorMessage(
              `Install of ${name} failed. ${lastLine(out) || `Run \`travsr lang install ${name}\` in a terminal for details.`}`
            );
          } else {
            void vscode.window.showInformationMessage(lastLine(out) || `${name} analyzer installed and enabled for this repository.`);
          }
          return;
        }
        case "enable": {
          // Installed on this machine, off for this repo. `--skip-wrapper` turns
          // the language on in config without downloading anything; full analysis
          // then runs on the next semantic index, which is offered right away.
          const { out, cancelled, code } = await spawnManagedInstall(
            bin,
            ["lang", "install", name, "--skip-wrapper", "--no-interactive", "--yes"],
            repo,
            `Enabling ${name} for this repository…`
          );
          await refresh();
          if (cancelled) {
            void vscode.window.showWarningMessage(`Enabling ${name} was cancelled.`);
          } else if (code !== 0 && code !== 2) {
            void vscode.window.showErrorMessage(
              `Could not enable ${name}. ${lastLine(out) || `Run \`travsr lang install ${name}\` in this repository for details.`}`
            );
          } else {
            const pick = await vscode.window.showInformationMessage(
              `${name} is enabled for this repository. Full analysis runs on the next semantic index.`,
              "Re-index now"
            );
            if (pick === "Re-index now") runTravsrCommand(["init", "--semantic", "--force"], repo);
          }
          return;
        }
        case "permission": {
          // A security-relevant grant: full analysis for this language will run
          // with the user's own privileges (its build tools cannot run isolated
          // on this OS). Confirm in plain language first, record it, then
          // re-index so it takes effect.
          const ok = await vscode.window.showWarningMessage(
            `Allow full analysis for ${name} to run on this machine?`,
            {
              modal: true,
              detail:
                "It will use your project's own build tools, the same as if you ran the build yourself, including downloading this project's dependencies. You can withdraw this permission later.",
            },
            "Allow"
          );
          if (ok !== "Allow") return;
          // The modal above is the explicit grant, so pass `--yes`: the CLI
          // refuses a non-interactive grant without it. Check the exit code: a
          // consent step must not report success unless the permission was
          // actually recorded.
          const grant = await spawnLangCommandResult(bin, ["lang", "allow-unsandboxed", name, "--yes"]);
          if (grant.code !== 0) {
            await refresh();
            void vscode.window.showErrorMessage(
              `Could not enable ${name}: ${lastLine(grant.out) || "the permission was not recorded."}`
            );
            return;
          }
          const { cancelled } = await spawnManagedInstall(bin, ["init", "--semantic", "--force"], repo, `Enabling ${name}…`);
          await refresh();
          void (cancelled
            ? vscode.window.showWarningMessage(`Enabling ${name} was cancelled before analysis finished. Refresh to check its state.`)
            : vscode.window.showInformationMessage(`${name} is enabled. Full analysis will run on the next index.`));
          return;
        }
        case "semantic":
          runTravsrCommand(["init", "--semantic", "--force"], repo);
          return;
        default:
          return;
      }
    }
    // #755: the skew banner's two remedies, the same actions the palette exposes.
    if (msg.command === "downloadBinary") {
      await vscode.commands.executeCommand("travsr.downloadBinary");
      await refresh();
      return;
    }
    if (msg.command === "openBinarySetting") {
      await vscode.commands.executeCommand("workbench.action.openSettings", "travsr.binaryPath");
      return;
    }
    if (msg.command === "runFix" || msg.command === "copyFix") {
      // The webview sends an index, not a command. The text is looked up here,
      // in the diagnostics this render produced, so a log line cannot inject a
      // command by being rendered into the page.
      const cmd = lastDiags[msg.index]?.command;
      if (cmd === undefined) return;
      if (msg.command === "copyFix") {
        await vscode.env.clipboard.writeText(cmd);
        void vscode.window.showInformationMessage(`Travsr: copied \`${cmd}\``);
        return;
      }
      const args = parseTravsrInvocation(cmd);
      if (args === null) {
        // Not a plain `travsr ...` call. Copy it rather than handing anything
        // with shell syntax in it to a shell.
        await vscode.env.clipboard.writeText(cmd);
        void vscode.window.showWarningMessage(
          "Travsr: that fix is not a plain travsr command, so it was copied instead of run."
        );
        return;
      }
      runTravsrCommand(args, repoRoot());
      return;
    }
    if (msg.command === "openFile") {
      // The log writes absolute paths in some places and repo-relative in
      // others, so both resolve against the repo root. The result must stay
      // inside it: the panel renders whatever the log file says, and a log file
      // is not a trusted input just because it is local. Without this a `path=`
      // field naming anything on disk becomes a click that opens it.
      const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      if (root === undefined) return;
      const target = path.resolve(root, msg.path);
      const rel = path.relative(root, target);
      if (rel.startsWith("..") || path.isAbsolute(rel)) {
        void vscode.window.showWarningMessage(
          `Travsr: ${msg.path} is outside the workspace, not opening it`
        );
        return;
      }
      try {
        const doc = await vscode.workspace.openTextDocument(target);
        await vscode.window.showTextDocument(doc, { preview: true });
      } catch {
        // The file the log complained about may be the file that is gone.
        void vscode.window.showWarningMessage(`Travsr: cannot open ${msg.path}`);
      }
      return;
    }
    if (msg.command === "setLogLines") {
      // Widening the window: re-read with the bigger cap. Log-only, because the
      // graph stats and the `travsr status` spawn have not changed and are the
      // expensive half of a render.
      logLines = Math.min(Math.max(Math.trunc(msg.lines) || 0, 1), LOG_MAX_LINES);
      logOnly = true;
      try {
        await refresh();
      } finally {
        logOnly = false;
      }
      return;
    }
    if (msg.command === "setLogFile") {
      // Always a re-read: the rows for another file were never sent to the
      // webview, so there is no local path to take. Log-only, for the same
      // reason widening Lines is: the graph stats and the `travsr status` spawn
      // have not changed and are the expensive half.
      //
      // Stored unchecked on purpose. `render` drops a name the directory does
      // not list and falls back to the newest, and `readDaemonLogFile` checks
      // again before it opens anything, so a crafted message cannot turn into a
      // path.
      logFile = msg.file;
      logOnly = true;
      try {
        await refresh();
      } finally {
        logOnly = false;
      }
      return;
    }
    if (msg.command === "setLogAuto") {
      // Validated against the options the control actually offers rather than
      // trusted: this number comes from the webview and goes into setInterval,
      // where a 0.001 would busy-loop the extension host.
      logAuto = LOG_AUTO_SECONDS.includes(msg.seconds) ? msg.seconds : 0;
      stopAuto();
      if (logAuto > 0) autoTimer = setInterval(autoTick, logAuto * 1000);
      // No refresh: the select in the live document already shows the choice,
      // and redrawing to confirm it would throw away the panel state the tick
      // path exists to protect. `autoSeconds` renders it on the next full pass.
      return;
    }
    await refresh();
  };

  return vscode.commands.registerCommand("travsr.showGraphStats", () =>
    openManagedPanel("travsrStats", "Travsr: Health", render, handle)
  );
}

/**
 * travsr.showDependencies — direct + transitive imports of a file, with
 * click-to-navigate for resolvable local imports. External/stdlib/crate deps
 * are shown dimmed and non-clickable.
 */
export function registerShowDependencies(client: McpClient): vscode.Disposable {
  return vscode.commands.registerCommand("travsr.showDependencies", async (file?: string) => {
    const activeFile = vscode.window.activeTextEditor?.document.fileName;
    const target =
      file ??
      (activeFile
        ? vscode.workspace.asRelativePath(activeFile)
        : undefined);
    if (!target) {
      void vscode.window.showInformationMessage("Open a file to see its dependencies.");
      return;
    }

    const raw = stripEnvelope(
      await client.callTool("get_dependencies", { file: target, transitive: "true", depth: "3" })
    );
    const lines = raw.split("\n").map((l) => l.replace(/\s+$/, "")).filter((l) => l.trim());
    const directLines = lines.filter((l) => !l.startsWith(" ") && !l.includes("↳"));
    const transitiveLines = lines.filter((l) => l.startsWith(" ") || l.includes("↳"));

    // Resolve specifiers to file paths when possible.
    const sourceAbsPath = activeFile ?? (
      vscode.workspace.workspaceFolders?.[0]?.uri.fsPath
        ? path.join(vscode.workspace.workspaceFolders[0].uri.fsPath, target)
        : target
    );

    const toEntry = (spec: string): import("./commands").DepEntry => {
      const clean = spec.replace(/^\s*↳\s*/, "").trim();
      const resolved = resolveDepSpec(clean, sourceAbsPath);
      const colon = clean.indexOf(":");
      const display = colon >= 0 ? clean.slice(colon + 1) : clean;
      return { display, path: resolved };
    };

    const direct = directLines.map(toEntry);
    const transitive = transitiveLines.map(toEntry);

    const panel = vscode.window.createWebviewPanel(
      "travsrDependencies",
      `Dependencies, ${target}`,
      vscode.ViewColumn.Beside,
      { enableScripts: true, localResourceRoots: [] }
    );
    panel.webview.html = buildDepListHtml(
      `Dependencies of <code>${escHtml(target)}</code>`,
      direct,
      transitive
    );
    panel.webview.onDidReceiveMessage((msg: { command?: string; path?: string }) => {
      if (msg.command === "open" && msg.path) void openAtLine(msg.path);
    });
  });
}

/**
 * travsr.showExecutionPath — prompt for source + sink (source seeded from the
 * word under the cursor), then render the PCST path in the graph panel.
 */
export function registerShowExecutionPath(
  client: McpClient,
  context: vscode.ExtensionContext
): vscode.Disposable {
  return vscode.commands.registerCommand("travsr.showExecutionPath", async () => {
    const editor = vscode.window.activeTextEditor;
    const seed = editor
      ? editor.document.getText(editor.document.getWordRangeAtPosition(editor.selection.active))
      : "";

    const source = await vscode.window.showInputBox({
      prompt: "Source symbol",
      value: seed,
    });
    if (!source) return;
    const sink = await vscode.window.showInputBox({ prompt: "Sink symbol" });
    if (!sink) return;

    const raw = await client.callTool("get_execution_path", { source, sink });
    const data = parseExecutionPath(raw);
    if (data.nodes.length === 0) {
      void vscode.window.showInformationMessage(`No path found from ${source} to ${sink}.`);
      return;
    }
    const panel = GraphPanel.show(client, context);
    panel.renderPath(data, `${source} → ${sink}`);
  });
}

/**
 * What is wrong with this repo's index, as `travsr status` reports it.
 *
 * The CLI already phrases every one of these for a person and names the command
 * that fixes it, so this parses that rather than reimplementing the mapping in
 * TypeScript and letting the two drift.
 *
 * These are all repo-scoped: an analyzer that crashed, a language with no tool
 * registered, an approval that was never given. None of them belong to a file,
 * which is why they are cards here rather than entries in the Problems panel,
 * which wants a file to attach to.
 */
export async function readDiagnostics(binary: string, cwd: string): Promise<Diagnostic[]> {
  const out = await spawnLangCommand(binary, ["status"], cwd);
  const found: Diagnostic[] = [];
  for (const line of out.split("\n")) {
    const m = /^\s*warning:\s*(.+)$/.exec(line);
    if (!m) continue;
    const text = m[1].trim();
    // The CLI writes the fix in backticks. Lift it out so it can be copied
    // without the reader having to pick it out of the sentence.
    const cmd = /`([^`]+)`/.exec(text);
    const severity: Diagnostic["severity"] = /crashed|failed|not usable/i.test(text)
      ? "error"
      : "warn";
    found.push({
      severity,
      title: text.replace(/\s*[-—,;.]?\s*(re-?run|run)\s+`[^`]+`.*$/i, "").trim(),
      hint: text,
      command: cmd ? cmd[1] : undefined,
    });
  }
  return found;
}

/** Spawn a travsr CLI command and return its combined stdout+stderr.
 *
 *  For fast, local, read-only commands (`lang list`, `lang remove`): a short
 *  wall-clock timeout is fine here because these do no network I/O, so a hang
 *  means something is wrong and killing it is the right move. Network installs
 *  must NOT use this — see `spawnManagedInstall`. */
/** Run a short lang command and resolve its combined output plus exit code.
 *  `code` is `null` on timeout or spawn error, so a caller that must confirm a
 *  command actually succeeded (a security-consent grant) can check `code === 0`
 *  rather than trust empty output. */
/** `out` is stdout+stderr interleaved, which is what the human-facing callers
 *  want (a CLI remedy line is as likely on stderr as on stdout). `stdout` is
 *  stdout alone, for the one caller that must `JSON.parse` the result: #755
 *  review — a single stderr line printed alongside the payload (a `tracing` line
 *  when the user has `RUST_LOG` set) made `JSON.parse` fail, which the parser maps
 *  to no rows and no skew. That is a false "current", the one wrong answer the
 *  contract check must never give, and it needed no timeout to reach. */
function spawnLangCommandResult(
  binary: string,
  args: string[],
  cwd?: string,
  timeoutMs = 4_000
): Promise<{ out: string; stdout: string; code: number | null }> {
  return new Promise((resolve) => {
    let out = "";
    let stdout = "";
    let resolved = false;
    const done = (v: { out: string; stdout: string; code: number | null }): void => { if (!resolved) { resolved = true; resolve(v); } };
    const proc = cp.spawn(binary, args, { env: { ...process.env, TERM: "dumb", NO_COLOR: "1" }, ...(cwd ? { cwd } : {}) });
    proc.stdout?.on("data", (d: Buffer) => { const s = d.toString(); out += s; stdout += s; });
    proc.stderr?.on("data", (d: Buffer) => { out += d.toString(); });
    const timer = setTimeout(() => { try { proc.kill(); } catch { /* ignore */ } done({ out, stdout, code: null }); }, timeoutMs);
    proc.on("close", (code) => { clearTimeout(timer); done({ out, stdout, code }); });
    proc.on("error", (e) => { clearTimeout(timer); done({ out: `error: ${e.message}`, stdout: "", code: null }); });
  });
}

function spawnLangCommand(binary: string, args: string[], cwd?: string, timeoutMs = 4_000): Promise<string> {
  return spawnLangCommandResult(binary, args, cwd, timeoutMs).then((r) => r.out);
}

/** The last non-empty line of CLI output, the final status the command printed
 *  (e.g. "'rust' is active — full cross-file analysis is on."). Empty when the
 *  command printed nothing. */
function lastLine(s: string): string {
  const lines = s.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  return lines.length ? lines[lines.length - 1] : "";
}

/** Run a network-bound install command (`lang install`, `lang detect --yes`,
 *  `init`) under a cancellable progress notification.
 *
 *  Deliberately imposes NO wall-clock timeout. A fixed timer is the wrong tool
 *  here: on a slow connection it SIGKILLs the CLI mid-download and — because the
 *  killed process resolves with empty output — the panel would report a false
 *  success while leaving a half-finished install behind. Instead the worst case
 *  is bounded by the CLI's own per-download network timeouts, and the user can
 *  stop it at any time from the notification's Cancel button. The result says
 *  whether the user cancelled so the caller can report an honest outcome. */
function spawnManagedInstall(
  binary: string,
  args: string[],
  cwd: string,
  title: string
): Thenable<{ out: string; cancelled: boolean; code: number | null }> {
  return vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title, cancellable: true },
    (_progress, token) =>
      new Promise((resolve) => {
        let out = "";
        let settled = false;
        const finish = (r: { out: string; cancelled: boolean; code: number | null }): void => {
          if (!settled) { settled = true; resolve(r); }
        };
        const proc = cp.spawn(binary, args, {
          env: { ...process.env, TERM: "dumb", NO_COLOR: "1" },
          cwd,
        });
        proc.stdout?.on("data", (d: Buffer) => { out += d.toString(); });
        proc.stderr?.on("data", (d: Buffer) => { out += d.toString(); });
        token.onCancellationRequested(() => {
          try { proc.kill(); } catch { /* ignore */ }
          finish({ out, cancelled: true, code: null });
        });
        // Exit code carries meaning: `lang install` exits 2 when the language was
        // set up but the project build tool it needs (e.g. Gradle, sbt, composer)
        // is not installed — a partial success the caller reports honestly.
        proc.on("close", (code) => finish({ out, cancelled: false, code }));
        proc.on("error", (e) =>
          finish({ out: `${out}\nerror: ${e.message}`, cancelled: false, code: null })
        );
      })
  );
}

/** Register all VSCODE-247 commands. */
export function registerParityCommands(
  client: McpClient,
  context: vscode.ExtensionContext,
  binary: string,
  // Kept for the caller's sake: the Languages panel used it after an in-panel
  // `init`; the Health panel runs `init` in a terminal, so nothing to hook.
  _onAfterInit?: () => void
): void {
  const activeRepo = new ActiveRepo(context);
  context.subscriptions.push(
    vscode.commands.registerCommand("travsr.selectRepository", () => activeRepo.pick()),
    registerAskSymbol(client),
    registerManageSynonyms(client),
    registerShowDependencies(client),
    registerShowExecutionPath(client, context),
    registerShowRepos(client),
    registerShowGraphStats(client, binary, activeRepo),
    // The Languages panel was folded into Health. The command id stays as an
    // alias so an existing keybinding or programmatic caller lands on the page
    // that now holds that table; it is no longer contributed to the palette.
    vscode.commands.registerCommand("travsr.showLanguages", () =>
      vscode.commands.executeCommand("travsr.showGraphStats")
    )
  );
}
