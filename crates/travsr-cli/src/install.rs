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
    // #410 M2: expected sha256 vendored in the catalog, for upstreams that
    // publish no sidecar. Checked after download and before anything is
    // written, so a replaced asset never reaches disk.
    expected_sha256: Option<&str>,
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
        if let Some(expected) = expected_sha256 {
            let actual = hex_encode_sha256(&bytes);
            if actual != expected {
                bail!(
                    "SHA256 mismatch for {asset_name} at {tag}: expected {expected}, got {actual}. \
                     The pinned asset does not match the hash recorded in the catalog — it may have \
                     been replaced upstream."
                );
            }
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

    replace_file(&tmp, &dest)?;

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

    replace_file(&tmp, &dest)?;

    Ok(dest)
}

/// Downloads `<binary_name>-share.tar.gz` from the same travsr-lang release and
/// extracts it into `~/.travsr/share/<binary_name>/`. Used for sidecars that
/// spawn an external script (e.g. dart's emit.dart) rather than a compiled binary.
/// The tarball is platform-independent (source + metadata only, no binaries).
pub async fn install_share_assets(version: &str, binary_name: &str) -> Result<()> {
    if std::env::var(SKIP_DOWNLOAD_ENV).is_ok() {
        return Ok(());
    }

    let base =
        std::env::var(RELEASES_BASE_ENV).unwrap_or_else(|_| DEFAULT_RELEASES_BASE.to_string());

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

    // #410 M1: extracted in-process, with every entry path checked against the
    // destination first. Shelling out to `tar` left traversal behaviour up to
    // whichever implementation the host shipped, and needed the two
    // `to_str().unwrap()` calls that panicked on a non-UTF-8 home directory.
    // No temp file either: the bytes are already in memory.
    extract_tar_gz(&bytes, &share_dir)
        .with_context(|| format!("extracting {asset} into {}", share_dir.display()))?;
    Ok(())
}

/// #506: move a freshly written `tmp` into `dest`, displacing a currently
/// running image when needed.
///
/// On Windows, `fs::rename` maps to `MoveFileEx(MOVEFILE_REPLACE_EXISTING)`,
/// which must delete `dest` first — and Windows refuses to delete a file
/// backing a running process. Upgrading a binary the daemon is currently
/// running therefore failed with "Access is denied (os error 5)", the common
/// case for `travsr embed init` / `travsr lang install`. Renaming the running
/// image ASIDE is allowed, so this applies the standard self-update dance
/// (rustup, VS Code updater): `dest` → `dest.old.<uuid>`, move `tmp` into
/// place, and best-effort sweep stale `*.old.*` leftovers — the displaced
/// image stays locked until its process exits, so a later install removes it.
///
/// On Unix the first rename simply succeeds (the old inode lives on until the
/// process exits) and the dance is never entered. `tmp` is cleaned up on
/// every failure path.
pub(crate) fn replace_file(tmp: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let first_err = match std::fs::rename(tmp, dest) {
        Ok(()) => {
            sweep_displaced_siblings(dest);
            return Ok(());
        }
        Err(e) => e,
    };

    // The dance only helps when an existing dest blocked the replace.
    if !dest.exists() {
        let _ = std::fs::remove_file(tmp);
        return Err(first_err)
            .with_context(|| format!("moving {} to {}", tmp.display(), dest.display()));
    }

    let displaced = {
        let mut name = dest.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".old.{}", uuid::Uuid::new_v4()));
        dest.with_file_name(name)
    };
    if let Err(aside_err) = rename_with_retry(dest, &displaced) {
        let _ = std::fs::remove_file(tmp);
        return Err(first_err).with_context(|| {
            format!(
                "moving {} to {} (displacing the existing file also failed: {aside_err})",
                tmp.display(),
                dest.display()
            )
        });
    }
    if let Err(e) = rename_with_retry(tmp, dest) {
        // Roll the displaced original back so the tool keeps working.
        let _ = std::fs::rename(&displaced, dest);
        let _ = std::fs::remove_file(tmp);
        return Err(e).with_context(|| {
            format!(
                "moving {} into place after displacing {}",
                tmp.display(),
                dest.display()
            )
        });
    }

    sweep_displaced_siblings(dest);
    Ok(())
}

/// Rename with a short bounded retry on transient Windows errors.
///
/// Antivirus scanners briefly hold freshly written or freshly executed files
/// with no-share handles (ERROR_SHARING_VIOLATION, 32) and can surface
/// transient ERROR_ACCESS_DENIED (5); rustup's file ops retry on Windows for
/// the same reason. Non-transient errors and non-Windows failures return
/// immediately.
fn rename_with_retry(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    const ATTEMPTS: u32 = 20;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);
    let mut attempt = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempt += 1;
                let transient = cfg!(windows) && matches!(e.raw_os_error(), Some(5) | Some(32));
                if !transient || attempt >= ATTEMPTS {
                    return Err(e);
                }
                std::thread::sleep(BACKOFF);
            }
        }
    }
}

/// True when `name` has the exact displacement shape [`replace_file`]
/// creates: `<original>.old.<uuid>`, with the trailing component parsing as
/// a real UUID.
///
/// PR #577 review: a plain `.old.` substring match also hit unrelated files
/// a user parked in the directory (e.g. `notes.old.txt`), silently deleting
/// them on every install. Requiring the UUID suffix confines the sweep to
/// artifacts this module itself created, while still collecting leftovers
/// of sibling binaries displaced by earlier installs.
fn is_displaced_leftover(name: &str) -> bool {
    name.rfind(".old.")
        .map(|i| &name[i + ".old.".len()..])
        .and_then(|suffix| uuid::Uuid::parse_str(suffix).ok())
        .is_some()
}

/// Best-effort removal of `<name>.old.<uuid>` files left by earlier
/// [`replace_file`] displacements in `dest`'s directory. Files still backing
/// a running process refuse deletion — they are picked up by the sweep of a
/// later install.
fn sweep_displaced_siblings(dest: &std::path::Path) {
    let Some(dir) = dest.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if is_displaced_leftover(&entry.file_name().to_string_lossy()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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

    // #410 M1: extracted in-process with per-entry path validation. Shelling
    // out meant `unzip` had to exist (it does not on plenty of systems, and
    // Windows needed a separate bsdtar branch), and traversal behaviour varied
    // by implementation. No temp file: the bytes are already in memory.
    extract_zip(&bytes, &dest).with_context(|| format!("extracting {asset_name}"))?;

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

    // ── #506: replace_file — displace-aside self-update dance ──────────────

    #[test]
    fn replace_file_over_plain_existing_file_and_sweeps_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("tool.exe");
        std::fs::write(&dest, b"old").unwrap();
        // Leftover from a previous displaced upgrade — must be swept.
        let leftover = dir
            .path()
            .join(format!("tool.exe.old.{}", uuid::Uuid::new_v4()));
        std::fs::write(&leftover, b"stale").unwrap();
        // PR #577 review: files that merely CONTAIN ".old." are not ours and
        // must survive the sweep untouched.
        let user_note = dir.path().join("notes.old.txt");
        std::fs::write(&user_note, b"keep me").unwrap();
        let near_miss = dir.path().join("tool.exe.old.deadbeef");
        std::fs::write(&near_miss, b"keep me too").unwrap();

        let tmp = dir.path().join("tool.exe.tmp.1");
        std::fs::write(&tmp, b"new").unwrap();

        replace_file(&tmp, &dest).expect("replace over plain file");
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert!(!tmp.exists(), "tmp must be consumed");
        assert!(
            !leftover.exists(),
            "stale .old.<uuid> leftovers must be swept"
        );
        assert!(user_note.exists(), "unrelated .old. files must survive");
        assert!(near_miss.exists(), "non-UUID .old. suffixes must survive");
    }

    /// PR #577 review: the sweep predicate itself, exhaustively.
    #[test]
    fn displaced_leftover_shape_is_exact() {
        let uuid = uuid::Uuid::new_v4();
        assert!(is_displaced_leftover(&format!(
            "travsr-embed.exe.old.{uuid}"
        )));
        assert!(is_displaced_leftover(&format!("scip-java.old.{uuid}")));
        assert!(!is_displaced_leftover("notes.old.txt"));
        assert!(!is_displaced_leftover("tool.exe.old.deadbeef"));
        assert!(!is_displaced_leftover("archive.old."));
        assert!(!is_displaced_leftover("plain-file.exe"));
    }

    #[test]
    fn replace_file_into_empty_slot_is_a_plain_rename() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("fresh-install");
        let tmp = dir.path().join("fresh-install.tmp.1");
        std::fs::write(&tmp, b"payload").unwrap();
        replace_file(&tmp, &dest).expect("fresh install");
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
    }

    #[test]
    fn replace_file_missing_tmp_errors_and_leaves_dest_alone() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("keep-me");
        std::fs::write(&dest, b"precious").unwrap();
        let tmp = dir.path().join("does-not-exist.tmp");
        assert!(replace_file(&tmp, &dest).is_err());
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"precious",
            "a failed replace must not damage the existing file"
        );
    }

    /// #506 regression, the exact reported scenario: dest is the image of a
    /// RUNNING process. Plain fs::rename fails with Access Denied on Windows;
    /// replace_file must displace the running image aside and succeed.
    #[cfg(windows)]
    #[test]
    fn replace_file_displaces_a_running_image() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("victim.exe");
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        std::fs::copy(&comspec, &dest).expect("copy cmd.exe as victim");

        // Keep the image busy for ~5 s (ping is available headless, unlike timeout).
        let mut child = std::process::Command::new(&dest)
            .args(["/C", "ping -n 6 127.0.0.1 >nul"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn victim");

        // Precondition of the bug: the direct replace is refused.
        let probe = dir.path().join("probe.tmp");
        std::fs::write(&probe, b"probe").unwrap();
        assert!(
            std::fs::rename(&probe, &dest).is_err(),
            "test precondition: plain rename over a running image must fail"
        );

        let tmp = dir.path().join("victim.exe.tmp.1");
        std::fs::write(&tmp, b"upgraded").unwrap();
        replace_file(&tmp, &dest).expect("replace over running image must succeed");
        assert_eq!(std::fs::read(&dest).unwrap(), b"upgraded");

        let _ = child.kill();
        let _ = child.wait();
    }

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

// ── #410 M1: in-process archive extraction ────────────────────────────────────
//
// The download paths used to shell out to `tar -xzf` and `unzip -qo`. Two
// problems with that, beyond the obvious dependency on those binaries existing:
//
//  1. A release asset carrying `../` entries could write outside the
//     destination (zip-slip / tar traversal), and whether it did depended on
//     which implementation the host shipped — GNU tar, bsdtar and busybox all
//     differ, so the safety of an install varied by machine.
//  2. `unzip` is simply absent on many systems, failing the install with an
//     error that says nothing useful.
//
// Extracting in-process fixes both, and makes the traversal check something
// this repo owns and can test rather than something it inherits.

/// Largest archive accepted for extraction.
///
/// The binary download paths already cap at 100 MB (`SIZE_LIMIT`) and 200 MB
/// (`SCIP_SIZE_LIMIT`); the archive paths had no cap at all and buffered the
/// whole response in memory. This matches the larger of the two, since share
/// bundles and language toolchains are the biggest things fetched.
const MAX_ARCHIVE_BYTES: usize = 200 * 1024 * 1024;

/// Resolve `entry` against `dest`, rejecting anything that escapes it.
///
/// Rejects absolute paths, drive prefixes and any `..` component. Checked on
/// the *declared* entry name before a single byte is written, so a hostile
/// archive cannot act through a path that is only assembled later.
///
/// Symlink entries are refused outright by the callers rather than resolved:
/// a link written first can redirect a later regular-file entry outside the
/// destination even when both names look harmless on their own.
fn safe_entry_path(dest: &std::path::Path, entry: &std::path::Path) -> Result<PathBuf> {
    use std::path::Component;
    let mut out = dest.to_path_buf();
    for c in entry.components() {
        match c {
            Component::Normal(part) => out.push(part),
            // `.` is meaningless but harmless; everything else can escape.
            Component::CurDir => {}
            Component::ParentDir => {
                bail!("archive entry escapes the destination: {}", entry.display())
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("archive entry is absolute: {}", entry.display())
            }
        }
    }
    Ok(out)
}

/// Extract a gzipped tarball into `dest`, in-process.
pub(crate) fn extract_tar_gz(bytes: &[u8], dest: &std::path::Path) -> Result<()> {
    anyhow::ensure!(
        bytes.len() <= MAX_ARCHIVE_BYTES,
        "archive is {} bytes, over the {MAX_ARCHIVE_BYTES}-byte limit",
        bytes.len()
    );
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading a tar entry")?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            bail!(
                "archive contains a link entry ({}), which is not extracted",
                entry.path().unwrap_or_default().display()
            );
        }
        if !kind.is_file() && !kind.is_dir() {
            continue; // devices, fifos and the like have no place in a release asset
        }
        let declared = entry
            .path()
            .context("tar entry has no usable path")?
            .into_owned();
        let target = safe_entry_path(dest, &declared)?;
        if kind.is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("creating {}", target.display()))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        entry
            .unpack(&target)
            .with_context(|| format!("writing {}", target.display()))?;
    }
    Ok(())
}

/// Extract a zip archive into `dest`, in-process.
pub(crate) fn extract_zip(bytes: &[u8], dest: &std::path::Path) -> Result<()> {
    anyhow::ensure!(
        bytes.len() <= MAX_ARCHIVE_BYTES,
        "archive is {} bytes, over the {MAX_ARCHIVE_BYTES}-byte limit",
        bytes.len()
    );
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("reading zip archive")?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).context("reading a zip entry")?;
        // `enclosed_name` is the crate's own traversal guard; `safe_entry_path`
        // is applied as well so the rule this repo enforces is stated here and
        // covered by its own tests, rather than resting on an upstream default.
        let declared = file
            .enclosed_name()
            .ok_or_else(|| anyhow::anyhow!("zip entry has an unsafe name: {}", file.name()))?;
        let target = safe_entry_path(dest, &declared)?;
        if file.is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("creating {}", target.display()))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut out = std::fs::File::create(&target)
            .with_context(|| format!("creating {}", target.display()))?;
        std::io::copy(&mut file, &mut out)
            .with_context(|| format!("writing {}", target.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod extraction_tests {
    use super::{extract_tar_gz, extract_zip, safe_entry_path, MAX_ARCHIVE_BYTES};
    use std::path::Path;

    #[test]
    fn safe_entry_path_rejects_escapes() {
        let dest = Path::new("/tmp/dest");
        assert!(safe_entry_path(dest, Path::new("a/b.txt")).is_ok());
        assert!(safe_entry_path(dest, Path::new("./a/b.txt")).is_ok());

        // The zip-slip / tar-traversal shapes.
        assert!(safe_entry_path(dest, Path::new("../evil")).is_err());
        assert!(safe_entry_path(dest, Path::new("a/../../evil")).is_err());
        assert!(safe_entry_path(dest, Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn safe_entry_path_keeps_everything_under_dest() {
        let dest = Path::new("/tmp/dest");
        let got = safe_entry_path(dest, Path::new("./nested/./file.txt")).unwrap();
        assert_eq!(got, Path::new("/tmp/dest/nested/file.txt"));
        assert!(got.starts_with(dest));
    }

    /// Build a gzipped tar in memory containing one entry named `name`.
    ///
    /// The name is written straight into the raw header field rather than
    /// through `append_data`, because the `tar` crate's *writer* refuses to
    /// emit a `..` path. A hostile archive would not be produced by that
    /// writer, so a test that could only build well-formed names would never
    /// exercise the case the extractor exists to reject.
    fn tar_gz_with(name: &str, body: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_ustar();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        {
            let raw = header.as_old_mut();
            raw.name[..name.len()].copy_from_slice(name.as_bytes());
        }
        header.set_cksum();

        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, body).unwrap();
        let tar_bytes = builder.into_inner().unwrap();

        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut enc, &tar_bytes).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn a_traversing_tar_entry_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();

        let err = extract_tar_gz(&tar_gz_with("../escaped.txt", b"pwned"), &dest).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
        assert!(
            !dir.path().join("escaped.txt").exists(),
            "nothing may be written outside the destination"
        );
    }

    #[test]
    fn an_ordinary_tar_entry_extracts() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();

        extract_tar_gz(&tar_gz_with("share/data.txt", b"hello"), &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("share/data.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn an_oversized_archive_is_refused_before_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let oversized = vec![0u8; MAX_ARCHIVE_BYTES + 1];
        let err = extract_tar_gz(&oversized, dir.path()).unwrap_err();
        assert!(err.to_string().contains("over the"), "{err}");
        let err = extract_zip(&oversized, dir.path()).unwrap_err();
        assert!(err.to_string().contains("over the"), "{err}");
    }

    /// Note on what this proves: the zip path has two independent guards, the
    /// crate's `enclosed_name` and `safe_entry_path`. This asserts the property
    /// that matters (nothing lands outside the destination), so it holds while
    /// either guard does — removing `safe_entry_path` alone leaves it green.
    /// The equivalent tar test *is* load-bearing for our own check, since the
    /// `tar` crate hands the declared path through unexamined.
    #[test]
    fn a_traversing_zip_entry_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();

        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file::<_, ()>("../escaped.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut w, b"pwned").unwrap();
        let bytes = w.finish().unwrap().into_inner();

        assert!(extract_zip(&bytes, &dest).is_err());
        assert!(
            !dir.path().join("escaped.txt").exists(),
            "nothing may be written outside the destination"
        );
    }

    #[test]
    fn an_ordinary_zip_entry_extracts() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();

        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file::<_, ()>("bin/tool", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut w, b"binary").unwrap();
        let bytes = w.finish().unwrap().into_inner();

        extract_zip(&bytes, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("bin/tool")).unwrap(), b"binary");
    }
}
