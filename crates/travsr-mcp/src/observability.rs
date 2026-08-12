//! #636: read-only MCP observability tools.
//!
//! `get_index_status`, `get_daemon_logs`, `get_graph_health`, three tools
//! that answer "is the index fresh / healthy / what did the daemon just do"
//! without going through the CLI. All three are strictly read-only (never
//! open a store read-write, never write `.travsr/daemon.lock`) and are
//! single-repo shaped: unlike most global-mode tools, they refuse to
//! aggregate across repos (`resolve_single_repo`) because a cross-repo merge
//! of index staleness or daemon logs is either meaningless (staleness is
//! per-repo) or a leak (`get_daemon_logs` reading another repo's logs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use travsr_store::SqliteStore;

use crate::rerank;
use crate::sanitize::{sanitize_log_value, validate_mcp_arg, wrap_envelope};
use crate::tools::git_short_head;

/// A resolved single-repo target for the observability tools' global-mode
/// variants: registry key, repo root (derived from the db path), and db path.
#[derive(Debug)]
struct RepoTarget {
    name: String,
    root: PathBuf,
    db_path: PathBuf,
}

/// Returned by [`resolve_single_repo`] for both "no such repo" and "repo
/// registered but its db is missing/stale", kept identical so the tool
/// cannot be used to probe which names exist in the registry.
const UNKNOWN_REPO_ERR: &str =
    "unknown repo, supply a valid `repo` name (run repos_list to discover names)";
const AMBIGUOUS_REPO_ERR: &str =
    "ambiguous: multiple repos registered, supply `repo` (run repos_list to discover names)";

/// Resolve `repo_arg` (or, when absent, the sole live registry entry) to a
/// single [`RepoTarget`]. Never aggregates across repos, see the module doc.
///
/// `repo_arg` is validated with [`validate_mcp_arg`] first (SEC-002).
fn resolve_single_repo(
    repos: &HashMap<String, PathBuf>,
    repo_arg: Option<&str>,
) -> Result<RepoTarget, String> {
    if let Some(name) = repo_arg {
        if let Err(reason) = validate_mcp_arg(name) {
            tracing::warn!("observability tool rejected invalid repo arg: {reason}");
            return Err(UNKNOWN_REPO_ERR.to_string());
        }
    }

    // Only registry entries whose db still exists are "live", mirrors
    // `collect_global`'s stale-entry filter (tools.rs).
    let live: Vec<(&String, &PathBuf)> = repos.iter().filter(|(_, db)| db.exists()).collect();

    let (name, db_path) = match repo_arg {
        Some(wanted) => live
            .into_iter()
            .find(|(k, _)| k.as_str() == wanted)
            .ok_or_else(|| UNKNOWN_REPO_ERR.to_string())?,
        None => match live.len() {
            0 => return Err(UNKNOWN_REPO_ERR.to_string()),
            1 => live[0],
            _ => return Err(AMBIGUOUS_REPO_ERR.to_string()),
        },
    };

    let root = db_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| UNKNOWN_REPO_ERR.to_string())?;

    Ok(RepoTarget {
        name: name.clone(),
        root: root.to_path_buf(),
        db_path: db_path.clone(),
    })
}

/// The stdio server's repo root, from the `repo_root` meta key the daemon
/// writes at init (daemon lib.rs). `None` when absent/empty, a store opened
/// standalone against a bare db with no daemon-written metadata.
fn stdio_repo_root(store: &SqliteStore) -> Option<PathBuf> {
    store
        .get_meta("repo_root")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Repo label for the stdio (single-repo) variants: the `corpus` meta value,
/// falling back to empty (never fabricated). Global-mode variants use the
/// registry key instead (`RepoTarget::name`).
fn stdio_repo_label(store: &SqliteStore) -> String {
    store.get_meta("corpus").ok().flatten().unwrap_or_default()
}

/// Commits between the indexed commit and `HEAD`, or `None` when git is
/// unavailable, `root` is not a repo, or `indexed` is not a resolvable SHA.
/// Never fabricates a `0` on failure, callers must treat `None` as unknown,
/// not "up to date" (see `get_index_status`'s doc comment).
///
/// `indexed` is validated as an ASCII-hex string before being interpolated
/// into the git revision range, mirroring the guard `tools.rs` uses before
/// forwarding a stored SHA to git.
fn commits_behind(root: &Path, indexed: &str) -> Option<u64> {
    if indexed.is_empty() || !indexed.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let range = format!("{indexed}..HEAD");
    let out = std::process::Command::new("git")
        .args(["-C", &root.to_string_lossy(), "rev-list", "--count", &range])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

/// `true` when `git status --porcelain` reports any tracked-file change.
/// Untracked files are deliberately excluded (`--untracked-files=no`): an
/// untracked scratch file does not make the *index* stale relative to the
/// tracked tree, which is what this field is answering. `None` when git is
/// unavailable or `root` is not a repo.
fn working_tree_dirty(root: &Path) -> Option<bool> {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &root.to_string_lossy(),
            "status",
            "--porcelain",
            "--untracked-files=no",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(!out.stdout.is_empty())
}

/// `true` iff a live daemon currently holds `<root>/.travsr/daemon.lock`.
///
/// Deliberately NOT `daemon_client::daemon_lock_held` (travsr-cli): that
/// helper opens the lock file with `.create(true)`, which would create
/// `.travsr/daemon.lock` as a side effect, unacceptable for a read-only MCP
/// tool. This variant opens the file only if it already exists and returns
/// `false` (not "unknown") when it is absent, matching the CLI helper's
/// semantics for every case that does not require creating the file.
fn daemon_running(root: &Path) -> bool {
    use fs2::FileExt as _;
    let lock_path = root.join(".travsr").join("daemon.lock");
    let Ok(file) = std::fs::OpenOptions::new().read(true).open(&lock_path) else {
        return false; // absent / unopenable → no daemon possible
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

/// Serialize `payload` and wrap it in the `<travsr-data>` envelope. No output
/// byte cap here, each tool bounds its own payload (`get_daemon_logs` via
/// `MAX_TOTAL_BYTES`; the others are small, bounded-cardinality JSON).
fn json_response(payload: &serde_json::Value) -> String {
    let body = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    wrap_envelope(&body)
}

fn error_payload(reason: &str) -> serde_json::Value {
    serde_json::json!({ "error": reason })
}

// ── #636: phase_b_warnings decoding (shared by get_index_status) ──────────────

/// Per-language Phase B failure/skip classification decoded from the
/// `phase_b_warnings` meta key (comma-separated `class:lang[:extra]` entries,
/// written by the daemon, see `travsr_daemon::run_semantic_layer`). Mirrors
/// `travsr status`'s classification (`travsr-cli/src/status.rs`) so the CLI
/// and this tool never disagree about why a language's Phase B is degraded.
///
/// Returns `language -> (state, detail)` where `state` is `"failed"` or
/// `"unavailable"` per the #636 plan's classification table. Unknown warning
/// classes (e.g. `scip_unification_misses`, which is not per-language) are
/// ignored, they're not a language state.
fn decode_phase_b_warnings(warnings: &str) -> HashMap<String, (&'static str, String)> {
    let mut out = HashMap::new();
    for warn in warnings.split(',') {
        let warn = warn.trim();
        if warn.is_empty() {
            continue;
        }
        let mut parts = warn.splitn(2, ':');
        let (Some(class), Some(rest)) = (parts.next(), parts.next()) else {
            continue;
        };
        match class {
            "crashed" => {
                out.insert(
                    rest.to_string(),
                    (
                        "failed",
                        format!(
                            "phase B analyzer for '{rest}' crashed, re-run \
                             `travsr init --semantic` to retry"
                        ),
                    ),
                );
            }
            "version_mismatch" => {
                let v: Vec<&str> = rest.splitn(3, ':').collect();
                if let [lang, expected, got] = v[..] {
                    out.insert(
                        lang.to_string(),
                        (
                            "failed",
                            format!(
                                "'{lang}' sidecar protocol v{got} != expected v{expected}, \
                                 run `travsr lang install {lang}`"
                            ),
                        ),
                    );
                }
            }
            "needs_approval" => {
                out.insert(
                    rest.to_string(),
                    (
                        "unavailable",
                        format!(
                            "'{rest}' requires elevated sandbox approval, \
                             run `travsr lang approve {rest}`"
                        ),
                    ),
                );
            }
            "skipped_unregistered" => {
                out.insert(
                    rest.to_string(),
                    (
                        "unavailable",
                        format!(
                            "'{rest}' sources found but semantic indexing is not set up. \
                             Run `travsr lang install {rest}`"
                        ),
                    ),
                );
            }
            "skipped_no_analyzer" => {
                out.insert(
                    rest.to_string(),
                    (
                        "unavailable",
                        format!(
                            "'{rest}' is registered but its analyzer binary is missing. \
                             Run `travsr lang install {rest}`"
                        ),
                    ),
                );
            }
            "skipped_no_compdb" => {
                out.insert(
                    rest.to_string(),
                    (
                        "unavailable",
                        format!(
                            "'{rest}' semantic indexing needs a compile_commands.json at the \
                             repo root. Generate one (e.g. `bear -- make`, or CMake's \
                             CMAKE_EXPORT_COMPILE_COMMANDS) to enable it"
                        ),
                    ),
                );
            }
            _ => {}
        }
    }
    out
}

// ── get_index_status ────────────────────────────────────────────────────────

/// Semantic (embeddings + rerank) section of `get_index_status`'s payload.
fn semantic_block(store: &SqliteStore, root: Option<&Path>) -> serde_json::Value {
    let model_and_root: Option<(String, &Path)> =
        root.and_then(|r| travsr_plugin_host::repo_backend_id(r).map(|m| (m, r)));
    let has_embed_db = store.has_embed_db();

    let embeddings = match (&model_and_root, has_embed_db) {
        (None, false) => "disabled",
        (Some(_), false) => "error",
        (None, true) => "building",
        (Some((model_id, r)), true) => {
            let db_path = r.join(".travsr").join("graph.db");
            let threshold =
                travsr_plugin_host::derive_phase1_threshold_for_status(&db_path).unwrap_or(0);
            match store.embed_progress(model_id, threshold) {
                Err(_) => "error",
                Ok((total, embedded, _phase1_total, _phase1_done)) => {
                    if total == 0 || embedded >= total {
                        "ready"
                    } else {
                        "building"
                    }
                }
            }
        }
    };

    let rerank_installed = matches!(rerank::rerank_status(), "installed" | "ready");
    serde_json::json!({
        "embeddings": embeddings,
        "model": model_and_root.map(|(m, _)| m),
        "calibrated": rerank::manifest_present(),
        "rerank": if rerank_installed { "installed" } else { "absent" },
    })
}

/// Build the `get_index_status` JSON payload. `repo_label` is the value to
/// report as `repo` (registry key in global mode, `corpus` meta in stdio
/// mode); `root` is the repo's working-tree root when known (`None` disables
/// every git-derived signal (staleness, `head_commit`, `daemon_running`)
/// rather than fabricating one).
fn index_status_payload(
    store: &SqliteStore,
    repo_label: &str,
    root: Option<&Path>,
) -> serde_json::Value {
    let repo = sanitize_log_value(repo_label, 256);

    let schema_version = store.current_schema_version().unwrap_or(0);
    let last_commit = store
        .get_meta("last_commit")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let head_commit = root.and_then(git_short_head);

    let behind_by = match (root, last_commit.as_deref()) {
        (Some(r), Some(indexed)) => commits_behind(r, indexed),
        _ => None,
    };
    let is_stale = behind_by.is_some_and(|n| n > 0);
    let dirty = root.and_then(working_tree_dirty);

    let node_count = store.node_count().unwrap_or(0);
    let edge_count = store.edge_count().unwrap_or(0);

    // Phase A: the Tree-sitter structural pass.
    let phase_a_state = if last_commit.is_none() {
        "pending"
    } else if node_count == 0 {
        "failed"
    } else {
        "done"
    };

    // Phase B: per-language semantic (SCIP/LSIF) state, see #636 plan A5.
    let phase_b_commit = store
        .get_meta("phase_b_commit")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let warnings_raw = store
        .get_meta("phase_b_warnings")
        .ok()
        .flatten()
        .unwrap_or_default();
    let decoded_warnings = decode_phase_b_warnings(&warnings_raw);
    // #636 A4: no persisted "job in flight" signal exists; derive it from the
    // live daemon-lock probe plus a commit mismatch. Best-effort, documented
    // on the field, not tested by any acceptance criterion.
    let daemon_up = root.is_some_and(daemon_running);
    let job_in_flight = daemon_up && phase_b_commit != last_commit;

    let languages = store.language_distribution().unwrap_or_default();
    let mut lang_entries = Vec::with_capacity(languages.len());
    let mut lang_states: Vec<&'static str> = Vec::with_capacity(languages.len());
    for (lang, _count) in &languages {
        let (state, detail) = if let Some((cls, msg)) = decoded_warnings.get(lang) {
            (*cls, Some(msg.clone()))
        } else if store.has_refcall_edges_for_language(lang) {
            ("done", None)
        } else {
            let pending = phase_b_commit.is_none() || phase_b_commit != last_commit;
            if pending {
                ("pending", None)
            } else {
                ("running", None)
            }
        };
        lang_states.push(state);
        let mut entry = serde_json::json!({ "language": lang, "state": state });
        if let Some(d) = detail {
            entry["detail"] = serde_json::json!(sanitize_log_value(&d, 512));
        }
        lang_entries.push(entry);
    }

    let degraded = |s: &&str| *s == "failed" || *s == "unavailable";
    let phase_b_state = if lang_states.is_empty() {
        "pending"
    } else if lang_states.iter().all(|s| *s == "done") {
        "done"
    } else if lang_states.iter().all(degraded) {
        "failed"
    } else if lang_states.contains(&"done") && lang_states.iter().any(degraded) {
        "partial"
    } else if lang_states.contains(&"pending") {
        "pending"
    } else {
        "running"
    };

    serde_json::json!({
        "repo": repo,
        "schema_version": schema_version,
        "indexed_commit": last_commit,
        "head_commit": head_commit,
        "staleness": {
            "behind_by": behind_by,
            "is_stale": is_stale,
            "working_tree_dirty": dirty,
        },
        "counts": { "nodes": node_count, "edges": edge_count },
        "phase_a": { "state": phase_a_state },
        "phase_b": {
            "state": phase_b_state,
            "job_in_flight": job_in_flight,
            "languages": lang_entries,
        },
        "semantic": semantic_block(store, root),
    })
}

/// Index freshness / completeness snapshot for the caller's own repo
/// (stdio, single-repo server).
pub fn get_index_status(store: &SqliteStore) -> String {
    let root = stdio_repo_root(store);
    let label = stdio_repo_label(store);
    json_response(&index_status_payload(store, &label, root.as_deref()))
}

/// Global-mode variant: resolves `repo_arg` (or the sole live repo) via
/// [`resolve_single_repo`] and opens it read-only. Never aggregates across
/// repos, see the module doc.
pub fn get_index_status_global(repos: &HashMap<String, PathBuf>, repo_arg: Option<&str>) -> String {
    let target = match resolve_single_repo(repos, repo_arg) {
        Ok(t) => t,
        Err(reason) => return json_response(&error_payload(&reason)),
    };
    let store = match SqliteStore::open_read_only(&target.db_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "get_index_status_global failed to open {}: {e}",
                target.db_path.display()
            );
            return json_response(&error_payload("failed to open repo database"));
        }
    };
    json_response(&index_status_payload(
        &store,
        &target.name,
        Some(&target.root),
    ))
}

// ── get_daemon_logs ─────────────────────────────────────────────────────────

/// Maximum `tail` accepted (clamped, never rejected).
const MAX_TAIL: usize = 500;
/// `tail` when the caller omits it.
pub const DEFAULT_TAIL: usize = 50;
/// Serialized-payload byte ceiling. Independent of `MAX_TAIL`: a large `tail`
/// of long lines can still blow this budget, which is exactly what this cap
/// (rather than the tail cap alone) exists to bound (#636 X2).
const MAX_TOTAL_BYTES: usize = 32_768;
/// Per-field sanitize/truncate ceiling (message, target, each field value).
const MAX_FIELD_BYTES: usize = 512;
/// Bounded per-file read window. A daemon log line is ~100-300 bytes, so this
/// comfortably covers `MAX_TAIL` lines from the active file without reading a
/// potentially large rotated log in full.
const PER_FILE_READ_BYTES: usize = 262_144;

/// Known `tracing` severity levels, ordered most-to-least severe. Index is
/// used as the numeric rank for the `level` (minimum severity) filter.
const KNOWN_LEVELS: &[&str] = &["ERROR", "WARN", "INFO", "DEBUG", "TRACE"];

fn level_rank(level: &str) -> usize {
    KNOWN_LEVELS
        .iter()
        .position(|&l| l == level)
        .unwrap_or(KNOWN_LEVELS.len())
}

/// Parse the `level` MCP arg into a minimum-severity rank. Unknown/absent
/// values default to `info`'s rank rather than erroring (#636 plan step 5.4).
fn normalize_min_level(level: &str) -> usize {
    match level.to_ascii_uppercase().as_str() {
        "ERROR" => 0,
        "WARN" => 1,
        "DEBUG" => 3,
        _ => 2, // "INFO", empty, or unrecognized
    }
}

/// Candidate `daemon.log*` file names directly under `.travsr`, sorted
/// newest-first. Rejects symlinks and non-regular files, and only ever reads
/// `file_name()` (an `OsStr`, no path components), no path is ever built
/// from a user-controlled string (#636 plan step 5.1).
fn list_daemon_log_files(travsr_dir: &Path) -> Vec<String> {
    let Ok(read_dir) = std::fs::read_dir(travsr_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read_dir
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("daemon.log") {
                return false;
            }
            matches!(entry.path().symlink_metadata(), Ok(md) if md.is_file())
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    // "daemon.log.YYYY-MM-DD" sorts lexicographically == chronologically.
    names.sort_by(|a, b| b.cmp(a));
    names
}

/// Read up to the last [`PER_FILE_READ_BYTES`] of `path`, return its lines
/// newest-first. A partial first line from the byte-window cut is dropped
/// (best-effort: the line is presumed present in full in an even-older read
/// that this bounded window intentionally does not perform).
fn read_tail_lines_newest_first(path: &Path) -> Vec<String> {
    let Ok(data) = std::fs::read(path) else {
        return Vec::new();
    };
    let start = data.len().saturating_sub(PER_FILE_READ_BYTES);
    let text = String::from_utf8_lossy(&data[start..]);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // possibly truncated mid-line by the window cut
    }
    lines.reverse();
    lines
}

/// One parsed `tracing` log line: `TS LEVEL target: message key=value ...`.
struct ParsedLine {
    ts: String,
    level: String,
    target: String,
    message: String,
    fields: Vec<(String, String)>,
}

/// Parse `raw` into structured fields, or `None` when it does not match the
/// expected shape (fallback to a raw entry, see [`build_log_entry`]).
/// Deliberately all-or-nothing: prefer under-parsing over misclassifying a
/// line that merely resembles the format (#636 plan risk 5).
fn parse_log_line(raw: &str) -> Option<ParsedLine> {
    let line = raw.trim_end();
    let mut top = line.splitn(2, char::is_whitespace);
    let ts = top.next()?.trim();
    let after_ts = top.next()?.trim_start();
    if ts.is_empty() || after_ts.is_empty() {
        return None;
    }
    // Shape check: a tracing timestamp is RFC3339-ish (digits/-:.TZ+ only).
    if !ts
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | '.' | 'T' | 'Z' | '+'))
    {
        return None;
    }

    let mut rest = after_ts.splitn(2, char::is_whitespace);
    let level = rest.next()?.trim().to_ascii_uppercase();
    let after_level = rest.next()?.trim_start();
    if !KNOWN_LEVELS.contains(&level.as_str()) {
        return None;
    }

    let colon_idx = after_level.find(": ")?;
    let target = after_level[..colon_idx].to_string();
    let remainder = &after_level[colon_idx + 2..];

    let (message, fields) = split_message_and_fields(remainder);
    Some(ParsedLine {
        ts: ts.to_string(),
        level,
        target,
        message,
        fields,
    })
}

/// Split `s` into `(message, fields)` by pulling `key=value` pairs off the
/// right end, quote-aware (a `"..."` value may contain spaces). Stops at the
/// first trailing token that isn't a `key=value` pair, everything before
/// that point is the message.
fn split_message_and_fields(s: &str) -> (String, Vec<(String, String)>) {
    let tokens = tokenize_whitespace_quoted(s);
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut split_at = tokens.len();
    while split_at > 0 {
        let tok = &tokens[split_at - 1];
        let Some(eq) = tok.find('=') else { break };
        let key = &tok[..eq];
        let key_ok = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !key_ok {
            break;
        }
        let mut value = tok[eq + 1..].to_string();
        if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value = value[1..value.len() - 1].to_string();
        }
        fields.push((key.to_string(), value));
        split_at -= 1;
    }
    fields.reverse();
    (tokens[..split_at].join(" "), fields)
}

/// Whitespace tokenizer that keeps a `"..."` run (including embedded spaces)
/// as one token, matching the shape of fields like `reason="a b c"`.
fn tokenize_whitespace_quoted(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
            cur.push(c);
        } else if c.is_whitespace() && !in_quotes {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Build one sanitized log entry from a raw line. Every message/target/field
/// value is redacted + capped via `sanitize_log_value` (X1/X2). Unparseable
/// lines fall back to `{ts: null, level: "raw", ...}` and are always
/// included regardless of the `level` filter (#636 plan step 5.3-5.4).
fn build_log_entry(raw: &str) -> serde_json::Value {
    match parse_log_line(raw) {
        Some(p) => {
            let mut fields_obj = serde_json::Map::with_capacity(p.fields.len());
            for (k, v) in &p.fields {
                fields_obj.insert(
                    k.clone(),
                    serde_json::json!(sanitize_log_value(v, MAX_FIELD_BYTES)),
                );
            }
            serde_json::json!({
                "ts": p.ts,
                "level": p.level,
                "target": sanitize_log_value(&p.target, MAX_FIELD_BYTES),
                "message": sanitize_log_value(&p.message, MAX_FIELD_BYTES),
                "fields": fields_obj,
            })
        }
        None => serde_json::json!({
            "ts": serde_json::Value::Null,
            "level": "raw",
            "target": "",
            "message": sanitize_log_value(raw, MAX_FIELD_BYTES),
            "fields": {},
        }),
    }
}

/// Build the `get_daemon_logs` JSON payload for a resolved repo root.
/// `root = None` (repo root unknown) returns an empty-but-valid payload
/// rather than guessing a path.
fn daemon_logs_payload(
    repo_label: &str,
    root: Option<&Path>,
    tail: usize,
    level: &str,
) -> serde_json::Value {
    let repo = sanitize_log_value(repo_label, 256);
    let tail = tail.clamp(1, MAX_TAIL);
    let min_level = normalize_min_level(level);

    let Some(root) = root else {
        return serde_json::json!({
            "repo": repo,
            "source": serde_json::Value::Null,
            "daemon_running": false,
            "returned": 0,
            "truncated": false,
            "entries": [],
        });
    };

    let daemon_up = daemon_running(root);
    let travsr_dir = root.join(".travsr");
    let candidates = list_daemon_log_files(&travsr_dir);

    // Over-collect by one so we can distinguish "exactly tail lines exist"
    // from "more exist" without a second pass (drives the `truncated` flag).
    let want = tail.saturating_add(1);
    let mut raw_newest_first: Vec<String> = Vec::with_capacity(want.min(4096));
    let mut source: Option<String> = None;
    for file_name in &candidates {
        if raw_newest_first.len() >= want {
            break;
        }
        let chunk = read_tail_lines_newest_first(&travsr_dir.join(file_name));
        if !chunk.is_empty() && source.is_none() {
            source = Some(file_name.clone());
        }
        raw_newest_first.extend(chunk);
    }
    let tail_truncated = raw_newest_first.len() > tail;
    raw_newest_first.truncate(tail);

    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(raw_newest_first.len());
    let mut total_bytes = 0usize;
    let mut byte_truncated = false;
    for raw in &raw_newest_first {
        let entry = build_log_entry(raw);
        let passes = match entry["level"].as_str() {
            Some("raw") => true,
            Some(lvl) => level_rank(lvl) <= min_level,
            None => true,
        };
        if !passes {
            continue;
        }
        let entry_bytes = serde_json::to_string(&entry).map(|s| s.len()).unwrap_or(0);
        if !entries.is_empty() && total_bytes + entry_bytes > MAX_TOTAL_BYTES {
            byte_truncated = true;
            break;
        }
        total_bytes += entry_bytes;
        entries.push(entry);
    }

    serde_json::json!({
        "repo": repo,
        "source": source,
        "daemon_running": daemon_up,
        "returned": entries.len(),
        "truncated": tail_truncated || byte_truncated,
        "entries": entries,
    })
}

/// Recent daemon log entries for the caller's own repo (stdio server).
/// Read-only: never creates `.travsr/daemon.lock`, never opens the store
/// read-write, never reads outside `<root>/.travsr/`.
pub fn get_daemon_logs(store: &SqliteStore, tail: usize, level: &str) -> String {
    let root = stdio_repo_root(store);
    let label = stdio_repo_label(store);
    json_response(&daemon_logs_payload(&label, root.as_deref(), tail, level))
}

/// Global-mode variant. `repo` is REQUIRED to be unambiguous, this never
/// falls back to `LAUNCH_CWD` or any other implicit root, and it never reads
/// logs for more than one repo per call (cross-repo leak guard, #636 X-AC5).
pub fn get_daemon_logs_global(
    repos: &HashMap<String, PathBuf>,
    tail: usize,
    level: &str,
    repo_arg: Option<&str>,
) -> String {
    let target = match resolve_single_repo(repos, repo_arg) {
        Ok(t) => t,
        Err(reason) => return json_response(&error_payload(&reason)),
    };
    json_response(&daemon_logs_payload(
        &target.name,
        Some(&target.root),
        tail,
        level,
    ))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn git(args: &[&str], cwd: &Path) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("git command");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_git_repo(dir: &Path) {
        git(&["-c", "init.defaultBranch=main", "init", "-q"], dir);
        git(&["config", "user.email", "test@example.com"], dir);
        git(&["config", "user.name", "Test"], dir);
        std::fs::write(dir.join("a.txt"), b"one").unwrap();
        git(&["add", "."], dir);
        git(&["commit", "-q", "-m", "init"], dir);
    }

    fn head_short(dir: &Path) -> String {
        let out = std::process::Command::new("git")
            .args(["-C", &dir.to_string_lossy(), "rev-parse", "--short", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    // ── resolve_single_repo ───────────────────────────────────────────────

    #[test]
    fn resolve_single_repo_picks_sole_live_entry_when_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".travsr").join("graph.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"x").unwrap();
        let mut repos = HashMap::new();
        repos.insert("only".to_string(), db.clone());

        let target = resolve_single_repo(&repos, None).unwrap();
        assert_eq!(target.name, "only");
        assert_eq!(target.root, tmp.path());
    }

    #[test]
    fn resolve_single_repo_ambiguous_with_multiple_live_entries_and_no_arg() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        let mut repos = HashMap::new();
        for (name, tmp) in [("a", &tmp1), ("b", &tmp2)] {
            let db = tmp.path().join(".travsr").join("graph.db");
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            std::fs::write(&db, b"x").unwrap();
            repos.insert(name.to_string(), db);
        }
        let err = resolve_single_repo(&repos, None).unwrap_err();
        assert_eq!(err, AMBIGUOUS_REPO_ERR);
    }

    #[test]
    fn resolve_single_repo_unknown_and_no_db_give_identical_error() {
        let repos: HashMap<String, PathBuf> = HashMap::new();
        let unknown = resolve_single_repo(&repos, Some("nope")).unwrap_err();

        let tmp = tempfile::tempdir().unwrap();
        let mut repos2 = HashMap::new();
        // Registered but the db path does not exist, "stale" entry.
        repos2.insert(
            "stale".to_string(),
            tmp.path().join(".travsr").join("graph.db"),
        );
        let stale = resolve_single_repo(&repos2, Some("stale")).unwrap_err();
        assert_eq!(unknown, stale, "identical error text prevents probing");
        assert_eq!(unknown, UNKNOWN_REPO_ERR);
    }

    #[test]
    fn resolve_single_repo_rejects_path_traversal_in_repo_arg() {
        let repos: HashMap<String, PathBuf> = HashMap::new();
        assert!(resolve_single_repo(&repos, Some("../../etc")).is_err());
        assert!(resolve_single_repo(&repos, Some("/etc")).is_err());
        assert!(resolve_single_repo(&repos, Some("%2e%2e%2f")).is_err());
    }

    // ── commits_behind / working_tree_dirty ──────────────────────────────

    #[test]
    fn commits_behind_zero_when_indexed_is_head() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let head = head_short(tmp.path());
        assert_eq!(commits_behind(tmp.path(), &head), Some(0));
    }

    #[test]
    fn commits_behind_counts_new_commits() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let indexed = head_short(tmp.path());
        std::fs::write(tmp.path().join("b.txt"), b"two").unwrap();
        git(&["add", "."], tmp.path());
        git(&["commit", "-q", "-m", "second"], tmp.path());
        std::fs::write(tmp.path().join("c.txt"), b"three").unwrap();
        git(&["add", "."], tmp.path());
        git(&["commit", "-q", "-m", "third"], tmp.path());

        assert_eq!(commits_behind(tmp.path(), &indexed), Some(2));
    }

    #[test]
    fn commits_behind_none_on_bogus_sha() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        assert_eq!(commits_behind(tmp.path(), "deadbeef"), None);
    }

    #[test]
    fn working_tree_dirty_ignores_untracked_files() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("untracked.txt"), b"scratch").unwrap();
        assert_eq!(working_tree_dirty(tmp.path()), Some(false));
    }

    #[test]
    fn working_tree_dirty_true_on_tracked_change() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), b"changed").unwrap();
        assert_eq!(working_tree_dirty(tmp.path()), Some(true));
    }

    // ── daemon_running ────────────────────────────────────────────────────

    #[test]
    fn daemon_running_false_and_no_lock_file_created_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".travsr")).unwrap();
        assert!(!daemon_running(tmp.path()));
        assert!(!tmp.path().join(".travsr").join("daemon.lock").exists());
    }

    // ── get_index_status payload ──────────────────────────────────────────

    #[test]
    fn index_status_reports_behind_by_and_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let indexed = head_short(tmp.path());
        std::fs::write(tmp.path().join("b.txt"), b"two").unwrap();
        git(&["add", "."], tmp.path());
        git(&["commit", "-q", "-m", "second"], tmp.path());
        std::fs::write(tmp.path().join("c.txt"), b"three").unwrap();
        git(&["add", "."], tmp.path());
        git(&["commit", "-q", "-m", "third"], tmp.path());

        let mut store = SqliteStore::open_in_memory().unwrap();
        store.set_meta("last_commit", &indexed).unwrap();

        let payload = index_status_payload(&store, "repo", Some(tmp.path()));
        assert_eq!(payload["staleness"]["behind_by"], 2);
        assert_eq!(payload["staleness"]["is_stale"], true);
    }

    #[test]
    fn index_status_empty_store_has_pending_phase_a_and_no_panic() {
        let store = SqliteStore::open_in_memory().unwrap();
        let payload = index_status_payload(&store, "repo", None);
        assert_eq!(payload["phase_a"]["state"], "pending");
        assert_eq!(payload["staleness"]["behind_by"], serde_json::Value::Null);
    }

    #[test]
    fn index_status_phase_b_partial_with_failed_unavailable_and_done_languages() {
        use travsr_core::{Node, VName};
        use travsr_store::Store as _;
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.set_meta("last_commit", "abc123").unwrap();
        store.set_meta("phase_b_commit", "abc123").unwrap();
        store
            .set_meta("phase_b_warnings", "crashed:go,skipped_unregistered:java")
            .unwrap();

        for (lang, sig) in [("go", "fn:a"), ("java", "fn:b"), ("typescript", "fn:c")] {
            let node = Node::new(
                VName::new("corpus", "main", format!("src/{lang}.x"), lang, sig),
                "function",
            );
            let id = store.put_node(&node).unwrap();
            if lang == "typescript" {
                store
                    .put_edge(&travsr_core::Edge::new(
                        id,
                        id,
                        travsr_core::EdgeKind::RefCall,
                    ))
                    .ok(); // self-edge is enough to make has_refcall_edges_for_language true
            }
        }

        let payload = index_status_payload(&store, "repo", None);
        assert_eq!(payload["phase_b"]["state"], "partial");
        let langs = payload["phase_b"]["languages"].as_array().unwrap();
        let state_of = |lang: &str| -> String {
            langs.iter().find(|l| l["language"] == lang).unwrap()["state"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(state_of("go"), "failed");
        assert_eq!(state_of("java"), "unavailable");
        assert_eq!(state_of("typescript"), "done");
    }

    // ── get_daemon_logs parsing / caps ────────────────────────────────────

    #[test]
    fn parse_log_line_extracts_ts_level_target_message_fields_with_quoted_value() {
        let line = r#"2026-08-12T05:25:16.221830Z ERROR travsr_indexer::ra_runner: rust-analyzer sandbox unavailable, install bubblewrap for isolation repo=/home/alice/proj reason="bwrap not found on PATH""#;
        let parsed = parse_log_line(line).expect("must parse");
        assert_eq!(parsed.ts, "2026-08-12T05:25:16.221830Z");
        assert_eq!(parsed.level, "ERROR");
        assert_eq!(parsed.target, "travsr_indexer::ra_runner");
        assert!(parsed
            .message
            .starts_with("rust-analyzer sandbox unavailable"));
        let field_map: HashMap<_, _> = parsed.fields.into_iter().collect();
        assert_eq!(field_map.get("repo").unwrap(), "/home/alice/proj");
        assert_eq!(field_map.get("reason").unwrap(), "bwrap not found on PATH");
    }

    #[test]
    fn parse_log_line_none_for_unparseable_line_and_entry_falls_back_to_raw() {
        assert!(parse_log_line("this is not a tracing log line").is_none());
        let entry = build_log_entry("this is not a tracing log line");
        assert_eq!(entry["level"], "raw");
        assert_eq!(entry["ts"], serde_json::Value::Null);
        assert_eq!(entry["message"], "this is not a tracing log line");
    }

    #[test]
    fn daemon_logs_level_filter_excludes_less_severe_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let travsr_dir = tmp.path().join(".travsr");
        std::fs::create_dir_all(&travsr_dir).unwrap();
        let lines = "2026-01-01T00:00:00Z ERROR mod: err one\n\
                     2026-01-01T00:00:01Z WARN mod: warn one\n\
                     2026-01-01T00:00:02Z INFO mod: info one\n";
        std::fs::write(travsr_dir.join("daemon.log.2026-01-01"), lines).unwrap();

        let payload = daemon_logs_payload("repo", Some(tmp.path()), 10, "error");
        let entries = payload["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["level"], "ERROR");
    }

    #[test]
    fn daemon_logs_tail_cap_returns_newest_and_sets_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let travsr_dir = tmp.path().join(".travsr");
        std::fs::create_dir_all(&travsr_dir).unwrap();
        let mut lines = String::new();
        for i in 0..10 {
            lines.push_str(&format!("2026-01-01T00:00:{i:02}Z INFO mod: line {i}\n"));
        }
        std::fs::write(travsr_dir.join("daemon.log.2026-01-01"), lines).unwrap();

        let payload = daemon_logs_payload("repo", Some(tmp.path()), 3, "info");
        assert_eq!(payload["returned"], 3);
        assert_eq!(payload["truncated"], true);
        let entries = payload["entries"].as_array().unwrap();
        // Newest-first: line 9, 8, 7.
        assert!(entries[0]["message"].as_str().unwrap().contains("line 9"));
        assert!(entries[1]["message"].as_str().unwrap().contains("line 8"));
        assert!(entries[2]["message"].as_str().unwrap().contains("line 7"));
    }

    #[test]
    fn daemon_logs_redacts_home_path_and_bearer_token() {
        let tmp = tempfile::tempdir().unwrap();
        let travsr_dir = tmp.path().join(".travsr");
        std::fs::create_dir_all(&travsr_dir).unwrap();
        let line = "2026-01-01T00:00:00Z ERROR mod: failed for repo=/home/alice/proj \
                     Authorization: Bearer abc.def.ghi\n";
        std::fs::write(travsr_dir.join("daemon.log.2026-01-01"), line).unwrap();

        let payload = daemon_logs_payload("repo", Some(tmp.path()), 10, "info");
        let serialized = payload.to_string();
        assert!(!serialized.contains("/home/alice"), "got: {serialized}");
        assert!(!serialized.contains("abc.def.ghi"), "got: {serialized}");
    }

    #[test]
    fn daemon_logs_byte_cap_bounds_serialized_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let travsr_dir = tmp.path().join(".travsr");
        std::fs::create_dir_all(&travsr_dir).unwrap();
        let mut lines = String::new();
        for i in 0..500 {
            let filler = "x".repeat(1024);
            lines.push_str(&format!(
                "2026-01-01T00:00:00Z INFO mod: line {i} {filler}\n"
            ));
        }
        std::fs::write(travsr_dir.join("daemon.log.2026-01-01"), lines).unwrap();

        let payload = daemon_logs_payload("repo", Some(tmp.path()), MAX_TAIL, "info");
        assert_eq!(payload["truncated"], true);
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(
            serialized.len() <= MAX_TOTAL_BYTES + 4096,
            "payload should be roughly bounded by MAX_TOTAL_BYTES, got {}",
            serialized.len()
        );
    }

    #[test]
    fn daemon_logs_cross_repo_never_leaks_other_repos_marker() {
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        for (tmp, marker) in [(&tmp_a, "MARKER_A"), (&tmp_b, "MARKER_B")] {
            let travsr_dir = tmp.path().join(".travsr");
            std::fs::create_dir_all(&travsr_dir).unwrap();
            std::fs::write(
                travsr_dir.join("daemon.log.2026-01-01"),
                format!("2026-01-01T00:00:00Z INFO mod: {marker}\n"),
            )
            .unwrap();
        }
        let mut repos = HashMap::new();
        repos.insert(
            "a".to_string(),
            tmp_a.path().join(".travsr").join("graph.db"),
        );
        repos.insert(
            "b".to_string(),
            tmp_b.path().join(".travsr").join("graph.db"),
        );
        // graph.db does not need to exist for this test's purposes, but
        // resolve_single_repo filters on db existence, so create stubs.
        std::fs::write(tmp_a.path().join(".travsr").join("graph.db"), b"x").unwrap();
        std::fs::write(tmp_b.path().join(".travsr").join("graph.db"), b"x").unwrap();

        let out_a = get_daemon_logs_global(&repos, 10, "info", Some("a"));
        assert!(out_a.contains("MARKER_A"));
        assert!(!out_a.contains("MARKER_B"));

        let out_omitted = get_daemon_logs_global(&repos, 10, "info", None);
        assert!(!out_omitted.contains("MARKER_A"));
        assert!(!out_omitted.contains("MARKER_B"));
    }
}
