use std::path::{Path, PathBuf};

/// Stable control-plane address derived from a repo root path.
///
/// Computed as `blake3(canonical_path)[..8]` encoded as 16 lowercase hex chars.
/// Both the daemon (server) and the hook/CLI (clients) call `for_repo` with the
/// same `repo_root` → they always agree on the socket name or pipe segment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ControlAddr {
    hex: String,
}

impl ControlAddr {
    /// Derive the control address for `repo_root`.
    ///
    /// Falls back to the raw path if canonicalization fails (e.g. the directory
    /// does not yet exist in tests). On Windows, the path is lowercased before
    /// hashing so that drive-letter casing differences produce the same address.
    pub fn for_repo(repo_root: &Path) -> Self {
        let canonical: PathBuf = repo_root
            .canonicalize()
            .unwrap_or_else(|_| repo_root.to_path_buf());

        let path_str = canonical.to_string_lossy();

        // On Windows, lowercase before hashing so C:\ and c:\ produce the same addr.
        #[cfg(windows)]
        let hash = blake3::hash(path_str.to_lowercase().as_bytes());
        #[cfg(not(windows))]
        let hash = blake3::hash(path_str.as_bytes());
        let hex: String =
            hash.as_bytes()[..8]
                .iter()
                .fold(String::with_capacity(16), |mut s, b| {
                    use std::fmt::Write as _;
                    let _ = write!(s, "{b:02x}");
                    s
                });
        Self { hex }
    }

    /// The 16-character hex string used as the socket name or pipe segment.
    pub fn as_hex(&self) -> &str {
        &self.hex
    }

    /// Unix domain socket path for this control address.
    ///
    /// Normally `<travsr_dir>/daemon-<hex>.sock`. Unix domain socket paths are
    /// capped by the platform `sun_path` array (104 bytes on macOS/BSD, 108 on
    /// Linux); a repo under a deep path pushes the in-`.travsr` socket over that
    /// limit and `bind()` fails with EINVAL ("path must be shorter than
    /// SUN_LEN") — travsr #592. When the in-repo path would not fit, fall back
    /// to a short per-user runtime directory
    /// (`$XDG_RUNTIME_DIR` / `$TMPDIR` / `/tmp` → `travsr-<uid>/daemon-<hex>.sock`).
    ///
    /// This function is pure and deterministic: the daemon (server) and every
    /// client pass the same `travsr_dir`, share the launching environment, and
    /// read the same owner uid, so they always resolve the same path. The daemon
    /// is responsible for creating and permission-verifying the fallback
    /// directory before it binds (see travsr-daemon).
    #[cfg(unix)]
    pub fn socket_path(&self, travsr_dir: &Path) -> PathBuf {
        let primary = travsr_dir.join(format!("daemon-{}.sock", self.hex));
        if unix_socket_path_fits(&primary) {
            return primary;
        }
        // Long-path fallback. Try each candidate base in order and pick the
        // first whose *resulting* socket path actually fits sun_path — a base
        // that is itself long (an unusual `$TMPDIR`/`$XDG_RUNTIME_DIR`) must not
        // silently defeat the fix by producing another over-long path. `/tmp`
        // is always the last resort and is short enough that the path always
        // fits, so this is guaranteed to return a fitting path.
        let leaf = format!("travsr-{}", owner_uid(travsr_dir));
        let sock = format!("daemon-{}.sock", self.hex);
        let mut last = PathBuf::new();
        for base in runtime_bases() {
            let candidate = base.join(&leaf).join(&sock);
            if unix_socket_path_fits(&candidate) {
                return candidate;
            }
            last = candidate;
        }
        last
    }

    /// Windows named pipe path: `\\.\pipe\travsr-<hex>`
    #[cfg(windows)]
    pub fn pipe_name(&self) -> String {
        format!(r"\\.\pipe\travsr-{}", self.hex)
    }
}

impl std::fmt::Display for ControlAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex)
    }
}

/// Size of the `sun_path` array on this platform. `bind()` rejects any path
/// whose byte length is `>= SUN_PATH_MAX` (std reports "path must be shorter
/// than SUN_LEN"), so a path fits iff its length is strictly less than this.
#[cfg(unix)]
const SUN_PATH_MAX: usize = if cfg!(any(target_os = "linux", target_os = "android")) {
    108
} else {
    104
};

#[cfg(unix)]
fn unix_socket_path_fits(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes().len() < SUN_PATH_MAX
}

/// Candidate base directories for the SUN_LEN fallback socket, in preference
/// order: `$XDG_RUNTIME_DIR` (Linux, user-private 0700), then `$TMPDIR` (macOS,
/// per-user 0700), then `/tmp` (always last, always short). The caller picks the
/// first whose resulting socket path fits. Daemon and clients resolve the same
/// list because the daemon inherits the launching CLI's environment.
#[cfg(unix)]
fn runtime_bases() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    for key in ["XDG_RUNTIME_DIR", "TMPDIR"] {
        if let Some(v) = std::env::var_os(key) {
            if !v.is_empty() {
                bases.push(PathBuf::from(v));
            }
        }
    }
    bases.push(PathBuf::from("/tmp"));
    bases
}

/// Owner uid of `path`, used as a per-user discriminator so distinct users
/// never collide under a shared `/tmp`. Falls back to 0 when the path cannot be
/// stat-ed (both daemon and clients degrade identically).
#[cfg(unix)]
fn owner_uid(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).map(|m| m.uid()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_path_produces_same_addr() {
        let tmp = tempfile::tempdir().unwrap();
        let a = ControlAddr::for_repo(tmp.path());
        let b = ControlAddr::for_repo(tmp.path());
        assert_eq!(a, b);
    }

    #[test]
    fn different_paths_produce_different_addrs() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        let a = ControlAddr::for_repo(tmp1.path());
        let b = ControlAddr::for_repo(tmp2.path());
        assert_ne!(a, b, "distinct repo roots must produce distinct addresses");
    }

    #[test]
    fn addr_is_sixteen_hex_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let addr = ControlAddr::for_repo(tmp.path());
        assert_eq!(addr.as_hex().len(), 16);
        assert!(
            addr.as_hex().chars().all(|c| c.is_ascii_hexdigit()),
            "address must be lowercase hex: {}",
            addr.as_hex()
        );
    }

    #[test]
    fn nonexistent_path_does_not_panic() {
        let path = std::path::Path::new("/nonexistent/repo/path-that-will-never-exist");
        let addr = ControlAddr::for_repo(path);
        assert_eq!(addr.as_hex().len(), 16);
    }

    #[cfg(unix)]
    #[test]
    fn socket_path_contains_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let addr = ControlAddr::for_repo(tmp.path());
        let sock = addr.socket_path(tmp.path());
        let name = sock.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("daemon-"));
        assert!(name.ends_with(".sock"));
        assert!(name.contains(addr.as_hex()));
    }

    // #592: a short repo path keeps the socket in-`.travsr`; a deep one falls
    // back to a short, per-user runtime path that fits the sun_path limit.

    #[cfg(unix)]
    #[test]
    fn short_path_uses_in_repo_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let addr = ControlAddr::for_repo(tmp.path());
        let sock = addr.socket_path(tmp.path());
        assert_eq!(
            sock.parent().unwrap(),
            tmp.path(),
            "short paths must stay in .travsr"
        );
    }

    #[cfg(unix)]
    #[test]
    fn long_path_falls_back_to_short_socket() {
        use std::os::unix::ffi::OsStrExt as _;
        // A .travsr dir deep enough that <dir>/daemon-<hex>.sock exceeds sun_path.
        let deep = PathBuf::from("/tmp")
            .join("a".repeat(60))
            .join("b".repeat(60))
            .join(".travsr");
        let addr = ControlAddr::for_repo(&deep);
        let primary = deep.join(format!("daemon-{}.sock", addr.as_hex()));
        assert!(
            primary.as_os_str().as_bytes().len() >= SUN_PATH_MAX,
            "precondition: in-repo socket path must be over the limit"
        );
        let sock = addr.socket_path(&deep);
        assert_ne!(
            sock.parent().unwrap(),
            deep.as_path(),
            "must not bind the over-long in-repo path"
        );
        assert!(
            sock.as_os_str().as_bytes().len() < SUN_PATH_MAX,
            "fallback socket must fit sun_path: {} ({} bytes)",
            sock.display(),
            sock.as_os_str().as_bytes().len()
        );
        let name = sock.file_name().unwrap().to_str().unwrap();
        assert!(name.contains(addr.as_hex()), "fallback keeps the repo hex");
    }

    #[cfg(unix)]
    #[test]
    fn daemon_and_client_agree_on_fallback() {
        // Determinism: two independent resolutions of the same deep travsr_dir
        // (the daemon and a client) must produce byte-identical socket paths.
        let deep = PathBuf::from("/tmp")
            .join("x".repeat(70))
            .join("y".repeat(70))
            .join(".travsr");
        let a = ControlAddr::for_repo(&deep).socket_path(&deep);
        let b = ControlAddr::for_repo(&deep).socket_path(&deep);
        assert_eq!(a, b);
    }
}
