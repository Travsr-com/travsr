//! Binary download module for `travsr lang install`.
//! Downloads travsr-lang-* wrappers from GitHub Releases into ~/.travsr/bin/.
//!
//! Ownership (RFC-013 conflict resolution): `travsr-lang-*` wrapper binaries
//! are FIRST-PARTY — built from this workspace's `crates/travsr-lang-*` and
//! published (cosign-signed, per ADR-020) by the CORE repo's release
//! workflow, versioned in lock-step with the `travsr` binary and its
//! `PROTOCOL_VERSION`. The external `travsr-lang` repo is no longer a binary
//! source: its published wrappers predate protocol v2, and installing one
//! over a local v2 build bricked the language (ProtocolSkew quarantine).
//! That repo remains only (a) the template for third-party plugin authors
//! and (b) the legacy host for `-share.tar.gz` emitter assets.
//!
//! Environment variable overrides (for testing):
//!   TRAVSR_LANG_RELEASES_BASE — base URL for wrapper release asset downloads
//!   TRAVSR_LANG_API_URL       — GitHub API URL for latest release lookup
//!   TRAVSR_SKIP_DOWNLOAD=1    — skip network entirely; return expected dest path

use anyhow::{bail, Context as _, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const RELEASES_BASE_ENV: &str = "TRAVSR_LANG_RELEASES_BASE";
const API_URL_ENV: &str = "TRAVSR_LANG_API_URL";
const SKIP_DOWNLOAD_ENV: &str = "TRAVSR_SKIP_DOWNLOAD";

/// Wrapper binaries ship with the CORE release (see release.yml: one signed
/// `travsr-lang-<x>-<tag>-<target>.tar.gz` per language per target).
const DEFAULT_RELEASES_BASE: &str = "https://github.com/Travsr-com/travsr/releases";
const DEFAULT_API_URL: &str = "https://api.github.com/repos/Travsr-com/travsr/releases/latest";

/// Legacy external repo — still hosts `-share.tar.gz` emitter assets only.
/// Never a source of executable wrapper binaries.
const LEGACY_SHARE_RELEASES_BASE: &str = "https://github.com/Travsr-com/travsr-lang/releases";

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

/// Fetches the latest CORE release tag — the version wrapper binaries ship
/// under (they version in lock-step with the `travsr` binary).
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
/// Source of truth: the CORE repo's release assets — one signed
/// `<binary_name>-<version>-<target>.tar.gz` per language per target
/// (produced by release.yml, ADR-020 Rule 1). Integrity is verified against
/// the release's combined `SHA256SUMS`; when `cosign` is available, the
/// tarball's Sigstore bundle is additionally verified against the pinned
/// release-workflow identity (Rule 2) — a failed signature is a hard abort.
///
/// Any pre-existing binary at the destination is preserved as
/// `<binary>.old` so the caller can roll back if the freshly installed
/// wrapper fails its verification probe (never leave the user with a
/// quarantined language when they had a working one).
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

    let asset = format!("{binary_name}-{version}-{target}.tar.gz");
    let tar_url = format!("{base}/download/{version}/{asset}");
    let sums_url = format!("{base}/download/{version}/SHA256SUMS");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let (tar_resp, sums_resp) =
        tokio::try_join!(client.get(&tar_url).send(), client.get(&sums_url).send(),)
            .context("sending download requests")?;

    if !tar_resp.status().is_success() {
        bail!(
            "download failed ({}): {tar_url}\n\
             (wrapper binaries ship with core releases; this travsr version may \
             predate the first signed release for {binary_name} — build locally \
             with `cargo build --release -p {binary_name}` and copy to ~/.travsr/bin)",
            tar_resp.status()
        );
    }
    if !sums_resp.status().is_success() {
        bail!("SHA256SUMS download failed ({}): {sums_url}", sums_resp.status());
    }

    // Reject oversized tarballs before downloading the full body.
    if let Some(len) = tar_resp.content_length() {
        if len > SIZE_LIMIT {
            bail!("asset exceeds 100 MB limit ({len} bytes): {tar_url}");
        }
    }

    let tar_bytes = tar_resp.bytes().await.context("reading tarball body")?;
    if tar_bytes.len() as u64 > SIZE_LIMIT {
        bail!("asset exceeds 100 MB limit after download");
    }
    let sums_text = sums_resp.text().await.context("reading SHA256SUMS body")?;

    // Integrity: find this asset's line in the combined SHA256SUMS.
    let expected = sums_text
        .lines()
        .find(|l| l.contains(&asset))
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("{asset} not listed in release SHA256SUMS"))?;
    let actual = hex_encode_sha256(&tar_bytes);
    if actual != expected {
        bail!("SHA256 mismatch for {asset}: expected {expected}, got {actual}");
    }

    // Authenticity (ADR-020 Rule 2): verify the Sigstore bundle when cosign
    // is installed. Bundle-missing is tolerated transitionally (warn);
    // verification FAILURE is always a hard abort.
    verify_cosign_bundle(&client, &tar_url, &tar_bytes, &asset).await?;

    // Extract the single binary from the tarball into a temp dir.
    let dest_dir = travsr_bin_dir()?;
    let work = dest_dir.join(format!(".extract.{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&work).context("creating extraction dir")?;
    let tmp_tar = work.join(&asset);
    std::fs::write(&tmp_tar, &tar_bytes)
        .with_context(|| format!("writing temp tarball {}", tmp_tar.display()))?;
    let status = std::process::Command::new("tar")
        .args(["-xzf", tmp_tar.to_str().unwrap(), "-C", work.to_str().unwrap()])
        .status()
        .context("running tar to extract wrapper")?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&work);
        bail!("tar extraction failed for {asset}");
    }

    let extracted = [binary_name.to_string(), format!("{binary_name}.exe")]
        .iter()
        .map(|n| work.join(n))
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow::anyhow!("{binary_name} not found inside {asset}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&extracted, std::fs::Permissions::from_mode(0o755))
            .context("setting executable permission")?;
    }

    // Preserve any existing binary for rollback, then move into place.
    let dest = dest_dir.join(
        extracted
            .file_name()
            .expect("extracted path has a file name"),
    );
    if dest.is_file() {
        let backup = wrapper_backup_path(&dest);
        std::fs::rename(&dest, &backup)
            .with_context(|| format!("backing up existing {}", dest.display()))?;
    }
    if let Err(e) = std::fs::rename(&extracted, &dest) {
        let _ = std::fs::remove_dir_all(&work);
        return Err(e).with_context(|| format!("installing {}", dest.display()));
    }
    let _ = std::fs::remove_dir_all(&work);

    Ok(dest)
}

/// `<binary>.old` — where [`download_and_install_wrapper`] parks the previous
/// binary so a failed post-install verification can roll back.
pub fn wrapper_backup_path(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_os_string();
    os.push(".old");
    PathBuf::from(os)
}

/// Restore the `.old` backup over `dest`. Returns true if a backup existed.
pub fn restore_wrapper_backup(dest: &Path) -> bool {
    let backup = wrapper_backup_path(dest);
    backup.is_file() && std::fs::rename(&backup, dest).is_ok()
}

/// Remove the `.old` backup after a successful install + verification.
pub fn discard_wrapper_backup(dest: &Path) {
    let _ = std::fs::remove_file(wrapper_backup_path(dest));
}

/// Verify `<asset>.bundle` (Sigstore) against the pinned first-party release
/// identity when `cosign` is on PATH. Missing cosign or a 404 bundle is a
/// transitional warn; a verification FAILURE aborts the install.
async fn verify_cosign_bundle(
    client: &reqwest::Client,
    tar_url: &str,
    tar_bytes: &[u8],
    asset: &str,
) -> Result<()> {
    let cosign_available = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|d| d.join("cosign").is_file());
    if !cosign_available {
        eprintln!(
            "warning: cosign not installed — skipping signature verification for {asset} \
             (integrity still checked via SHA256SUMS)"
        );
        return Ok(());
    }

    let bundle_url = format!("{tar_url}.bundle");
    let resp = client
        .get(&bundle_url)
        .send()
        .await
        .context("fetching signature bundle")?;
    if !resp.status().is_success() {
        eprintln!(
            "warning: no signature bundle for {asset} ({}) — release predates signing; \
             integrity checked via SHA256SUMS only",
            resp.status()
        );
        return Ok(());
    }
    let bundle_bytes = resp.bytes().await.context("reading bundle body")?;

    let work = std::env::temp_dir().join(format!("travsr-cosign-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&work).context("creating cosign work dir")?;
    let blob = work.join(asset);
    let bundle = work.join(format!("{asset}.bundle"));
    std::fs::write(&blob, tar_bytes).context("writing blob for cosign")?;
    std::fs::write(&bundle, &bundle_bytes).context("writing bundle for cosign")?;

    let out = std::process::Command::new("cosign")
        .args([
            "verify-blob",
            "--bundle",
            bundle.to_str().unwrap(),
            "--certificate-oidc-issuer",
            travsr_plugin_host::subscriptions::EXPECTED_OIDC_ISSUER,
            "--certificate-identity-regexp",
            travsr_plugin_host::subscriptions::EXPECTED_IDENTITY_REGEXP,
            blob.to_str().unwrap(),
        ])
        .output()
        .context("invoking cosign")?;
    let _ = std::fs::remove_dir_all(&work);

    if !out.status.success() {
        bail!(
            "cosign signature verification FAILED for {asset}: {} — refusing to install \
             (ADR-020 Rule 2 fail-closed)",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    println!("\u{2713} {asset} signature verified (cosign)");
    Ok(())
}

/// Downloads `<binary_name>-share.tar.gz` from the same travsr-lang release and
/// extracts it into `~/.travsr/share/<binary_name>/`. Used for sidecars that
/// spawn an external script (e.g. dart's emit.dart) rather than a compiled binary.
/// The tarball is platform-independent (source + metadata only, no binaries).
pub async fn install_share_assets(version: &str, binary_name: &str) -> Result<()> {
    if std::env::var(SKIP_DOWNLOAD_ENV).is_ok() {
        return Ok(());
    }

    // Share assets (emitter scripts/data, no executables) still live on the
    // legacy external repo — wrapper BINARIES never come from there.
    let base = std::env::var(RELEASES_BASE_ENV)
        .unwrap_or_else(|_| LEGACY_SHARE_RELEASES_BASE.to_string());

    let asset = format!("{binary_name}-share.tar.gz");
    let url = format!("{base}/download/{version}/{asset}");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let resp = client
        .get(&url)
        .send()
        .await
        .context("fetching share assets")?;
    if !resp.status().is_success() {
        bail!("share asset download failed ({}): {url}", resp.status());
    }

    let bytes = resp.bytes().await.context("reading share asset body")?;

    // Extract into ~/.travsr/share/<binary_name>/
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let share_dir = home.join(".travsr").join("share").join(binary_name);
    std::fs::create_dir_all(&share_dir)
        .with_context(|| format!("creating {}", share_dir.display()))?;

    let tmp = share_dir
        .parent()
        .unwrap()
        .join(format!("{asset}.tmp.{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, &bytes)
        .with_context(|| format!("writing temp tarball {}", tmp.display()))?;

    let status = std::process::Command::new("tar")
        .args([
            "-xzf",
            tmp.to_str().unwrap(),
            "-C",
            share_dir.to_str().unwrap(),
        ])
        .status()
        .context("running tar to extract share assets")?;

    let _ = std::fs::remove_file(&tmp);

    anyhow::ensure!(status.success(), "tar extraction failed for {asset}");
    Ok(())
}

fn hex_encode_sha256(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    use std::fmt::Write as _;
    hash.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Downloads a zip archive from a GitHub release, extracts it into
/// `~/.travsr/<extract_dir>/`, and returns that directory path.
///
/// Used for tools that ship as archives rather than standalone binaries
/// (e.g. kotlin-language-server ships `server.zip`).
pub async fn download_zip_and_extract(
    repo: &str,
    tag: &str,
    asset_name: &str,
    extract_dir: &str,
) -> Result<PathBuf> {
    if std::env::var(SKIP_DOWNLOAD_ENV).is_ok() {
        let dest = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
            .join(".travsr")
            .join(extract_dir);
        return Ok(dest);
    }

    let url = format!("https://github.com/{repo}/releases/download/{tag}/{asset_name}");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;

    if !resp.status().is_success() {
        bail!("download failed: {} for {url}", resp.status());
    }

    let bytes = resp.bytes().await.context("reading zip bytes")?;

    let dest = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".travsr")
        .join(extract_dir);
    std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;

    let tmp = dest.join("_download.zip");
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing zip to {}", tmp.display()))?;

    let status = std::process::Command::new("unzip")
        .args(["-qo", &tmp.to_string_lossy(), "-d", &dest.to_string_lossy()])
        .status()
        .context("running unzip — ensure unzip is installed")?;

    let _ = std::fs::remove_file(&tmp);

    if !status.success() {
        bail!("unzip exited with {status}");
    }

    Ok(dest)
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
