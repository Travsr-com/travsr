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

/// Register `repo_name → db_path` in the global registry.
///
/// Reads the existing registry (or starts fresh if missing), upserts the
/// entry, and writes back atomically. Parent directory is created if needed.
/// Non-fatal: callers should log and continue on error.
pub fn register(repo_name: &str, db_path: &Path) -> anyhow::Result<()> {
    let reg_path = registry_path();
    std::fs::create_dir_all(reg_path.parent().expect("registry path has parent"))
        .context("creating ~/.travsr directory")?;

    let mut repos = read_registry(&reg_path).unwrap_or_default();
    repos.insert(repo_name.to_string(), db_path.to_path_buf());
    write_registry_atomic(&reg_path, &repos)
}

/// Return all entries from the global registry.
///
/// Returns an empty map if the file does not exist (not an error).
pub fn all_repos() -> anyhow::Result<HashMap<String, PathBuf>> {
    Ok(read_registry(&registry_path()).unwrap_or_default())
}

fn read_registry(path: &Path) -> Option<HashMap<String, PathBuf>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let map: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let repos = map.get("repos")?.as_object()?;
    Some(
        repos
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), PathBuf::from(s))))
            .collect(),
    )
}

fn write_registry_atomic(path: &Path, repos: &HashMap<String, PathBuf>) -> anyhow::Result<()> {
    let json_repos: serde_json::Map<String, serde_json::Value> = repos
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                serde_json::Value::String(v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let serialized = serde_json::to_string_pretty(&serde_json::json!({ "repos": json_repos }))
        .context("serializing registry")?;

    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &serialized).context("writing registry tmp file")?;
    std::fs::rename(&tmp_path, path).context("renaming registry into place")?;
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
}
