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
}
