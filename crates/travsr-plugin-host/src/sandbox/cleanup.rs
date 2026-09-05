//! #575: reversal of the Windows AppContainer grants the Phase B sandbox
//! leaves behind. `sandbox/windows.rs` grants an AppContainer profile SID
//! inheritable ACEs on the repo tree, the toolchain caches and `~/.travsr/bin`,
//! and traverse ACEs on every ancestor of the repo root; those grants persist
//! in each object's DACL and the profile persists in the user's registry hive
//! until something removes them.
//!
//! The profile SID is derived from a public unkeyed hash of the repo path
//! (`profile_name` below), so a leftover ACE is assumable by any local process
//! that can create an AppContainer with the same name, not merely disk residue.
//!
//! Everything here is platform-independent decision logic; the Win32 calls live
//! behind `execute` in `windows.rs` -> `windows/ffi.rs` (ADR-017 A2 invariant 1).

use std::path::{Path, PathBuf};

/// AppContainer profile name for `repo_root`, the identity every grant in
/// `sandbox/windows.rs` is written to.
pub fn profile_name(repo_root: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    repo_root.hash(&mut h);
    format!("travsr-{:016x}", h.finish())
}

/// What a deregistration must revoke: the profiles no remaining repo claims,
/// and the objects whose DACLs may carry an ACE for them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupPlan {
    pub profiles: Vec<String>,
    pub paths: Vec<PathBuf>,
}

impl CleanupPlan {
    pub fn is_noop(&self) -> bool {
        self.profiles.is_empty()
    }
}

/// Outcome of running a [`CleanupPlan`]. Failures are collected, never fatal:
/// a deregistration must not fail because an ACL could not be rewritten.
#[derive(Debug, Default)]
pub struct CleanupReport {
    pub revoked_paths: usize,
    pub deleted_profiles: usize,
    pub failures: Vec<String>,
}

impl CleanupReport {
    pub fn is_empty(&self) -> bool {
        self.revoked_paths == 0 && self.deleted_profiles == 0 && self.failures.is_empty()
    }
}

/// The registry key is verbatim-stripped at write time
/// (`travsr_store::registry::register`) while the `repo_root` the sandbox
/// hashes keeps the `\\?\` prefix on Windows, and the two spellings hash to
/// different profile names. Both are cleaned so a repo registered under either
/// form is covered.
fn verbatim_spelling(repo_root: &str) -> Option<String> {
    if repo_root.starts_with(r"\\?\") {
        return None;
    }
    if let Some(host_and_share) = repo_root.strip_prefix(r"\\") {
        return Some(format!(r"\\?\UNC\{host_and_share}"));
    }
    let is_drive_path = repo_root.as_bytes().get(1) == Some(&b':');
    is_drive_path.then(|| format!(r"\\?\{repo_root}"))
}

fn spellings(repo_root: &str) -> Vec<String> {
    let mut out = vec![repo_root.to_string()];
    out.extend(verbatim_spelling(repo_root));
    out
}

/// Ancestors of `repo_root`, mirroring `ffi::grant_ancestor_traverse`'s walk.
///
/// Splits on both separators rather than using `Path::ancestors`, because a
/// registry key is a Windows path even when this runs on Unix.
fn ancestors(repo_root: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut end = repo_root.trim_end_matches(['/', '\\']).len();
    while let Some(sep) = repo_root[..end].rfind(['/', '\\']) {
        let parent = &repo_root[..sep];
        if parent.is_empty() {
            out.push("/".to_string());
            break;
        }
        if parent.chars().all(|c| c == '/' || c == '\\') {
            break;
        }
        if let Some(drive) = parent.strip_suffix(':') {
            out.push(format!(r"{drive}:\"));
            break;
        }
        out.push(parent.to_string());
        end = sep;
    }
    out
}

/// Plans the cleanup for `removed_repo`, a registry key just dropped from
/// `~/.travsr/registry.json`. Yields an empty plan when a still-registered repo
/// in `remaining_repos` maps to the same profile, so a shared profile is never
/// revoked out from under a live repo.
pub fn plan_cleanup(
    removed_repo: &str,
    remaining_repos: &[String],
    toolchain_paths: &[PathBuf],
    travsr_bin: Option<&Path>,
) -> CleanupPlan {
    let still_claimed: Vec<String> = remaining_repos
        .iter()
        .flat_map(|r| spellings(r))
        .map(|s| profile_name(Path::new(&s)))
        .collect();
    let profiles: Vec<String> = spellings(removed_repo)
        .iter()
        .map(|s| profile_name(Path::new(s)))
        .filter(|p| !still_claimed.contains(p))
        .collect();
    if profiles.is_empty() {
        return CleanupPlan::default();
    }

    let mut paths: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !paths.contains(&p) {
            paths.push(p);
        }
    };
    push(PathBuf::from(removed_repo));
    for ancestor in ancestors(removed_repo) {
        push(PathBuf::from(ancestor));
    }
    for path in toolchain_paths {
        push(path.clone());
    }
    if let Some(bin) = travsr_bin {
        push(bin.to_path_buf());
    }
    CleanupPlan { profiles, paths }
}

/// Every out-of-repo path any language's Phase B analyzer may have been granted.
/// The languages that actually ran are not recorded, so the whole catalog is
/// enumerated; a path carrying no ACE for the profile is skipped cheaply.
#[cfg(windows)]
fn toolchain_grant_paths() -> Vec<PathBuf> {
    crate::phase_b::CATALOG
        .iter()
        .map(|entry| super::toolchain::toolchain_access(entry.language))
        .flat_map(|access| {
            access
                .read_paths
                .into_iter()
                .chain(access.write_paths)
                .chain(access.exec_paths)
        })
        .collect()
}

/// Revokes the AppContainer grants left by `removed_repo` and deletes its
/// profile. Idempotent: an absent path, an absent ACE and an absent profile all
/// count as nothing to do. No-op off Windows, where the sandbox grants nothing.
pub fn purge_repo_sandbox_grants(removed_repo: &str, remaining_repos: &[String]) -> CleanupReport {
    #[cfg(windows)]
    {
        let travsr_bin = dirs::home_dir().map(|h| h.join(".travsr").join("bin"));
        let plan = plan_cleanup(
            removed_repo,
            remaining_repos,
            &toolchain_grant_paths(),
            travsr_bin.as_deref(),
        );
        super::windows::execute_cleanup(&plan)
    }
    #[cfg(not(windows))]
    {
        let _ = (removed_repo, remaining_repos);
        CleanupReport::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = r"C:\Users\dev\src\travsr";

    fn plan(remaining: &[String]) -> CleanupPlan {
        plan_cleanup(REPO, remaining, &[], None)
    }

    #[test]
    fn profile_name_is_the_sandbox_derivation() {
        assert_eq!(
            profile_name(Path::new(REPO)),
            format!("travsr-{:016x}", {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                Path::new(REPO).hash(&mut h);
                h.finish()
            })
        );
    }

    #[test]
    fn plan_covers_repo_root_and_every_ancestor() {
        let p = plan(&[]);
        for expected in [
            REPO,
            r"C:\Users\dev\src",
            r"C:\Users\dev",
            r"C:\Users",
            r"C:\",
        ] {
            assert!(
                p.paths.contains(&PathBuf::from(expected)),
                "missing {expected} in {:?}",
                p.paths
            );
        }
    }

    #[test]
    fn plan_carries_both_verbatim_spellings_of_the_profile_name() {
        let p = plan(&[]);
        let clean = profile_name(Path::new(REPO));
        let verbatim = profile_name(Path::new(&format!(r"\\?\{REPO}")));
        assert_ne!(clean, verbatim, "the two spellings must hash differently");
        assert!(p.profiles.contains(&clean), "{:?}", p.profiles);
        assert!(p.profiles.contains(&verbatim), "{:?}", p.profiles);
    }

    #[test]
    fn plan_is_noop_when_another_registered_repo_maps_to_the_same_profile() {
        let p = plan(&[format!(r"\\?\{REPO}"), REPO.to_string()]);
        assert!(p.is_noop());
        assert!(p.paths.is_empty());
    }

    #[test]
    fn plan_keeps_profiles_no_remaining_repo_claims() {
        let p = plan(&[r"C:\Users\dev\src\other".to_string()]);
        assert_eq!(p.profiles.len(), 2);
    }

    #[test]
    fn plan_includes_toolchain_and_bin_paths_without_duplicates() {
        let gradle = PathBuf::from(r"C:\Users\dev\.gradle");
        let bin = PathBuf::from(r"C:\Users\dev\.travsr\bin");
        let p = plan_cleanup(
            REPO,
            &[],
            &[gradle.clone(), gradle.clone(), bin.clone()],
            Some(&bin),
        );
        assert_eq!(p.paths.iter().filter(|x| **x == gradle).count(), 1);
        assert_eq!(p.paths.iter().filter(|x| **x == bin).count(), 1);
    }

    #[test]
    fn plan_handles_unix_repo_roots() {
        let p = plan_cleanup("/home/dev/src/travsr", &[], &[], None);
        assert!(p.paths.contains(&PathBuf::from("/home/dev/src")));
        assert!(p.paths.contains(&PathBuf::from("/")));
        assert_eq!(p.profiles.len(), 1, "no verbatim spelling for a unix path");
    }

    #[test]
    fn unc_repo_roots_get_the_unc_verbatim_spelling() {
        assert_eq!(
            verbatim_spelling(r"\\srv\share\repo").as_deref(),
            Some(r"\\?\UNC\srv\share\repo")
        );
        assert_eq!(verbatim_spelling(r"\\?\C:\repo"), None);
    }

    #[test]
    fn ancestors_stop_at_the_unc_share_root() {
        assert_eq!(
            ancestors(r"\\srv\share\repo"),
            vec![r"\\srv\share".to_string(), r"\\srv".to_string()]
        );
    }
}
