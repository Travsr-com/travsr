//! Global registry — maps repo names to their graph.db paths.
//!
//! Lives at `~/.travsr/registry.json`. Writes are atomic: JSON is written
//! to a sibling `.registry.json.tmp` then renamed into place so a crash
//! during write never produces a corrupt file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Path to the global registry file: `~/.travsr/registry.json`.
pub fn registry_path() -> PathBuf {
    home_dir().join(".travsr").join("registry.json")
}

/// Strip the Windows extended-length / verbatim path prefix (`\\?\`) that
/// `std::fs::canonicalize` prepends on Windows, so registry keys and displayed
/// paths read as normal paths (UX-018). Verbatim UNC (`\\?\UNC\server\share`)
/// is rewritten back to its `\\server\share` form. No-op on any path without the
/// prefix, so it is safe to call unconditionally on every platform (a POSIX path
/// never carries it).
pub fn strip_verbatim_prefix(p: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = p.strip_prefix(r"\\?\") {
        Cow::Borrowed(rest)
    } else {
        Cow::Borrowed(p)
    }
}

/// Human-facing display name for a registry entry (UX-018): the repo-root
/// basename, with the Windows `\\?\` verbatim prefix stripped. Registry *keys*
/// stay the full canonical repo-root path — unique across `~/proj` vs
/// `/home/user/proj`, which is what dedup relies on — so this is a display-only
/// derivation. #705 screenshotted the current repo listed by its full prefixed
/// path sitting next to a basename-named entry; deriving every Name from the
/// basename makes the column read consistently regardless of how old the entry
/// is. Falls back to the cleaned full path when the key has no final component
/// (e.g. a bare drive root), so it never returns an empty string.
///
/// Splits on both `/` and `\` explicitly rather than via `Path::file_name`,
/// because a registry key is a Windows path even when this runs on Unix (the
/// registry file is portable and the tests exercise Windows paths on every OS),
/// and `Path::file_name` only treats `/` as a separator off Windows.
pub fn display_name(key: &str) -> String {
    let cleaned = strip_verbatim_prefix(key).into_owned();
    cleaned
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or(cleaned)
}

/// Outcome of [`unregister_resolving`].
#[derive(Debug, PartialEq, Eq)]
pub enum UnregisterResult {
    /// The entry was found and removed.
    Removed,
    /// No entry matched by key, cleaned key, or basename.
    NotFound,
    /// A basename matched more than one entry; the caller must disambiguate
    /// with a full path. Carries the colliding entries' cleaned full paths.
    Ambiguous(Vec<String>),
}

/// A registry row: where this repo's `graph.db` lives, plus what the registry
/// knows about the repo ever having been indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoEntry {
    pub db_path: PathBuf,
    index_history: IndexHistory,
}

/// What the registry records about a repo's indexing history (#454). The db file
/// cannot answer this on its own: it is created by `SqliteStore::open` at the
/// *start* of `init`, and deleting it takes any evidence stored inside it along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexHistory {
    /// Row written before the registry tracked index completions, so whether the
    /// repo was ever indexed is not knowable from the registry.
    Untracked,
    NeverIndexed,
    IndexedAt(u64),
}

/// Index state of one registry entry, as reported by `travsr repos` and the MCP
/// `repos_list` tool (#454).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    /// The graph.db is on disk.
    Indexed,
    /// An index completed at some point and the graph.db has since gone.
    IndexMissing,
    /// Registered, with no index run ever completed for it.
    NotIndexed,
    /// The graph.db is gone and this entry predates index tracking, so the
    /// registry cannot say which of the two cases above it is.
    Unknown,
}

impl IndexStatus {
    /// Machine-facing tag: the CLI `--json` `status` field and the MCP TSV.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indexed => "indexed",
            Self::IndexMissing => "index_missing",
            Self::NotIndexed => "not_indexed",
            Self::Unknown => "unknown",
        }
    }

    /// Text for the CLI table's `Exists` column, keeping its `yes` / `no (...)`
    /// shape.
    pub fn label(self) -> &'static str {
        match self {
            Self::Indexed => "yes",
            Self::IndexMissing => "no (index deleted)",
            Self::NotIndexed => "no (never indexed)",
            Self::Unknown => "no (unknown)",
        }
    }
}

impl RepoEntry {
    pub fn index_status(&self) -> IndexStatus {
        if self.db_path.exists() {
            return IndexStatus::Indexed;
        }
        match self.index_history {
            IndexHistory::IndexedAt(_) => IndexStatus::IndexMissing,
            IndexHistory::NeverIndexed => IndexStatus::NotIndexed,
            IndexHistory::Untracked => IndexStatus::Unknown,
        }
    }
}

/// SEC (#507, Windows): mirror the Unix owner-only restriction via `icacls`,
/// following the daemon's graph.db pattern. `/inheritance:r` strips all
/// inherited ACEs; `/grant:r` re-grants to the current user only —
/// `(OI)(CI)F` on directories so children inherit the restriction, plain `F`
/// on files. `USERDOMAIN\USERNAME` avoids ambiguity on domain-joined
/// machines. Best-effort with loud warnings, matching the Unix branches.
#[cfg(windows)]
fn restrict_to_owner_windows(path: &Path) {
    let Some(path_str) = path.to_str() else {
        tracing::warn!(
            path = %path.display(),
            "path is not valid UTF-8, skipping icacls permission restriction"
        );
        return;
    };
    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        tracing::warn!(
            path = %path.display(),
            "USERNAME env var not set, skipping permission restriction on Windows"
        );
        return;
    }
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let account = if domain.is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    };
    let grant = if path.is_dir() {
        format!("{account}:(OI)(CI)F")
    } else {
        format!("{account}:(F)")
    };
    let status = std::process::Command::new("icacls")
        .args([path_str, "/inheritance:r", "/grant:r", &grant])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(
            path = %path.display(),
            exit_code = ?s.code(),
            "icacls failed to restrict permissions, may be readable by other users on this machine"
        ),
        Err(e) => tracing::warn!(
            path = %path.display(),
            err = %e,
            "icacls not available, permissions not restricted on Windows"
        ),
    }
}

/// Register `repo_name → db_path` in the global registry.
///
/// Reads the existing registry (or starts fresh if missing), upserts the
/// entry, and writes back atomically. Parent directory is created if needed.
/// Non-fatal: callers should log and continue on error.
pub fn register(repo_name: &str, db_path: &Path) -> anyhow::Result<()> {
    let (reg_path, _lock) = lock_registry()?;

    // UX-018: normalize away the Windows `\\?\` verbatim prefix so the registry
    // key and stored db path are clean, consistent paths (never a mix of a
    // prefixed and a bare entry for the same repo).
    let repo_name = strip_verbatim_prefix(repo_name).into_owned();
    let db_path = PathBuf::from(strip_verbatim_prefix(&db_path.to_string_lossy()).into_owned());

    let mut repos = read_registry(&reg_path).unwrap_or_default();
    // #454: a re-`init` must not erase the record that this repo was indexed
    // before, so the upsert below carries the existing row's history over.
    let index_history = repos
        .iter()
        .find(|(k, _)| strip_verbatim_prefix(k).as_ref() == repo_name.as_str())
        .map_or(IndexHistory::NeverIndexed, |(_, entry)| entry.index_history);
    // UX-018: drop any pre-existing entry that only differs from the normalized
    // key by the Windows `\\?\` verbatim prefix, then re-insert the clean one.
    // Without this migration a repo first registered by an older build under its
    // prefixed key would keep *both* the old prefixed key and the new clean key
    // and show up twice in `repos`. Removing by normalized form and re-inserting
    // collapses the pair to a single clean entry on the next `init`.
    repos.retain(|k, _| strip_verbatim_prefix(k).as_ref() != repo_name.as_str());
    repos.insert(
        repo_name,
        RepoEntry {
            db_path,
            index_history,
        },
    );
    write_registry_atomic(&reg_path, &repos)?;

    // Explicit unlock is not needed; the lock file drops at end of scope.
    Ok(())
}

/// Record that an index run completed for `repo_name` (#454), so a later listing
/// can tell an index that was deleted apart from one that was never built.
///
/// No-op when the repo is not registered. Non-fatal: callers should log and
/// continue on error.
pub fn mark_indexed(repo_name: &str) -> anyhow::Result<()> {
    let (reg_path, _lock) = lock_registry()?;
    let key = strip_verbatim_prefix(repo_name).into_owned();
    let Some(mut repos) = read_registry(&reg_path) else {
        return Ok(());
    };
    let Some(entry) = repos.get_mut(&key) else {
        return Ok(());
    };
    entry.index_history = IndexHistory::IndexedAt(now_secs());
    write_registry_atomic(&reg_path, &repos)
}

/// Create `~/.travsr` owner-only, take the exclusive `registry.lock`, and return
/// the registry path together with the held lock.
///
/// M1: serializes concurrent registry writes. The atomic rename protects against
/// crash-corruption but not against concurrent read-modify-write by two
/// processes, so every writer goes through here.
fn lock_registry() -> anyhow::Result<(PathBuf, std::fs::File)> {
    let reg_path = registry_path();
    let travsr_home = reg_path
        .parent()
        .context("registry path has no parent directory")?;
    std::fs::create_dir_all(travsr_home).context("creating ~/.travsr directory")?;

    // SEC: ~/.travsr/ and registry.json contain repo paths — restrict to owner only.
    // A silent failure here would leave the directory world-readable, so warn loudly.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(travsr_home, std::fs::Permissions::from_mode(0o700))
        {
            tracing::warn!(
                path = %travsr_home.display(),
                err = %e,
                "failed to restrict ~/.travsr/ permissions to 0700, directory may be world-readable"
            );
        }
    }
    // SEC (#507): Windows equivalent of the 0o700 restriction above.
    #[cfg(windows)]
    restrict_to_owner_windows(travsr_home);

    let lock_path = travsr_home.join("registry.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&lock_path)
        .context("opening registry.lock")?;
    fs2::FileExt::lock_exclusive(&lock_file).context("acquiring registry.lock")?;
    Ok((reg_path, lock_file))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Return all entries from the global registry.
///
/// Returns an empty map if the file does not exist (not an error).
pub fn all_repos() -> anyhow::Result<HashMap<String, PathBuf>> {
    Ok(all_entries()?
        .into_iter()
        .map(|(name, entry)| (name, entry.db_path))
        .collect())
}

/// Return all entries with their index history, which is what a repo listing
/// needs to report an [`IndexStatus`] rather than a bare "the db file is there"
/// boolean.
pub fn all_entries() -> anyhow::Result<HashMap<String, RepoEntry>> {
    Ok(read_registry(&registry_path()).unwrap_or_default())
}

/// Remove a single repo entry by name.
///
/// Returns `Ok(true)` if the entry existed and was removed, `Ok(false)` if it
/// was absent or the registry file does not exist. Non-fatal.
pub fn unregister(repo_name: &str) -> anyhow::Result<bool> {
    let reg_path = registry_path();
    let mut repos = match read_registry(&reg_path) {
        Some(r) => r,
        None => return Ok(false), // no registry → nothing to remove
    };
    if repos.remove(repo_name).is_none() {
        return Ok(false);
    }
    write_registry_atomic(&reg_path, &repos)?;
    Ok(true)
}

/// Remove a repo entry by full registry key **or** its display basename (UX-018).
///
/// Because `repos` now shows the basename as the Name (not the full path), a user
/// or the VS Code webview removes by whatever the list showed them. Resolution is
/// tried most-specific first, so an exact path can always target one entry: the
/// exact registry key, then the verbatim-stripped key (the cleaned path the list
/// displays), then the display basename — the last only when it matches exactly
/// one entry. A basename that collides across several repos returns
/// [`UnregisterResult::Ambiguous`] with their full paths so the caller can re-run
/// with a path. Non-fatal.
pub fn unregister_resolving(name: &str) -> anyhow::Result<UnregisterResult> {
    let reg_path = registry_path();
    let mut repos = match read_registry(&reg_path) {
        Some(r) => r,
        None => return Ok(UnregisterResult::NotFound),
    };

    // 1. Exact key.
    if repos.remove(name).is_some() {
        write_registry_atomic(&reg_path, &repos)?;
        return Ok(UnregisterResult::Removed);
    }

    // 2. Cleaned (verbatim-stripped) key — what the list actually printed.
    let target = strip_verbatim_prefix(name).into_owned();
    let clean_matches: Vec<String> = repos
        .keys()
        .filter(|k| strip_verbatim_prefix(k).as_ref() == target.as_str())
        .cloned()
        .collect();
    if clean_matches.len() == 1 {
        repos.remove(&clean_matches[0]);
        write_registry_atomic(&reg_path, &repos)?;
        return Ok(UnregisterResult::Removed);
    }

    // 3. Display basename — unambiguous only.
    let base_matches: Vec<String> = repos
        .keys()
        .filter(|k| display_name(k) == name)
        .cloned()
        .collect();
    match base_matches.len() {
        0 => Ok(UnregisterResult::NotFound),
        1 => {
            repos.remove(&base_matches[0]);
            write_registry_atomic(&reg_path, &repos)?;
            Ok(UnregisterResult::Removed)
        }
        _ => Ok(UnregisterResult::Ambiguous(
            base_matches
                .iter()
                .map(|k| strip_verbatim_prefix(k).into_owned())
                .collect(),
        )),
    }
}

/// Prune stale entries — those whose `graph.db` no longer exists on disk.
///
/// This is the cleanup for registry pollution: `travsr init` runs in throwaway
/// directories (e.g. tests in `/tmp`) leave entries behind that never get
/// removed when the directory is deleted. Returns the names removed (sorted);
/// empty when the registry is absent or already clean. Only rewrites the file
/// when something was actually pruned.
///
/// O(n) over registry entries, one `stat` per entry.
pub fn prune() -> anyhow::Result<Vec<String>> {
    let reg_path = registry_path();
    let repos = match read_registry(&reg_path) {
        Some(r) => r,
        None => return Ok(Vec::new()),
    };

    let mut removed: Vec<String> = Vec::new();
    let mut kept: HashMap<String, RepoEntry> = HashMap::new();
    for (name, entry) in repos {
        if entry.db_path.exists() {
            kept.insert(name, entry);
        } else {
            removed.push(name);
        }
    }

    if !removed.is_empty() {
        write_registry_atomic(&reg_path, &kept)?;
        removed.sort();
    }
    Ok(removed)
}

fn read_registry(path: &Path) -> Option<HashMap<String, RepoEntry>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let map: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let repos = map.get("repos")?.as_object()?;
    Some(
        repos
            .iter()
            .filter_map(|(k, v)| parse_entry(v).map(|entry| (k.clone(), entry)))
            .collect(),
    )
}

/// A row is either the pre-#454 bare db-path string or `{ db_path, indexed_at? }`.
/// The shape is the only record of whether the writer tracked index completions,
/// so [`entry_to_json`] preserves it rather than upgrading rows in place.
fn parse_entry(value: &serde_json::Value) -> Option<RepoEntry> {
    if let Some(db_path) = value.as_str() {
        return Some(RepoEntry {
            db_path: PathBuf::from(db_path),
            index_history: IndexHistory::Untracked,
        });
    }
    let row = value.as_object()?;
    Some(RepoEntry {
        db_path: PathBuf::from(row.get("db_path")?.as_str()?),
        index_history: match row.get("indexed_at").and_then(serde_json::Value::as_u64) {
            Some(secs) => IndexHistory::IndexedAt(secs),
            None => IndexHistory::NeverIndexed,
        },
    })
}

fn entry_to_json(entry: &RepoEntry) -> serde_json::Value {
    let db_path = entry.db_path.to_string_lossy().into_owned();
    match entry.index_history {
        IndexHistory::Untracked => serde_json::Value::String(db_path),
        IndexHistory::NeverIndexed => serde_json::json!({ "db_path": db_path }),
        IndexHistory::IndexedAt(secs) => {
            serde_json::json!({ "db_path": db_path, "indexed_at": secs })
        }
    }
}

fn write_registry_atomic(path: &Path, repos: &HashMap<String, RepoEntry>) -> anyhow::Result<()> {
    let json_repos: serde_json::Map<String, serde_json::Value> = repos
        .iter()
        .map(|(k, entry)| (k.clone(), entry_to_json(entry)))
        .collect();
    let serialized = serde_json::to_string_pretty(&serde_json::json!({ "repos": json_repos }))
        .context("serializing registry")?;

    let tmp_path = path.with_extension("json.tmp");

    // SEC-2: create the temp file with 0o600 from birth to avoid a TOCTOU window
    // where another local process could open(2) the world-readable umask version
    // between fs::write and the post-rename chmod. On Unix we open via
    // OpenOptions with .mode(0o600); on other platforms we fall back to fs::write.
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .context("opening registry tmp file with 0600")?;
        f.write_all(serialized.as_bytes())
            .context("writing registry tmp file")?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp_path, &serialized).context("writing registry tmp file")?;

    std::fs::rename(&tmp_path, path).context("renaming registry into place")?;

    // Belt-and-suspenders for non-Unix and exotic filesystems that may not
    // preserve permissions across rename. A warn here is fatal-shaped — the
    // file is on disk by now and a failed chmod leaves it readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(
                path = %path.display(),
                err = %e,
                "failed to restrict registry.json permissions to 0600, file may be world-readable"
            );
        }
    }
    // SEC (#507): Windows equivalent of the 0o600 restriction above — the
    // registry lists every indexed repo path, same sensitivity as graph.db.
    #[cfg(windows)]
    restrict_to_owner_windows(path);
    Ok(())
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Rust tests run in parallel; HOME is process-global. Serialize all
    // registry tests through this lock so HOME mutations don't race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_temp_home(f: impl FnOnce(&Path)) {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        f(tmp.path());
        std::env::remove_var("HOME");
    }

    #[test]
    fn strip_verbatim_prefix_handles_all_forms() {
        // Plain drive-letter verbatim path → prefix removed.
        assert_eq!(
            strip_verbatim_prefix(r"\\?\D:\com.travsr\travsr"),
            r"D:\com.travsr\travsr"
        );
        // Verbatim UNC → rewritten to the normal `\\server\share` form.
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\server\share\proj"),
            r"\\server\share\proj"
        );
        // No prefix (including POSIX paths) → returned untouched.
        assert_eq!(
            strip_verbatim_prefix(r"D:\already\clean"),
            r"D:\already\clean"
        );
        assert_eq!(strip_verbatim_prefix("/home/user/proj"), "/home/user/proj");
    }

    #[test]
    fn register_creates_registry_on_first_call() {
        with_temp_home(|home| {
            let db = home.join("proj/.travsr/graph.db");
            register("my-repo", &db).unwrap();
            assert_eq!(all_repos().unwrap().get("my-repo"), Some(&db));
        });
    }

    #[test]
    fn register_upserts_existing_entry() {
        with_temp_home(|home| {
            let db1 = home.join("old/graph.db");
            let db2 = home.join("new/graph.db");
            register("repo", &db1).unwrap();
            register("repo", &db2).unwrap();
            assert_eq!(all_repos().unwrap().get("repo"), Some(&db2));
        });
    }

    #[test]
    fn register_preserves_other_entries() {
        with_temp_home(|home| {
            register("alpha", &home.join("a/graph.db")).unwrap();
            register("beta", &home.join("b/graph.db")).unwrap();
            assert_eq!(all_repos().unwrap().len(), 2);
        });
    }

    #[test]
    fn all_repos_returns_empty_when_no_registry() {
        with_temp_home(|_| {
            assert!(all_repos().unwrap().is_empty());
        });
    }

    #[test]
    fn unregister_removes_existing_and_reports_absent() {
        with_temp_home(|home| {
            register("alpha", &home.join("a/graph.db")).unwrap();
            register("beta", &home.join("b/graph.db")).unwrap();
            assert!(unregister("alpha").unwrap(), "existing entry → true");
            assert!(!unregister("alpha").unwrap(), "already gone → false");
            assert!(!unregister("never").unwrap(), "never present → false");
            let repos = all_repos().unwrap();
            assert_eq!(repos.len(), 1);
            assert!(repos.contains_key("beta"), "other entries preserved");
        });
    }

    #[test]
    fn unregister_returns_false_when_no_registry() {
        with_temp_home(|_| {
            assert!(!unregister("anything").unwrap());
        });
    }

    #[test]
    fn prune_drops_stale_keeps_live() {
        with_temp_home(|home| {
            // "live" gets a real graph.db on disk; "stale" points at a deleted dir.
            let live_db = home.join("live/.travsr/graph.db");
            std::fs::create_dir_all(live_db.parent().unwrap()).unwrap();
            std::fs::write(&live_db, b"x").unwrap();
            register("live", &live_db).unwrap();
            register("stale", &home.join("gone/.travsr/graph.db")).unwrap();

            let removed = prune().unwrap();
            assert_eq!(removed, vec!["stale".to_string()]);
            let repos = all_repos().unwrap();
            assert_eq!(repos.len(), 1);
            assert!(repos.contains_key("live"));
        });
    }

    #[test]
    fn prune_empty_when_no_registry_or_all_live() {
        with_temp_home(|home| {
            assert!(prune().unwrap().is_empty(), "no registry → empty");
            let live_db = home.join("live/.travsr/graph.db");
            std::fs::create_dir_all(live_db.parent().unwrap()).unwrap();
            std::fs::write(&live_db, b"x").unwrap();
            register("live", &live_db).unwrap();
            assert!(prune().unwrap().is_empty(), "all live → nothing removed");
            assert_eq!(all_repos().unwrap().len(), 1);
        });
    }

    #[test]
    fn display_name_is_the_basename_prefix_stripped() {
        // UX-018: the Name column derives from the repo-root basename with the
        // Windows verbatim prefix removed — the exact case #705 screenshotted.
        assert_eq!(display_name(r"\\?\D:\com.travsr\travsr"), "travsr");
        assert_eq!(display_name(r"C:\Users\me\demoProj"), "demoProj");
        assert_eq!(display_name("/home/user/proj"), "proj");
        // No final component (a bare root) → cleaned full path, never empty.
        assert!(!display_name(r"\\?\D:\").is_empty());
    }

    #[test]
    fn register_migrates_a_pre_existing_verbatim_prefixed_key() {
        // UX-018: an entry stored by an older build under its `\\?\` key must
        // collapse to the single clean key on the next registration, not sit
        // beside a duplicate.
        with_temp_home(|home| {
            let db = home.join("proj/.travsr/graph.db");
            // Simulate the legacy prefixed key already on disk.
            register(r"\\?\D:\proj", &db).unwrap();
            // A fresh registration under the normalized key.
            register(r"D:\proj", &db).unwrap();
            let repos = all_repos().unwrap();
            assert_eq!(repos.len(), 1, "prefixed + clean must collapse to one");
            assert!(repos.contains_key(r"D:\proj"));
            assert!(!repos.contains_key(r"\\?\D:\proj"));
        });
    }

    #[test]
    fn unregister_resolving_matches_exact_key_and_basename() {
        with_temp_home(|home| {
            register(r"D:\com.travsr\travsr", &home.join("t/graph.db")).unwrap();
            // Removal by basename (what the list now shows) resolves to the key.
            assert_eq!(
                unregister_resolving("travsr").unwrap(),
                UnregisterResult::Removed
            );
            assert!(all_repos().unwrap().is_empty());

            // Removal by full key still works.
            register(r"D:\com.travsr\travsr", &home.join("t/graph.db")).unwrap();
            assert_eq!(
                unregister_resolving(r"D:\com.travsr\travsr").unwrap(),
                UnregisterResult::Removed
            );

            // Unknown name → NotFound (no registry mutation).
            assert_eq!(
                unregister_resolving("nope").unwrap(),
                UnregisterResult::NotFound
            );
        });
    }

    #[test]
    fn unregister_resolving_reports_ambiguous_basename() {
        with_temp_home(|home| {
            // Two different repos share the basename `travsr`.
            register(r"D:\a\travsr", &home.join("a/graph.db")).unwrap();
            register(r"D:\b\travsr", &home.join("b/graph.db")).unwrap();
            match unregister_resolving("travsr").unwrap() {
                UnregisterResult::Ambiguous(paths) => {
                    assert_eq!(paths.len(), 2, "both colliding paths reported");
                }
                other => panic!("expected Ambiguous, got {other:?}"),
            }
            // Nothing removed on an ambiguous match.
            assert_eq!(all_repos().unwrap().len(), 2);
        });
    }

    /// Write a registry in the pre-#454 shape: values are bare db-path strings.
    fn write_legacy_registry(pairs: &[(&str, &Path)]) {
        let reg = registry_path();
        std::fs::create_dir_all(reg.parent().unwrap()).unwrap();
        let repos: serde_json::Map<String, serde_json::Value> = pairs
            .iter()
            .map(|(k, p)| {
                (
                    (*k).to_string(),
                    serde_json::Value::String(p.to_string_lossy().into_owned()),
                )
            })
            .collect();
        std::fs::write(
            &reg,
            serde_json::to_string_pretty(&serde_json::json!({ "repos": repos })).unwrap(),
        )
        .unwrap();
    }

    fn touch_db(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn index_status_separates_never_indexed_present_and_deleted() {
        with_temp_home(|home| {
            let present = home.join("present/.travsr/graph.db");
            touch_db(&present);
            register("present", &present).unwrap();
            mark_indexed("present").unwrap();

            let never = home.join("never/.travsr/graph.db");
            register("never", &never).unwrap();

            let deleted = home.join("deleted/.travsr/graph.db");
            touch_db(&deleted);
            register("deleted", &deleted).unwrap();
            mark_indexed("deleted").unwrap();
            std::fs::remove_file(&deleted).unwrap();

            let entries = all_entries().unwrap();
            assert_eq!(
                entries["present"].index_status(),
                IndexStatus::Indexed,
                "db on disk means the index is there"
            );
            assert_eq!(
                entries["never"].index_status(),
                IndexStatus::NotIndexed,
                "registered but no index ever completed"
            );
            assert_eq!(
                entries["deleted"].index_status(),
                IndexStatus::IndexMissing,
                "index completed, then the db was removed"
            );
        });
    }

    #[test]
    fn legacy_row_reports_unknown_only_when_its_db_is_gone() {
        with_temp_home(|home| {
            let present = home.join("present/.travsr/graph.db");
            touch_db(&present);
            write_legacy_registry(&[
                ("present", &present),
                ("gone", &home.join("gone/.travsr/graph.db")),
            ]);

            let entries = all_entries().unwrap();
            assert_eq!(entries["present"].index_status(), IndexStatus::Indexed);
            assert_eq!(
                entries["gone"].index_status(),
                IndexStatus::Unknown,
                "a row predating index tracking must not claim 'never indexed'"
            );
        });
    }

    #[test]
    fn register_preserves_a_recorded_index_across_reregistration() {
        with_temp_home(|home| {
            let db = home.join("proj/.travsr/graph.db");
            register("proj", &db).unwrap();
            mark_indexed("proj").unwrap();
            register("proj", &db).unwrap();

            assert_eq!(
                all_entries().unwrap()["proj"].index_status(),
                IndexStatus::IndexMissing,
                "re-init must not erase the record that this repo was indexed"
            );
        });
    }

    #[test]
    fn rewrites_do_not_invent_index_history_for_legacy_rows() {
        with_temp_home(|home| {
            let live = home.join("live/.travsr/graph.db");
            touch_db(&live);
            write_legacy_registry(&[
                ("live", &live),
                ("stale", &home.join("stale/.travsr/graph.db")),
            ]);
            prune().unwrap();

            let raw = std::fs::read_to_string(registry_path()).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert!(
                parsed["repos"]["live"].is_string(),
                "a pruning rewrite must leave an untracked row untracked, got: {raw}"
            );
        });
    }

    #[test]
    fn mark_indexed_is_a_no_op_for_an_unregistered_repo() {
        with_temp_home(|_| {
            mark_indexed("never-registered").unwrap();
            assert!(all_entries().unwrap().is_empty());
        });
    }
}
