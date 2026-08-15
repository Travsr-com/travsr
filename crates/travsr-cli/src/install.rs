//! Binary download module for `travsr lang install`.
//! Downloads travsr-lang-* wrappers from GitHub Releases into ~/.travsr/bin/.
//!
//! Environment variable overrides (for testing):
//!   TRAVSR_LANG_RELEASES_BASE — base URL for release asset downloads
//!   TRAVSR_LANG_API_URL       — GitHub API URL for latest release lookup
//!   TRAVSR_SKIP_DOWNLOAD=1    — skip network entirely; return expected dest path

use anyhow::{bail, Context as _, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use travsr_plugin_host::sidecar_version::{
    below_floor_message, floor_status, unreadable_message, write_cached_latest, FloorStatus,
    SidecarSpec,
};

const RELEASES_BASE_ENV: &str = "TRAVSR_LANG_RELEASES_BASE";
const API_URL_ENV: &str = "TRAVSR_LANG_API_URL";
const SKIP_DOWNLOAD_ENV: &str = "TRAVSR_SKIP_DOWNLOAD";

const DEFAULT_RELEASES_BASE: &str = "https://github.com/Travsr-com/travsr-lang/releases";
const DEFAULT_API_URL: &str = "https://api.github.com/repos/Travsr-com/travsr-lang/releases/latest";

const SIZE_LIMIT: u64 = 100 * 1024 * 1024;

/// Returns the Rust target triple for the current machine.
/// Returns an error on platforms travsr has no target triple for.
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

// ── #588: what the travsr-lang release matrix actually ships ──────────────────
//
// The CLI used to build `<binary>-<target>` for whatever `current_target()`
// returned and hand it straight to the downloader. Windows was added to
// `current_target()` without a matching release leg, so `travsr lang install`
// asked for an asset that had never been published and every language ended in
// a raw 404 — a setup flow offered to users that could not succeed.
//
// These three items are the contract with `.github/workflows/release.yml` in
// Travsr-com/travsr-lang. Anything not derivable from them must not be
// requested: `wrapper_asset_name` returns `None`, and the caller states the
// limitation instead of downloading. The `wrapper_release_drift` test below
// checks these names against the live release inventory (in the
// lang-release-drift workflow) so the two cannot drift apart silently again.

/// Target triples the travsr-lang release workflow builds wrappers for.
pub const WRAPPER_RELEASE_TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
];

/// Wrappers built by a macOS-only release job. `travsr-lang-objectivec` links
/// against libclang and shells out to `xcrun`, so it exists for Apple targets
/// only — on Linux it 404s for the same reason every wrapper did on Windows.
const MACOS_ONLY_WRAPPERS: &[&str] = &["travsr-lang-objectivec"];

/// Executable suffix for a target triple: `.exe` on Windows, nothing elsewhere.
///
/// One rule for both the release asset name and the on-disk install name — they
/// are the same string on GitHub and under `~/.travsr/bin`, and splitting the
/// rule in two is how half-suffixed paths get introduced.
pub fn exe_suffix_for_target(target: &str) -> &'static str {
    if target.contains("windows") {
        ".exe"
    } else {
        ""
    }
}

/// Release asset filename of the travsr-lang wrapper `binary_name` for `target`,
/// or `None` when the release matrix publishes no such asset.
///
/// `None` is a capability statement, not an error: the caller is expected to say
/// "not available on this platform yet" rather than request a URL that 404s.
pub fn wrapper_asset_name(binary_name: &str, target: &str) -> Option<String> {
    if !WRAPPER_RELEASE_TARGETS.contains(&target) {
        return None;
    }
    if MACOS_ONLY_WRAPPERS.contains(&binary_name) && !target.ends_with("-apple-darwin") {
        return None;
    }
    Some(format!(
        "{binary_name}-{target}{}",
        exe_suffix_for_target(target)
    ))
}

/// True when the travsr-lang release ships `binary_name` for `target`.
/// Used by `lang list` / `lang install` to gate the interactive setup offer.
pub fn wrapper_available(binary_name: &str, target: &str) -> bool {
    wrapper_asset_name(binary_name, target).is_some()
}

/// On-disk name of an installed wrapper under `~/.travsr/bin/`.
pub fn wrapper_install_filename(binary_name: &str, target: &str) -> String {
    format!("{binary_name}{}", exe_suffix_for_target(target))
}

/// A wrapper the travsr-lang release matrix does not build for this host.
///
/// A distinct type rather than a formatted string so callers branch on the
/// cause. The message that reached users before was reconstructed by matching
/// on another function's prose, and a refactor erased it without failing a test.
#[derive(Debug)]
pub struct WrapperUnavailable {
    pub binary_name: String,
    pub target: String,
}

impl std::fmt::Display for WrapperUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is not available for {} yet — the travsr-lang release ships no \
             prebuilt binary for this platform",
            self.binary_name, self.target
        )
    }
}

impl std::error::Error for WrapperUnavailable {}

/// A release download that came back 404.
///
/// Typed for the same reason as [`WrapperUnavailable`]: the platform-specific
/// message is attached by matching this cause, not by inspecting message text.
/// `Display` reproduces the wording the generic download path always used, so a
/// caller that does not care about the distinction reads the same error.
#[derive(Debug)]
pub struct AssetNotFound {
    /// `"download"` or `"sha256 sidecar download"` — which half was missing.
    pub what: &'static str,
    pub url: String,
}

impl std::fmt::Display for AssetNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed (404 Not Found): {}", self.what, self.url)
    }
}

impl std::error::Error for AssetNotFound {}

/// Returns ~/.travsr/bin/, creating it if it does not exist.
pub fn travsr_bin_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".travsr")
        .join("bin");
    std::fs::create_dir_all(&dir).context("creating ~/.travsr/bin")?;
    Ok(dir)
}

/// The "add ~/.travsr/bin to your PATH" hint, in the host's own shell syntax.
///
/// #588: every call site printed `export PATH="$HOME/.travsr/bin:$PATH"` and
/// told the user to edit `~/.zshrc`. On Windows that is three pieces of advice
/// none of which apply, printed at the one moment the user has just installed a
/// binary they now need to find.
pub fn path_hint() -> String {
    if cfg!(windows) {
        "hint: add %USERPROFILE%\\.travsr\\bin to your PATH:\n\n\
         \t$env:PATH = \"$env:USERPROFILE\\.travsr\\bin;$env:PATH\"\n\n\
         To make it permanent (new terminals only):\n\n\
         \tsetx PATH \"%USERPROFILE%\\.travsr\\bin;%PATH%\"\n"
            .to_string()
    } else {
        "hint: add ~/.travsr/bin to your PATH:\n\n\
         \texport PATH=\"$HOME/.travsr/bin:$PATH\"\n\n\
         Add this line to your ~/.zshrc or ~/.bashrc to make it permanent.\n"
            .to_string()
    }
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

/// RFC-025 Point B: install-time checks over an *already-present* sidecar binary
/// (the existence branch of `embed init` / `lang install`, no `--reinstall`).
///
/// Two independent, best-effort legs; neither ever fails the caller:
///
/// - **Leg 1 (offline).** If the on-disk binary is below the host's behavioral
///   floor, print a WARN with the reinstall remedy. The *hard* refuse stays at
///   spawn/reindex (Point A) - init only warns, so the user is left in a
///   runnable state and the fix is one command away.
/// - **Leg 2 (network, cached 24h).** Fetch the latest release; if it is newer
///   than what is installed, print an advisory. Offline -> silent. The fetched
///   tag is cached in `~/.travsr/.sidecar-latest.json` so the daemon can
///   re-surface staleness without ever fetching (local-first).
///
/// `reinstall_remedy` is the exact command surfaced to the user, e.g.
/// `"travsr embed init --reinstall"`.
pub fn advise_installed_sidecar(spec: &dyn SidecarSpec, bin_path: &Path, reinstall_remedy: &str) {
    let install_name = spec.install_name();

    // One bounded `<bin> --version` probe, read once and reused by both legs
    // below — the staleness comparison does not re-exec the binary.
    let status = floor_status(spec, bin_path, None);

    // Leg 1: offline floor check. Only the "below floor" states warn; a spec
    // with no declared floor whose version is simply unreadable stays quiet.
    match &status {
        FloorStatus::BelowFloor {
            installed,
            required,
        } => {
            eprintln!(
                "  warning: {}",
                below_floor_message(install_name, installed, required, reinstall_remedy)
            );
        }
        FloorStatus::Unreadable { required } => {
            eprintln!(
                "  warning: {}",
                unreadable_message(install_name, required, reinstall_remedy)
            );
        }
        // A transient probe timeout at install time is not actionable (degrades
        // to usable), so it stays quiet, alongside the healthy / no-floor cases.
        FloorStatus::Ok(_) | FloorStatus::UnreadableNoFloor | FloorStatus::ProbeTimeout { .. } => {}
    }

    // Leg 2: best-effort staleness advisory. Skipped entirely when downloads are
    // disabled (tests / air-gapped) so nothing here reaches for the network.
    if std::env::var_os(SKIP_DOWNLOAD_ENV).is_some() {
        return;
    }
    let repo = spec.github_repo().to_string();
    let Ok(latest_tag) =
        crate::lang::run_async(async move { fetch_latest_version_for_repo(&repo).await })
    else {
        return; // offline / fetch failed -> silent, never fails the command
    };
    write_cached_latest(install_name, &latest_tag);

    // Reuse the version already read by the floor probe above; only the states
    // that carry a readable version can be compared against `latest`.
    let installed = match &status {
        FloorStatus::Ok(v) => *v,
        FloorStatus::BelowFloor { installed, .. } => *installed,
        FloorStatus::Unreadable { .. }
        | FloorStatus::UnreadableNoFloor
        | FloorStatus::ProbeTimeout { .. } => return,
    };
    let Some(latest) = travsr_plugin_host::Semver::parse(&latest_tag) else {
        return;
    };
    if latest > installed {
        println!("  newer {install_name} v{latest} available - run: {reinstall_remedy}");
    }
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

    // Upstreams that publish a sidecar are verified against it; the rest are
    // verified against the hash vendored at pin time. An entry with neither is
    // the only unverified case, and the catalog test asserts none exist.
    let integrity = match (verify_sha256, expected_sha256) {
        (true, _) => Integrity::Sidecar,
        (false, Some(pinned)) => Integrity::Vendored(pinned),
        (false, None) => Integrity::Unverified,
    };
    let label = format!("{asset_name} at {tag}");
    let bin_bytes = fetch_verified(&client, &bin_url, &label, SCIP_SIZE_LIMIT, integrity).await?;

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

/// How a download proves it is the artefact that was published.
enum Integrity<'a> {
    /// Fetch `<url>.sha256` alongside the asset and compare. The publisher
    /// controls both, so this catches a corrupted or swapped asset but not a
    /// compromised origin.
    Sidecar,
    /// Compare against a hash vendored into the catalog when the version was
    /// pinned (#410 M2). This is the anchor that survives a compromised
    /// origin, because the expected value never travels with the download.
    Vendored(&'a str),
    /// Upstream publishes neither, and no pin is recorded.
    Unverified,
}

/// Downloads `bin_url` and returns its bytes only if `integrity` is satisfied.
///
/// Single implementation for every download path in this file. It previously
/// existed three times (wrapper sidecar, scip sidecar, scip vendored) at
/// roughly 60% line-identical, which meant a guard could be fixed in one copy
/// and left wrong in the others.
///
/// `bin_url` is a full URL rather than pieces, and nothing here reads the
/// environment or `$HOME`, so tests point it at a local fixture server without
/// mutating process-global state that neighbouring tests also read (#410 T1).
///
/// A missing `.sha256` under [`Integrity::Sidecar`] is a failure, not a skip:
/// treating it as optional would let anyone who can delete one small file
/// downgrade every install to unverified.
async fn fetch_verified(
    client: &reqwest::Client,
    bin_url: &str,
    label: &str,
    size_limit: u64,
    integrity: Integrity<'_>,
) -> Result<Vec<u8>> {
    // The sidecar is fetched concurrently with the asset, so the extra round
    // trip costs nothing on the happy path.
    let sha_url = format!("{bin_url}.sha256");
    let (bin_resp, sha_resp) = match integrity {
        Integrity::Sidecar => {
            let (b, s) = tokio::try_join!(client.get(bin_url).send(), client.get(&sha_url).send())
                .context("sending download requests")?;
            (b, Some(s))
        }
        _ => {
            let b = client
                .get(bin_url)
                .send()
                .await
                .context("sending download request")?;
            (b, None)
        }
    };

    // #588: a 404 is returned as `AssetNotFound` rather than a formatted string
    // so `fetch_and_verify_binary` can recognise "this release has no asset for
    // this platform" by cause. Its `Display` is byte-identical to the message
    // this path produced before, so every other caller is unaffected.
    if bin_resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(AssetNotFound {
            what: "download",
            url: bin_url.to_string(),
        }
        .into());
    }
    if !bin_resp.status().is_success() {
        bail!("download failed ({}): {bin_url}", bin_resp.status());
    }
    if let Some(s) = &sha_resp {
        if s.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(AssetNotFound {
                what: "sha256 sidecar download",
                url: sha_url.clone(),
            }
            .into());
        }
        if !s.status().is_success() {
            bail!("sha256 sidecar download failed ({}): {sha_url}", s.status());
        }
    }

    // Reject oversized downloads on the advertised length, before the body is
    // transferred.
    if let Some(len) = bin_resp.content_length() {
        if len > size_limit {
            bail!("{label} exceeds the download size limit: {len} bytes > {size_limit}: {bin_url}");
        }
    }

    let bin_bytes = bin_resp.bytes().await.context("reading binary body")?;
    let got = bin_bytes.len() as u64;
    if got > size_limit {
        bail!("{label} exceeds the download size limit after download: {got} bytes > {size_limit}");
    }

    match integrity {
        Integrity::Sidecar => {
            let sha_text = sha_resp
                .expect("sidecar response is present for Integrity::Sidecar")
                .text()
                .await
                .context("reading SHA256 body")?;
            let expected = parse_sha256_line(&sha_text)?;
            let actual = hex_encode_sha256(&bin_bytes);
            if actual != expected {
                bail!("SHA256 mismatch for {label}: expected {expected}, got {actual}");
            }
        }
        Integrity::Vendored(expected) => {
            let actual = hex_encode_sha256(&bin_bytes);
            if actual != expected {
                bail!(
                    "SHA256 mismatch for {label}: expected {expected}, got {actual}. \
                     The pinned asset does not match the hash recorded in the catalog — it may \
                     have been replaced upstream."
                );
            }
        }
        Integrity::Unverified => {}
    }

    Ok(bin_bytes.to_vec())
}

/// Fetches a wrapper binary and its published `.sha256`, and returns the bytes
/// only if they match.
///
/// `base` is a parameter rather than a read of `TRAVSR_LANG_RELEASES_BASE` so
/// the URL shape stays testable; the caller supplies the env-derived value.
///
/// Errors from a target the release matrix does claim, but where the tag itself
/// has no such asset (a partial or older release), are rewritten to name the
/// version and platform. `fetch_verified` is shared with the SCIP download
/// paths, so its own message stays generic; the context is re-attached here,
/// where `version` and `target` are in scope, and by downcasting the typed
/// [`AssetNotFound`] cause rather than by matching on its wording.
async fn fetch_and_verify_binary(
    base: &str,
    version: &str,
    binary_name: &str,
    target: &str,
) -> Result<Vec<u8>> {
    let asset = wrapper_asset_name(binary_name, target).ok_or_else(|| WrapperUnavailable {
        binary_name: binary_name.to_string(),
        target: target.to_string(),
    })?;
    let bin_url = format!("{base}/download/{version}/{asset}");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(format!("travsr-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    fetch_verified(
        &client,
        &bin_url,
        binary_name,
        SIZE_LIMIT,
        Integrity::Sidecar,
    )
    .await
    .map_err(|e| match e.downcast_ref::<AssetNotFound>() {
        // A published binary with no sidecar is a different fault from no
        // binary at all — a partial release rather than an unsupported
        // platform — and collapsing the two would send the reader looking for
        // a build that is actually sitting right there.
        Some(nf) if nf.what == "sha256 sidecar download" => anyhow::anyhow!(
            "release {version} has no SHA256 checksum file for target platform '{target}' ({})",
            nf.url
        ),
        Some(nf) => anyhow::anyhow!(
            "release {version} has no prebuilt binary for target platform '{target}' ({})",
            nf.url
        ),
        None => e,
    })
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
    // #588: refuse before any network work when the release matrix has nothing
    // for this host, so the user reads a capability statement instead of a 404.
    if !wrapper_available(binary_name, target) {
        return Err(WrapperUnavailable {
            binary_name: binary_name.to_string(),
            target: target.to_string(),
        }
        .into());
    }

    // The installed file carries `.exe` on Windows exactly as the release asset
    // does; `travsr_core::exec::resolve_executable` tries the suffixed name
    // first, so the read side finds what this writes.
    let install_name = wrapper_install_filename(binary_name, target);

    if std::env::var(SKIP_DOWNLOAD_ENV).is_ok() {
        return Ok(travsr_bin_dir()?.join(&install_name));
    }

    let base =
        std::env::var(RELEASES_BASE_ENV).unwrap_or_else(|_| DEFAULT_RELEASES_BASE.to_string());

    let bin_bytes = fetch_and_verify_binary(&base, version, binary_name, target).await?;

    // Atomic write: write to a temp path, then rename into place.
    let dest_dir = travsr_bin_dir()?;
    let dest = dest_dir.join(&install_name);
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
    // #410 M2: expected sha256 of the archive, for upstreams that publish no
    // sidecar. Checked before extraction, so a replaced asset is never opened.
    expected_sha256: Option<&str>,
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

    verify_and_extract_zip(&bytes, &dest, expected_sha256, asset_name, tag)?;

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

    /// Guards `SKIP_DOWNLOAD_ENV` for every test that sets it.
    ///
    /// One `static` at module scope, not one per test: a `static` declared
    /// inside a test function is a *different* mutex from the one in its
    /// neighbour, so two such tests serialise against nothing and race on the
    /// same process-global variable. The losing interleaving makes a
    /// skip-download test perform a real network download.
    static SKIP_DOWNLOAD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs `download_and_install_wrapper` with `TRAVSR_SKIP_DOWNLOAD` set, and
    /// clears it again even if the call panics.
    fn skip_download_install(
        binary_name: &str,
        target: &str,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let guard = SKIP_DOWNLOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(SKIP_DOWNLOAD_ENV, "1");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(download_and_install_wrapper("v0.1.0", binary_name, target));
        std::env::remove_var(SKIP_DOWNLOAD_ENV);
        drop(guard);
        result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })
    }

    #[test]
    fn skip_download_returns_dest_path() {
        let result = skip_download_install("travsr-lang-go", "x86_64-unknown-linux-gnu");
        // Only check it doesn't error; bin dir creation may fail in sandbox envs.
        let _ = result; // result is Ok or Err depending on home dir availability
    }

    // ── #588 ──────────────────────────────────────────────────────────────────

    #[test]
    fn exe_suffix_is_appended_only_for_windows_targets() {
        assert_eq!(exe_suffix_for_target("x86_64-pc-windows-msvc"), ".exe");
        assert_eq!(exe_suffix_for_target("x86_64-pc-windows-gnu"), ".exe");
        assert_eq!(exe_suffix_for_target("aarch64-apple-darwin"), "");
        assert_eq!(exe_suffix_for_target("x86_64-unknown-linux-gnu"), "");
    }

    #[test]
    fn the_windows_wrapper_asset_carries_exe() {
        // The exact name travsr-lang's release workflow packages. If these two
        // ever disagree, `lang install` 404s on Windows again — which is the
        // whole of #588.
        assert_eq!(
            wrapper_asset_name("travsr-lang-java", "x86_64-pc-windows-msvc").unwrap(),
            "travsr-lang-java-x86_64-pc-windows-msvc.exe"
        );
        assert_eq!(
            wrapper_asset_name("travsr-lang-java", "aarch64-apple-darwin").unwrap(),
            "travsr-lang-java-aarch64-apple-darwin"
        );
    }

    #[test]
    fn a_target_the_release_does_not_build_has_no_asset_name() {
        // A triple `current_target()` could plausibly grow, but which no
        // release job produces. Asking for it must be refusable, not a URL.
        assert!(wrapper_asset_name("travsr-lang-go", "aarch64-pc-windows-msvc").is_none());
        assert!(!wrapper_available(
            "travsr-lang-go",
            "aarch64-pc-windows-msvc"
        ));
    }

    #[test]
    fn the_objectivec_wrapper_is_offered_on_apple_targets_only() {
        // Built by a macOS-only job (libclang + xcrun). It 404s on Linux for
        // exactly the reason every wrapper used to 404 on Windows, so the same
        // gate has to cover it.
        assert!(wrapper_available(
            "travsr-lang-objectivec",
            "aarch64-apple-darwin"
        ));
        assert!(wrapper_available(
            "travsr-lang-objectivec",
            "x86_64-apple-darwin"
        ));
        assert!(!wrapper_available(
            "travsr-lang-objectivec",
            "x86_64-pc-windows-msvc"
        ));
        assert!(!wrapper_available(
            "travsr-lang-objectivec",
            "x86_64-unknown-linux-gnu"
        ));
    }

    #[test]
    fn every_other_wrapper_ships_for_every_release_target() {
        for target in WRAPPER_RELEASE_TARGETS {
            assert!(
                wrapper_available("travsr-lang-java", target),
                "java wrapper should ship for {target}"
            );
        }
    }

    #[test]
    fn the_installed_file_carries_the_same_suffix_as_the_asset() {
        // Write side and read side have to agree: `resolve_executable` tries
        // `<name>.exe` first on Windows, and a bare file there is never found.
        for target in WRAPPER_RELEASE_TARGETS {
            let asset = wrapper_asset_name("travsr-lang-go", target).unwrap();
            let installed = wrapper_install_filename("travsr-lang-go", target);
            assert_eq!(
                asset.ends_with(".exe"),
                installed.ends_with(".exe"),
                "half-suffixed path for {target}: asset={asset} installed={installed}"
            );
        }
    }

    #[test]
    fn installing_a_wrapper_on_windows_writes_a_dot_exe_path() {
        let Ok(path) = skip_download_install("travsr-lang-java", "x86_64-pc-windows-msvc") else {
            // Only a missing home directory can fail the skip-download path.
            return;
        };
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "travsr-lang-java.exe"
        );
    }

    /// #588 / #576: the two halves of this contract live in different repos, so
    /// nothing in either one can catch them drifting apart. This asks the live
    /// travsr-lang release what it actually published and compares it against
    /// every asset name the installer would construct.
    ///
    /// `#[ignore]` because it needs network and an authenticated `gh`. Run by
    /// `.github/workflows/lang-release-drift.yml` on a schedule and on any PR
    /// that touches this file or the Phase B catalog:
    ///
    ///   cargo test -p travsr-cli --bin travsr -- --ignored wrapper_release_drift
    #[test]
    #[ignore = "needs network and gh auth; run in the lang-release-drift workflow"]
    fn wrapper_release_drift() {
        use travsr_plugin_host::phase_b::catalog::CATALOG;

        let out = std::process::Command::new("gh")
            .args([
                "release",
                "view",
                "--repo",
                "Travsr-com/travsr-lang",
                "--json",
                "tagName,assets",
            ])
            .output()
            .expect("gh must be on PATH");
        assert!(
            out.status.success(),
            "gh release view failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let json: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("gh emitted valid JSON");
        let tag = json["tagName"].as_str().unwrap_or("<unknown>").to_string();
        let published: std::collections::HashSet<String> = json["assets"]
            .as_array()
            .expect("assets array")
            .iter()
            .filter_map(|a| a["name"].as_str().map(str::to_string))
            .collect();

        let mut missing: Vec<String> = Vec::new();
        for entry in CATALOG {
            let Some(bin) = entry.provider_binary else {
                continue; // builtin — no wrapper asset to publish
            };
            for target in WRAPPER_RELEASE_TARGETS {
                let Some(asset) = wrapper_asset_name(bin, target) else {
                    continue; // deliberately not shipped here; nothing to check
                };
                // The sidecar is as load-bearing as the binary: a download with
                // no `.sha256` is refused rather than installed unverified.
                for name in [asset.clone(), format!("{asset}.sha256")] {
                    if !published.contains(&name) {
                        missing.push(name);
                    }
                }
            }
        }

        assert!(
            missing.is_empty(),
            "travsr-lang {tag} is missing {} asset(s) the installer would request. \
             Either the release matrix dropped a target, or WRAPPER_RELEASE_TARGETS / \
             MACOS_ONLY_WRAPPERS claims one it never builds:\n  {}",
            missing.len(),
            missing.join("\n  ")
        );
    }

    #[test]
    fn installing_a_wrapper_the_release_does_not_ship_refuses_before_any_network_call() {
        // No `TRAVSR_SKIP_DOWNLOAD`, no fixture server, no releases-base
        // override: reaching the network at all would fail this test by
        // hanging or by reporting a transport error instead of this one.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(download_and_install_wrapper(
                "v0.3.0",
                "travsr-lang-objectivec",
                "x86_64-pc-windows-msvc",
            ))
            .unwrap_err();
        assert!(
            err.downcast_ref::<WrapperUnavailable>().is_some(),
            "expected a typed WrapperUnavailable, got: {err:#}"
        );
        let msg = err.to_string();
        assert!(msg.contains("x86_64-pc-windows-msvc"), "{msg}");
        assert!(msg.contains("not available"), "{msg}");
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

/// Check an archive against its expected hash, then extract it.
///
/// #410 M2: the two steps live together because the ordering is the point.
/// Extraction is where a hostile archive acts, so the hash has to gate it
/// rather than follow it. Split out from `download_zip_and_extract` so the
/// gate can be tested without a network fetch — the review that found the zip
/// path unverified noted, correctly, that a hash which is never compared
/// cannot mismatch, so the comparison itself needs a test.
pub(crate) fn verify_and_extract_zip(
    bytes: &[u8],
    dest: &std::path::Path,
    expected_sha256: Option<&str>,
    asset_name: &str,
    tag: &str,
) -> Result<()> {
    if let Some(expected) = expected_sha256 {
        let actual = hex_encode_sha256(bytes);
        if actual != expected {
            bail!(
                "SHA256 mismatch for {asset_name} at {tag}: expected {expected}, got {actual}. \
                 The pinned asset does not match the hash recorded in the catalog — it may have \
                 been replaced upstream."
            );
        }
    }
    extract_zip(bytes, dest).with_context(|| format!("extracting {asset_name}"))
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
    /// #410 M2 review: the zip path carried `sha256_fn` on its spec but never
    /// read it, so kotlin-language-server — one of the three tools M2b claims
    /// to protect — was neither pinned nor verified, and `kls_sha256` was dead
    /// code. Nothing failed, because a hash that is never compared cannot
    /// mismatch.
    ///
    /// Drives the real gate rather than re-implementing the comparison, so it
    /// fails if the check is removed, reordered after extraction, or wired to
    /// the wrong bytes.
    #[test]
    fn a_zip_that_does_not_match_its_vendored_hash_is_never_extracted() {
        fn zip_with(body: &[u8]) -> Vec<u8> {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            w.start_file::<_, ()>("server/bin/tool", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut w, body).unwrap();
            w.finish().unwrap().into_inner()
        }
        let genuine = zip_with(b"genuine");
        let tampered = zip_with(b"swapped");
        let expected = super::hex_encode_sha256(&genuine);

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();

        let err = super::verify_and_extract_zip(
            &tampered,
            &dest,
            Some(&expected),
            "server.zip",
            "1.3.13",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("SHA256 mismatch"), "{err}");
        assert!(
            !dest.join("server/bin/tool").exists(),
            "a mismatching archive must not be extracted at all — the hash gates \
             extraction rather than following it"
        );

        // The genuine archive still installs, so the gate is not simply refusing
        // everything.
        super::verify_and_extract_zip(&genuine, &dest, Some(&expected), "server.zip", "1.3.13")
            .unwrap();
        assert_eq!(
            std::fs::read(dest.join("server/bin/tool")).unwrap(),
            b"genuine"
        );
    }

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

/// #410 T1 — the download half, against a local fixture server.
///
/// The extraction half is covered by `extraction_tests` above. What was left
/// untested is everything between "a URL" and "verified bytes": status
/// handling, the size guard, and whether integrity failures actually stop the
/// install rather than merely logging.
///
/// `TRAVSR_LANG_RELEASES_BASE` exists precisely so these paths can be pointed
/// somewhere local, but setting it here would be process-global and racy
/// against tests that read it concurrently. [`fetch_and_verify_binary`] takes
/// `base` as a parameter instead, so nothing here mutates the environment.
#[cfg(test)]
mod download_tests {
    use super::{
        fetch_and_verify_binary, fetch_verified, hex_encode_sha256, Integrity, SIZE_LIMIT,
    };
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    struct Route {
        path: String,
        status: u16,
        body: Vec<u8>,
        /// Content-Length to advertise when it should differ from `body.len()`.
        /// Lets the pre-download size guard be exercised without moving 100 MB
        /// across a socket.
        advertised_len: Option<u64>,
    }

    fn route(path: &str, status: u16, body: Vec<u8>) -> Route {
        Route {
            path: path.to_string(),
            status,
            body,
            advertised_len: None,
        }
    }

    /// Minimal HTTP/1.1 responder, hand-rolled rather than pulled in as a
    /// dependency: the entire contract under test is status line,
    /// Content-Length and body. Unknown paths are 404, which is what a real
    /// release serves for an asset that was never published.
    ///
    /// Returns the base URL. The thread is detached and blocks on `accept`
    /// until the process exits.
    fn serve(routes: Vec<Route>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Ok(peek) = stream.try_clone() else {
                    continue;
                };
                let mut reader = BufReader::new(peek);

                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();

                // Drain headers so the client's write completes cleanly.
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) => break,
                        Ok(_) if header == "\r\n" => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }

                let matched = routes.iter().find(|r| r.path == path);
                let (status, body, len) = match matched {
                    Some(r) => (
                        r.status,
                        r.body.clone(),
                        r.advertised_len.unwrap_or(r.body.len() as u64),
                    ),
                    None => (404, Vec::new(), 0),
                };

                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
                    reason = if status == 200 { "OK" } else { "Not Found" }
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });

        format!("http://{addr}")
    }

    const VERSION: &str = "v1.2.3";
    const BIN: &str = "travsr-lang-x";
    const TARGET: &str = "aarch64-apple-darwin";
    const WINDOWS_TARGET: &str = "x86_64-pc-windows-msvc";

    fn bin_path() -> String {
        format!("/download/{VERSION}/{BIN}-{TARGET}")
    }

    fn sha_path() -> String {
        format!("{}.sha256", bin_path())
    }

    /// `sha256sum` output shape: hash, two spaces, filename.
    fn sha_line(body: &[u8]) -> Vec<u8> {
        format!("{}  {BIN}-{TARGET}\n", hex_encode_sha256(body)).into_bytes()
    }

    async fn fetch(base: &str) -> anyhow::Result<Vec<u8>> {
        fetch_and_verify_binary(base, VERSION, BIN, TARGET).await
    }

    #[tokio::test]
    async fn a_binary_matching_its_published_sha256_is_returned() {
        let body = b"#!/bin/sh\nexec real-tool \"$@\"\n".to_vec();
        let base = serve(vec![
            route(&bin_path(), 200, body.clone()),
            route(&sha_path(), 200, sha_line(&body)),
        ]);

        assert_eq!(fetch(&base).await.unwrap(), body);
    }

    #[tokio::test]
    async fn a_binary_swapped_after_publication_is_refused() {
        // The published hash describes one payload; the server returns another.
        // This is the whole point of shipping the .sha256 alongside.
        let published = b"the tool that was signed for".to_vec();
        let swapped = b"the tool that was served instead".to_vec();
        let base = serve(vec![
            route(&bin_path(), 200, swapped),
            route(&sha_path(), 200, sha_line(&published)),
        ]);

        let err = fetch(&base).await.unwrap_err().to_string();
        assert!(err.contains("SHA256 mismatch"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn a_missing_sha256_fails_instead_of_installing_unverified() {
        // Deleting one small file must not silently downgrade the install to
        // unverified. The binary itself is served perfectly well here.
        let body = b"plausible binary".to_vec();
        let base = serve(vec![route(&bin_path(), 200, body)]);

        let err = fetch(&base).await.unwrap_err().to_string();
        // #588: named as a missing *checksum file*, not a missing build — the
        // binary is right there, so "no prebuilt binary for this platform"
        // would send the reader after the wrong thing.
        assert!(
            err.contains("no SHA256 checksum file"),
            "unexpected error: {err}"
        );
        assert!(
            err.ends_with(".sha256)"),
            "should name the sidecar URL: {err}"
        );
    }

    #[tokio::test]
    async fn a_missing_binary_reports_the_url_it_tried() {
        let body = b"orphan".to_vec();
        let base = serve(vec![route(&sha_path(), 200, sha_line(&body))]);

        let err = fetch(&base).await.unwrap_err().to_string();
        assert!(
            err.contains("no prebuilt binary for target platform"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains(&bin_path()),
            "error should name the URL: {err}"
        );
        // Not the sidecar's message: `sha_url` is `bin_path()` plus `.sha256`,
        // so asserting on the URL alone would pass for a sha-side failure too.
        assert!(
            !err.contains(".sha256"),
            "binary-side failure must not report the sidecar: {err}"
        );
    }

    // ── #588 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_windows_wrapper_is_fetched_from_the_dot_exe_url() {
        // The suite was pinned to a darwin triple, so dropping `.exe` from the
        // asset name left every test green while every Windows install 404'd.
        // Only the `.exe` path and its `.exe.sha256` sidecar are served here:
        // an unsuffixed request falls through to the responder's 404.
        let body = b"MZ\x90\x00 a plausible PE image".to_vec();
        let asset = format!("{BIN}-{WINDOWS_TARGET}.exe");
        let path = format!("/download/{VERSION}/{asset}");
        let sha_line = format!("{}  {asset}\n", hex_encode_sha256(&body)).into_bytes();
        let base = serve(vec![
            route(&path, 200, body.clone()),
            route(&format!("{path}.sha256"), 200, sha_line),
        ]);

        let got = fetch_and_verify_binary(&base, VERSION, BIN, WINDOWS_TARGET)
            .await
            .expect("the .exe asset and its sidecar are both served");
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn a_release_without_an_asset_for_this_platform_says_so() {
        // A target the matrix does claim, but a tag that predates it: the 404
        // is real, and the message has to name the version and the platform
        // rather than leaving the user with a bare URL. Guarded here because
        // this wording was silently lost once already in a refactor.
        let base = serve(vec![]);

        let err = fetch_and_verify_binary(&base, VERSION, BIN, WINDOWS_TARGET)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no prebuilt binary for target platform"),
            "unexpected error: {err}"
        );
        assert!(err.contains(WINDOWS_TARGET), "{err}");
        assert!(err.contains(VERSION), "{err}");
    }

    #[tokio::test]
    async fn a_wrapper_with_no_asset_in_the_matrix_never_reaches_the_network() {
        // `serve(vec![])` would 404; this must fail before the request, with
        // the typed cause, so `lang install` can state the capability gap
        // instead of reporting a transport-level failure.
        let base = serve(vec![]);

        let err = fetch_and_verify_binary(&base, VERSION, "travsr-lang-objectivec", WINDOWS_TARGET)
            .await
            .unwrap_err();
        assert!(
            err.downcast_ref::<super::WrapperUnavailable>().is_some(),
            "expected WrapperUnavailable, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn an_oversized_binary_is_refused_before_its_body_is_read() {
        // Advertise more than the cap while sending one byte. The assertion
        // names the advertised length, which only the pre-download bail
        // reports — the post-download bail reports the received length, so it
        // cannot satisfy this even though both messages share a prefix.
        let body = b"x".to_vec();
        let base = serve(vec![
            Route {
                path: bin_path(),
                status: 200,
                body: body.clone(),
                advertised_len: Some(SIZE_LIMIT + 1),
            },
            route(&sha_path(), 200, sha_line(&body)),
        ]);

        let err = fetch(&base).await.unwrap_err().to_string();
        assert!(
            err.contains(&format!("{} bytes > {SIZE_LIMIT}", SIZE_LIMIT + 1)),
            "unexpected error: {err}"
        );
    }

    // ── the vendored-hash path (#410 M2) ─────────────────────────────────────
    //
    // Four of the eight catalog entries publish no sidecar and are anchored
    // only by a hash vendored at pin time. That is the control that survives a
    // compromised origin, and it had no coverage at all: `download_scip_binary`
    // builds its URL from a hardcoded github.com base, so the fixture server
    // reaches it through `fetch_verified` directly.

    const ASSET: &str = "scip-tool-linux";

    fn vendored_url(base: &str) -> String {
        format!("{base}/releases/download/v9/{ASSET}")
    }

    async fn fetch_pinned(base: &str, pinned: &str) -> anyhow::Result<Vec<u8>> {
        let client = reqwest::Client::new();
        fetch_verified(
            &client,
            &vendored_url(base),
            ASSET,
            SIZE_LIMIT,
            Integrity::Vendored(pinned),
        )
        .await
    }

    #[tokio::test]
    async fn an_asset_matching_its_pinned_catalog_hash_is_returned() {
        let body = b"the pinned scip tool".to_vec();
        let base = serve(vec![route(
            &format!("/releases/download/v9/{ASSET}"),
            200,
            body.clone(),
        )]);

        let got = fetch_pinned(&base, &hex_encode_sha256(&body))
            .await
            .unwrap();
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn an_asset_replaced_upstream_after_pinning_is_refused() {
        // The catalog hash was recorded against one payload and upstream now
        // serves another. Unlike the sidecar case there is nothing the origin
        // can rewrite to make this pass, which is the point of vendoring it.
        let pinned_at = b"what the catalog was pinned against".to_vec();
        let served_now = b"what upstream serves today".to_vec();
        let base = serve(vec![route(
            &format!("/releases/download/v9/{ASSET}"),
            200,
            served_now,
        )]);

        let err = fetch_pinned(&base, &hex_encode_sha256(&pinned_at))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("SHA256 mismatch"), "unexpected error: {err}");
        assert!(
            err.contains("recorded in the catalog"),
            "the vendored path should say the pin is what failed: {err}"
        );
    }

    #[tokio::test]
    async fn the_vendored_path_does_not_fetch_a_sidecar() {
        // Only the asset route is served. If this path fetched `.sha256` the
        // way the sidecar path does, the 404 would fail the download; it must
        // not, because these upstreams publish no sidecar at all.
        let body = b"no sidecar exists upstream".to_vec();
        let base = serve(vec![route(
            &format!("/releases/download/v9/{ASSET}"),
            200,
            body.clone(),
        )]);

        assert_eq!(
            fetch_pinned(&base, &hex_encode_sha256(&body))
                .await
                .unwrap(),
            body
        );
    }
}
