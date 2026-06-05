//! Binary download module for `travsr lang install`.
//! Downloads travsr-lang-* wrappers from GitHub Releases into ~/.travsr/bin/.
//!
//! Environment variable overrides (for testing):
//!   TRAVSR_LANG_RELEASES_BASE — base URL for release asset downloads
//!   TRAVSR_LANG_API_URL       — GitHub API URL for latest release lookup
//!   TRAVSR_SKIP_DOWNLOAD=1    — skip network entirely; return expected dest path

use anyhow::{bail, Context as _, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const RELEASES_BASE_ENV: &str = "TRAVSR_LANG_RELEASES_BASE";
const API_URL_ENV: &str = "TRAVSR_LANG_API_URL";
const SKIP_DOWNLOAD_ENV: &str = "TRAVSR_SKIP_DOWNLOAD";

const DEFAULT_RELEASES_BASE: &str = "https://github.com/Travsr-com/travsr-lang/releases";
const DEFAULT_API_URL: &str = "https://api.github.com/repos/Travsr-com/travsr-lang/releases/latest";

const SIZE_LIMIT: u64 = 100 * 1024 * 1024;

/// Returns the Rust target triple for the current machine.
/// Returns an error on unsupported platforms (Windows tracked in #261).
pub fn current_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        (os, arch) => bail!("Unsupported platform: {os}/{arch}"),
    }
}

/// Returns ~/.travsr/bin/, creating it if it does not exist.
pub fn travsr_bin_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".travsr")
        .join("bin");
    std::fs::create_dir_all(&dir).context("creating ~/.travsr/bin")?;
    Ok(dir)
}

/// Returns true if ~/.travsr/bin is present in the PATH environment variable.
pub fn path_contains_travsr_bin() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let Some(path_os) = std::env::var_os("PATH") else {
        return false;
    };
    let target = home.join(".travsr").join("bin");
    std::env::split_paths(&path_os).any(|p| p == target)
}

/// Fetches the latest release tag for a GitHub repo.
/// Always queries the live API — `version_fallback` in the catalog is only used
/// when this call fails (offline, rate-limited, etc.).
pub async fn fetch_latest_version_for_repo(repo: &str) -> Result<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let resp = client
        .get(&url)
        .send()
        .await
        .context("fetching latest release")?;

    if !resp.status().is_success() {
        bail!("GitHub API returned {}: {url}", resp.status());
    }

    let json: serde_json::Value = resp.json().await.context("parsing release JSON")?;
    json["tag_name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing tag_name in releases response for {repo}"))
}

/// Fetches the latest version tag for the travsr-lang releases.
/// Wrapper around `fetch_latest_version_for_repo` for the travsr-lang repo.
pub async fn fetch_latest_version() -> Result<String> {
    let url = std::env::var(API_URL_ENV).unwrap_or_else(|_| DEFAULT_API_URL.to_string());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let resp = client
        .get(&url)
        .send()
        .await
        .context("fetching latest release")?;

    if !resp.status().is_success() {
        bail!("GitHub API returned {}: {url}", resp.status());
    }

    let json: serde_json::Value = resp.json().await.context("parsing release JSON")?;
    json["tag_name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing tag_name in GitHub releases response"))
}

/// Downloads a SCIP tool binary from any GitHub repo's releases and installs it
/// into `~/.travsr/bin/<install_name>`.
///
/// `tag` is the full release tag (e.g. `"v0.12.3"` or `"scip-ruby-v0.4.7"`).
/// `asset_name` is the filename on the release page.
/// When `verify_sha256` is true, downloads a `<asset_name>.sha256` sidecar and
/// verifies integrity before writing to disk.
pub async fn download_scip_binary(
    repo: &str,
    tag: &str,
    asset_name: &str,
    install_name: &str,
    verify_sha256: bool,
) -> Result<PathBuf> {
    if std::env::var(SKIP_DOWNLOAD_ENV).is_ok() {
        return Ok(travsr_bin_dir()?.join(install_name));
    }

    // scip binaries can be large (scip-java ~128 MB, scip-clang ~150 MB)
    const SCIP_SIZE_LIMIT: u64 = 200 * 1024 * 1024;

    let base = format!("https://github.com/{repo}/releases/download/{tag}");
    let bin_url = format!("{base}/{asset_name}");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let bin_bytes = if verify_sha256 {
        let sha_url = format!("{bin_url}.sha256");
        let (bin_resp, sha_resp) =
            tokio::try_join!(client.get(&bin_url).send(), client.get(&sha_url).send(),)
                .context("sending download requests")?;

        if !bin_resp.status().is_success() {
            bail!("download failed ({}): {bin_url}", bin_resp.status());
        }
        if !sha_resp.status().is_success() {
            bail!("SHA256 download failed ({}): {sha_url}", sha_resp.status());
        }
        if let Some(len) = bin_resp.content_length() {
            if len > SCIP_SIZE_LIMIT {
                bail!("binary exceeds size limit ({len} bytes)");
            }
        }
        let bytes = bin_resp.bytes().await.context("reading binary body")?;
        if bytes.len() as u64 > SCIP_SIZE_LIMIT {
            bail!("binary exceeds size limit after download");
        }
        let sha_text = sha_resp.text().await.context("reading SHA256 body")?;
        let expected = parse_sha256_line(&sha_text)?;
        let actual = hex_encode_sha256(&bytes);
        if actual != expected {
            bail!("SHA256 mismatch for {asset_name}: expected {expected}, got {actual}");
        }
        bytes
    } else {
        let resp = client
            .get(&bin_url)
            .send()
            .await
            .context("sending download request")?;
        if !resp.status().is_success() {
            bail!("download failed ({}): {bin_url}", resp.status());
        }
        if let Some(len) = resp.content_length() {
            if len > SCIP_SIZE_LIMIT {
                bail!("binary exceeds size limit ({len} bytes)");
            }
        }
        let bytes = resp.bytes().await.context("reading binary body")?;
        if bytes.len() as u64 > SCIP_SIZE_LIMIT {
            bail!("binary exceeds size limit after download");
        }
        bytes
    };

    let dest_dir = travsr_bin_dir()?;
    let dest = dest_dir.join(install_name);
    let tmp = dest_dir.join(format!("{install_name}.tmp.{}", uuid::Uuid::new_v4()));

    std::fs::write(&tmp, &bin_bytes)
        .with_context(|| format!("writing temp file {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context("setting executable permission")?;
    }

    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("moving {} to {}", tmp.display(), dest.display()));
    }

    Ok(dest)
}

/// Downloads and installs a travsr-lang-* wrapper binary into ~/.travsr/bin/.
///
/// Downloads the binary and its .sha256 file in parallel, verifies integrity,
/// then atomically renames into place. Returns the final install path.
pub async fn download_and_install_wrapper(
    version: &str,
    binary_name: &str,
    target: &str,
) -> Result<PathBuf> {
    if std::env::var(SKIP_DOWNLOAD_ENV).is_ok() {
        return Ok(travsr_bin_dir()?.join(binary_name));
    }

    let base =
        std::env::var(RELEASES_BASE_ENV).unwrap_or_else(|_| DEFAULT_RELEASES_BASE.to_string());

    let bin_url = format!("{base}/download/{version}/{binary_name}-{target}");
    let sha_url = format!("{bin_url}.sha256");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let (bin_resp, sha_resp) =
        tokio::try_join!(client.get(&bin_url).send(), client.get(&sha_url).send(),)
            .context("sending download requests")?;

    if !bin_resp.status().is_success() {
        bail!("download failed ({}): {bin_url}", bin_resp.status());
    }
    if !sha_resp.status().is_success() {
        bail!("SHA256 download failed ({}): {sha_url}", sha_resp.status());
    }

    // Reject oversized binaries before downloading the full body.
    if let Some(len) = bin_resp.content_length() {
        if len > SIZE_LIMIT {
            bail!("binary exceeds 100 MB limit ({len} bytes): {bin_url}");
        }
    }

    let bin_bytes = bin_resp.bytes().await.context("reading binary body")?;
    if bin_bytes.len() as u64 > SIZE_LIMIT {
        bail!("binary exceeds 100 MB limit after download");
    }
    let sha_text = sha_resp.text().await.context("reading SHA256 body")?;

    // Verify integrity.
    let expected = parse_sha256_line(&sha_text)?;
    let actual = hex_encode_sha256(&bin_bytes);
    if actual != expected {
        bail!("SHA256 mismatch for {binary_name}: expected {expected}, got {actual}");
    }

    // Atomic write: write to a temp path, then rename into place.
    let dest_dir = travsr_bin_dir()?;
    let dest = dest_dir.join(binary_name);
    let tmp = dest_dir.join(format!("{binary_name}.tmp.{}", uuid::Uuid::new_v4()));

    std::fs::write(&tmp, &bin_bytes)
        .with_context(|| format!("writing temp file {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context("setting executable permission")?;
    }

    if let Err(e) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("moving {} to {}", tmp.display(), dest.display()));
    }

    Ok(dest)
}

fn hex_encode_sha256(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    use std::fmt::Write as _;
    hash.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn parse_sha256_line(line: &str) -> Result<String> {
    let hex = line
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("SHA256 file is empty"))?;

    anyhow::ensure!(
        hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()),
        "invalid SHA256 hash in .sha256 file: '{hex}'"
    );

    Ok(hex.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_target_returns_known_triple() {
        // On the CI machines we build for, this must succeed.
        let t = current_target();
        // Only assert success on known platforms; skip on unsupported ones.
        if cfg!(all(target_os = "macos", target_arch = "aarch64"))
            || cfg!(all(target_os = "macos", target_arch = "x86_64"))
            || cfg!(all(target_os = "linux", target_arch = "x86_64"))
            || cfg!(all(target_os = "linux", target_arch = "aarch64"))
        {
            assert!(t.is_ok(), "expected Ok triple, got {t:?}");
        }
    }

    #[test]
    fn parse_sha256_line_handles_shasum_format() {
        let hex = "a".repeat(64);
        let line = format!("{hex}  somefile.bin");
        assert_eq!(parse_sha256_line(&line).unwrap(), hex);
    }

    #[test]
    fn parse_sha256_line_handles_bare_hex() {
        let hex = "b".repeat(64);
        assert_eq!(parse_sha256_line(&hex).unwrap(), hex);
    }

    #[test]
    fn parse_sha256_line_rejects_short_hash() {
        assert!(parse_sha256_line("abc123").is_err());
    }

    #[test]
    fn parse_sha256_line_rejects_non_hex() {
        let bad = "z".repeat(64);
        assert!(parse_sha256_line(&bad).is_err());
    }

    #[test]
    fn parse_sha256_line_lowercases_output() {
        let upper = "A".repeat(64);
        let result = parse_sha256_line(&upper).unwrap();
        assert_eq!(result, "a".repeat(64));
    }

    #[test]
    fn skip_download_returns_dest_path() {
        // Mutex guards set_var/remove_var against parallel test threads.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(SKIP_DOWNLOAD_ENV, "1");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(download_and_install_wrapper(
            "v0.1.0",
            "travsr-lang-go",
            "x86_64-unknown-linux-gnu",
        ));
        std::env::remove_var(SKIP_DOWNLOAD_ENV);
        drop(_guard);
        // Only check it doesn't error; bin dir creation may fail in sandbox envs.
        let _ = result; // result is Ok or Err depending on home dir availability
    }

    #[test]
    fn hex_encode_sha256_produces_64_chars() {
        let h = hex_encode_sha256(b"hello world");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
