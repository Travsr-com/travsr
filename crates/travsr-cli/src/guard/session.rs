//! The strict-mode per-session release valve (#916).
//!
//! Strict mode denies a search the graph can answer and names the Travsr call
//! that replaces it. That is right until the graph comes back empty: the agent
//! then genuinely needs plain text search, and a guard that denies it again has
//! turned a redirect into a dead end.
//!
//! Two independent releases, both scoped to one `session_id` so neither
//! disables the guard globally:
//!
//! 1. **The agent already asked Travsr.** The installed hook matches the travsr
//!    MCP tools as well as the search tools, so the guard *observes* a
//!    `get_callers(symbol="X")` go past (and always allows it). A later search
//!    for `X` in that session is then released: the agent has been to the graph
//!    and is coming back for the raw text, which is exactly the move the
//!    fallback rule in the agent guidance tells it to make.
//!
//! 2. **The guard already denied this term once.** A second identical search in
//!    the same session is released whatever happened in between. This is the
//!    backstop that makes the deny bounded by construction: at most one denial
//!    per `(session, term)`, so no agent can be held in a loop by this guard,
//!    including one whose Travsr query the hook never saw.
//!
//! `PreToolUse` fires *before* a call, so the guard sees that the agent asked
//! Travsr about `X` but never what came back. Release 1 therefore releases on
//! the question, not on an empty answer. Releasing slightly early costs a
//! redirect that was not needed; the other error costs the agent its fallback.
//!
//! State is a single JSON file under `.travsr/`, which is git-ignored by
//! `init`. Every read and write fails open: no state means no release, which
//! only ever falls back to release 2.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Sessions older than this are dropped on the next write. A Claude Code
/// session that has not touched the guard for a day is over.
const SESSION_TTL_SECS: u64 = 24 * 60 * 60;

/// Hard ceiling on retained sessions, newest first. The TTL alone is not a
/// bound: a machine that opens hundreds of short sessions in a day would grow
/// the file all day and only prune at midnight.
const MAX_SESSIONS: usize = 64;

/// Hard ceiling on remembered terms per session, oldest dropped first. Bounds
/// one very long session, which the session-level caps cannot.
const MAX_TERMS: usize = 256;

/// Cap on the file we will parse at all. A file past this is treated as absent
/// and replaced, rather than read into memory.
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// One session's remembered terms, in insertion order, with the most recent
/// last. A `Vec` rather than a set because the eviction policy is by age.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct SessionState {
    /// Unix seconds of the last write touching this session. Drives pruning.
    updated: u64,
    /// Terms the agent has asked Travsr about (release 1).
    queried: Vec<String>,
    /// Terms the guard has already denied once (release 2).
    denied: Vec<String>,
}

/// The whole file: session id to state.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Store {
    sessions: BTreeMap<String, SessionState>,
}

/// Where the valve's state lives for a repo.
pub fn state_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".travsr").join("guard-sessions.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Terms are compared case-sensitively but trimmed, so `grep " Foo "` and
/// `get_callers(symbol="Foo")` are the same term. Case is kept because symbol
/// names are case-sensitive in every language the graph indexes.
fn normalise(term: &str) -> String {
    term.trim().to_string()
}

fn load(path: &Path) -> Store {
    // Size-check before reading: a corrupted or adversarially large file must
    // not be pulled into memory just to be discarded.
    match std::fs::metadata(path) {
        Ok(m) if m.len() <= MAX_FILE_BYTES => {}
        _ => return Store::default(),
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Write the store back, pruned. Best-effort: a failure here loses a release
/// hint, which release 2 then covers, so it is never worth reporting.
fn save(path: &Path, mut store: Store) {
    let cutoff = now_secs().saturating_sub(SESSION_TTL_SECS);
    store.sessions.retain(|_, s| s.updated >= cutoff);
    if store.sessions.len() > MAX_SESSIONS {
        // Keep the most recently touched. `BTreeMap` is ordered by session id,
        // not by time, so the threshold has to be computed rather than sliced.
        let mut stamps: Vec<u64> = store.sessions.values().map(|s| s.updated).collect();
        stamps.sort_unstable();
        let keep_from = stamps[stamps.len() - MAX_SESSIONS];
        let mut kept = 0usize;
        store.sessions.retain(|_, s| {
            let take = s.updated >= keep_from && kept < MAX_SESSIONS;
            if take {
                kept += 1;
            }
            take
        });
    }
    for s in store.sessions.values_mut() {
        for list in [&mut s.queried, &mut s.denied] {
            if list.len() > MAX_TERMS {
                list.drain(..list.len() - MAX_TERMS);
            }
        }
    }

    let Ok(text) = serde_json::to_string(&store) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    // Temp + rename, so a guard killed at the deadline never leaves a
    // half-written file for the next invocation to choke on. Two guards racing
    // is a last-writer-wins on one hint, which costs at most one extra redirect.
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, text).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Record that `session` asked Travsr about `term` (release 1).
pub fn record_query(repo_root: &Path, session: &str, term: &str) {
    let term = normalise(term);
    if session.is_empty() || term.is_empty() {
        return;
    }
    let path = state_path(repo_root);
    let mut store = load(&path);
    let entry = store.sessions.entry(session.to_string()).or_default();
    entry.updated = now_secs();
    if !entry.queried.iter().any(|t| t == &term) {
        entry.queried.push(term);
    }
    save(&path, store);
}

/// Whether `session` may run a plain text search for `term`, recording the
/// denial when it may not.
///
/// Returns `true` (released) when the agent has already asked Travsr about the
/// term, or when the guard has already denied this exact term in this session.
/// Otherwise records the denial and returns `false`, which is the one refusal
/// this pair will ever produce for that `(session, term)`.
///
/// A payload with no `session_id` releases unconditionally: without a session
/// there is nothing to scope the valve to, and a guard that cannot bound its
/// own denials must not make them.
pub fn release(repo_root: &Path, session: Option<&str>, term: &str) -> bool {
    let Some(session) = session.filter(|s| !s.is_empty()) else {
        return true;
    };
    let term = normalise(term);
    if term.is_empty() {
        return true;
    }
    let path = state_path(repo_root);
    let mut store = load(&path);
    let entry = store.sessions.entry(session.to_string()).or_default();
    entry.updated = now_secs();

    if entry.queried.iter().any(|t| t == &term) || entry.denied.iter().any(|t| t == &term) {
        // Touch the timestamp so an active session is not pruned mid-use.
        save(&path, store);
        return true;
    }
    entry.denied.push(term);
    save(&path, store);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".travsr")).unwrap();
        d
    }

    #[test]
    fn a_first_search_is_denied_and_the_second_is_released() {
        let d = repo();
        assert!(
            !release(d.path(), Some("s1"), "charge"),
            "the first search for a graph-answerable term is the one redirect"
        );
        assert!(
            release(d.path(), Some("s1"), "charge"),
            "a repeat in the same session must never be denied again"
        );
    }

    #[test]
    fn asking_travsr_first_releases_the_follow_up_search() {
        let d = repo();
        record_query(d.path(), "s1", "charge");
        assert!(
            release(d.path(), Some("s1"), "charge"),
            "an agent that already queried the graph must be allowed to verify"
        );
    }

    #[test]
    fn a_release_is_scoped_to_one_session() {
        let d = repo();
        record_query(d.path(), "s1", "charge");
        assert!(release(d.path(), Some("s1"), "charge"));
        assert!(
            !release(d.path(), Some("s2"), "charge"),
            "another session must still get its redirect"
        );
    }

    #[test]
    fn a_release_is_scoped_to_one_term() {
        let d = repo();
        record_query(d.path(), "s1", "charge");
        assert!(!release(d.path(), Some("s1"), "refund"));
    }

    #[test]
    fn a_payload_without_a_session_is_never_denied() {
        let d = repo();
        assert!(release(d.path(), None, "charge"));
        assert!(release(d.path(), Some(""), "charge"));
    }

    #[test]
    fn terms_are_trimmed_so_the_two_surfaces_agree() {
        let d = repo();
        record_query(d.path(), "s1", "  charge  ");
        assert!(release(d.path(), Some("s1"), "charge"));
    }

    #[test]
    fn a_corrupt_state_file_is_treated_as_empty_not_as_an_error() {
        let d = repo();
        std::fs::write(state_path(d.path()), "{not json").unwrap();
        // Falls back to "nothing remembered", which denies once then releases.
        assert!(!release(d.path(), Some("s1"), "charge"));
        assert!(release(d.path(), Some("s1"), "charge"));
    }

    #[test]
    fn an_unwritable_state_directory_still_releases_on_the_repeat() {
        // No `.travsr` at all: `save` creates it; if that fails the valve falls
        // back to denying once per invocation, which is still not a loop the
        // agent cannot leave, because the deny reason names the replacement.
        let d = tempfile::tempdir().unwrap();
        assert!(!release(d.path(), Some("s1"), "charge"));
        assert!(release(d.path(), Some("s1"), "charge"));
    }

    #[test]
    fn expired_sessions_are_pruned_on_write() {
        let d = repo();
        let path = state_path(d.path());
        let mut store = Store::default();
        store.sessions.insert(
            "ancient".into(),
            SessionState {
                updated: now_secs() - SESSION_TTL_SECS - 60,
                queried: vec!["charge".into()],
                denied: vec![],
            },
        );
        std::fs::write(&path, serde_json::to_string(&store).unwrap()).unwrap();

        // Any write prunes. Use a different session so the assertion is about
        // pruning rather than about the entry being rewritten.
        record_query(d.path(), "fresh", "refund");
        let after = load(&path);
        assert!(!after.sessions.contains_key("ancient"));
        assert!(after.sessions.contains_key("fresh"));
    }

    #[test]
    fn the_session_count_is_bounded() {
        let d = repo();
        for i in 0..(MAX_SESSIONS + 40) {
            record_query(d.path(), &format!("s{i}"), "charge");
        }
        let after = load(&state_path(d.path()));
        assert!(
            after.sessions.len() <= MAX_SESSIONS,
            "retained {} sessions, cap is {MAX_SESSIONS}",
            after.sessions.len()
        );
    }

    #[test]
    fn the_term_count_per_session_is_bounded() {
        let d = repo();
        for i in 0..(MAX_TERMS + 40) {
            record_query(d.path(), "s1", &format!("sym{i}"));
        }
        let after = load(&state_path(d.path()));
        let n = after.sessions["s1"].queried.len();
        assert!(n <= MAX_TERMS, "retained {n} terms, cap is {MAX_TERMS}");
        // Newest kept, oldest evicted.
        assert!(after.sessions["s1"]
            .queried
            .iter()
            .any(|t| t == &format!("sym{}", MAX_TERMS + 39)));
    }

    #[test]
    fn an_oversized_state_file_is_not_parsed() {
        let d = repo();
        let path = state_path(d.path());
        std::fs::write(&path, "x".repeat((MAX_FILE_BYTES + 1) as usize)).unwrap();
        assert!(load(&path).sessions.is_empty());
    }
}
