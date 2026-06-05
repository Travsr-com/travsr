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
        let hex: String = hash.as_bytes()[..8]
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

    /// Unix domain socket path: `<travsr_dir>/daemon-<hex>.sock`
    #[cfg(unix)]
    pub fn socket_path(&self, travsr_dir: &Path) -> PathBuf {
        travsr_dir.join(format!("daemon-{}.sock", self.hex))
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
            "address must be lowercase hex: {}", addr.as_hex()
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
}
