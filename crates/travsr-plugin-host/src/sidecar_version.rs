//! Sidecar version contract (RFC-025).
//!
//! Travsr spawns external, independently-versioned binaries: the embed sidecar
//! (`travsr-embed`) and the Phase B language family (`travsr-lang-*`, `scip-*`).
//! Historically each was gated by file *presence* alone, which is monotonic: it
//! never flips false when a better release ships. Issue #701 is the failure that
//! exposed this — a `travsr-embed` v1.0.0 binary lingered three releases behind
//! the host and produced a cryptic SQLite error instead of "your sidecar is too
//! old."
//!
//! This module is the one place both families share. It provides:
//!
//! - [`Semver`] + [`parse_sidecar_version`]: read a version out of a binary's
//!   `--version` output or its negotiated handshake `plugin_version`.
//! - [`SidecarSpec`]: a uniform descriptor implemented by every sidecar catalog
//!   struct, adding exactly one new axis — [`SidecarSpec::min_version`], the
//!   behavioral floor the host requires.
//! - [`floor_status`] / [`check_and_log`]: the offline, network-free
//!   compatibility gate (Point A). Below the floor → the caller refuses with a
//!   named remedy instead of failing deep.
//! - A 24h-TTL latest-release cache ([`read_cached_latest`] /
//!   [`write_cached_latest`]) that the install-time advisory (Point B) warms and
//!   the daemon reads (it never fetches — local-first, no surprise network).
//!
//! Everything here is strictly local: no network, no model, no graph. The floor
//! is enforced from the handshake the process already negotiates or a cheap
//! local `--version` probe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

// ── Semver ──────────────────────────────────────────────────────────────────

/// Minimal semantic version (`major.minor.patch`). Deliberately tiny: the
/// sidecar contract only needs ordering and display, not the full semver grammar
/// (pre-release / build metadata are ignored), so a dependency would be overkill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Semver {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Semver {
    /// The trivially-met floor: a spec declaring `ZERO` has no active floor, so
    /// even an unreadable version cannot violate it (see [`floor_status`]).
    pub const ZERO: Semver = Semver {
        major: 0,
        minor: 0,
        patch: 0,
    };

    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Semver {
            major,
            minor,
            patch,
        }
    }

    /// Parse a clean or `v`-prefixed / tag-embedded semver, e.g. `"1.2.0"`,
    /// `"v1.2.0"`, `"scip-ruby-v0.4.7"`. Returns `None` when no `X.Y.Z` triple is
    /// present. Convenience wrapper over [`parse_sidecar_version`] for a single
    /// token such as a catalog `version_fallback`.
    pub fn parse(s: &str) -> Option<Semver> {
        parse_sidecar_version(s)
    }
}

impl std::fmt::Display for Semver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Scan `text` for the first whitespace-delimited token that contains an
/// `X.Y.Z` semver and return it.
///
/// **Why not `.last()`** (the shape the pre-RFC `sidecar_supports_cancel` used):
/// real `--version` output is not uniformly a single `name X.Y.Z` token. Against
/// the sidecars actually installed on a dev box, `.last()` silently mis-reads
/// two of five (RFC-025 §5.2):
///
/// | binary output                          | `.last()`     | first-semver-token |
/// |----------------------------------------|---------------|--------------------|
/// | `travsr-embed 1.2.0`                   | `1.2.0` ok    | `1.2.0`            |
/// | `scip-java` -> `0.12.3`                | `0.12.3` ok   | `0.12.3`          |
/// | `scip-clang 0.4.0` (+ trailing lines)  | some junk     | `0.4.0`           |
/// | `scip-ruby 0.4.7 git <hash> clean=1`   | `clean=1`     | `0.4.7`           |
///
/// Scanning for the first token that parses as `X.Y.Z` reads all of them.
pub fn parse_sidecar_version(text: &str) -> Option<Semver> {
    text.split_whitespace().find_map(parse_semver_token)
}

/// Parse one token that may carry a leading prefix (`v`, `scip-ruby-v`, …) and
/// trailing junk (`-rc1`). Returns `None` unless a full `X.Y.Z` is present.
fn parse_semver_token(tok: &str) -> Option<Semver> {
    // Skip any leading non-digit prefix (`v`, `scip-ruby-v`, …).
    let start = tok.find(|c: char| c.is_ascii_digit())?;
    let mut parts = tok[start..].split('.');
    let major = leading_u64(parts.next()?)?;
    let minor = leading_u64(parts.next()?)?;
    let patch = leading_u64(parts.next()?)?;
    Some(Semver::new(major, minor, patch))
}

/// Parse the leading run of ASCII digits of `s` (so `"0-rc1"` -> `0`). `None`
/// when `s` does not begin with a digit.
fn leading_u64(s: &str) -> Option<u64> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

// ── SidecarSpec ─────────────────────────────────────────────────────────────

/// Uniform descriptor over every external sidecar binary Travsr spawns.
///
/// Implemented by `EmbedBackend`, `PhaseBEntry`, `ScipBinarySpec` and
/// `ZipBinarySpec`. All methods but [`min_version`](SidecarSpec::min_version)
/// are read-throughs to data those structs already hold; `min_version` is the
/// single genuinely new field — the behavioral floor the *host* requires.
///
/// The point of the trait is that Point A (the floor) and Point B (the
/// advisory) are written once against `&dyn SidecarSpec`, so both families and
/// any future sidecar inherit the contract by implementing five methods.
pub trait SidecarSpec {
    /// On-disk binary name under `~/.travsr/bin/`, e.g. `"travsr-embed"`,
    /// `"scip-java"`. Used as the stable de-dup + cache key and in messages.
    fn install_name(&self) -> &str;
    /// GitHub `owner/repo` slug the install-time advisory fetches `latest` from.
    fn github_repo(&self) -> &str;
    /// The minimum version the host requires for correct behavior. Bumped in the
    /// same commit that first relies on a sidecar behavior (RFC-025 decision 3).
    /// `Semver::ZERO` means "no active floor" for this spec.
    fn min_version(&self) -> Semver;
    /// Offline fallback tag used when the GitHub API is unreachable at install.
    /// Kept `>= min_version` by an honesty test so a fresh offline install can
    /// never land below the floor.
    fn version_fallback(&self) -> &str;
    /// True when the entry is pinned to `version_fallback` (supply-chain hash
    /// vendoring) rather than resolving `releases/latest`.
    fn pinned(&self) -> bool;
}

// ── Installed-version reader (offline) ──────────────────────────────────────

/// Read the installed version of a sidecar binary. Strictly local, no network.
///
/// Resolution order (RFC-025 §5.2):
/// 1. Prefer an already-negotiated handshake `plugin_version` (`live`) — for the
///    embed sidecar this is live at zero cost once spawned.
/// 2. Else a cheap `<bin> --version` probe.
///
/// Returns `None` when neither yields a parseable `X.Y.Z` (an ancient binary
/// that predates `--version`, or one whose probe exits non-zero).
pub fn installed_version(bin: &Path, live: Option<&str>) -> Option<Semver> {
    if let Some(v) = live {
        if let Some(s) = parse_sidecar_version(v) {
            return Some(s);
        }
    }
    let out = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_sidecar_version(&String::from_utf8_lossy(&out.stdout))
}

// ── Point A: the compatibility floor ────────────────────────────────────────

/// Outcome of the offline floor check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorStatus {
    /// Installed version satisfies the declared floor.
    Ok(Semver),
    /// Installed version is below the required floor — hard refuse with remedy.
    BelowFloor { installed: Semver, required: Semver },
    /// Version could not be read AND a floor is declared: compliance cannot be
    /// proven, so this is treated conservatively as a refusal (§9).
    Unreadable { required: Semver },
    /// Version could not be read but no floor is declared (`min_version` ==
    /// `ZERO`): the floor is trivially met, so this is allowed (§9).
    UnreadableNoFloor,
}

impl FloorStatus {
    /// True when the sidecar may be used (floor satisfied, or no floor declared).
    pub fn is_usable(&self) -> bool {
        matches!(self, FloorStatus::Ok(_) | FloorStatus::UnreadableNoFloor)
    }
}

/// Compare a binary's installed version against its spec's declared floor.
/// 100% offline: reads the handshake `live` value or a local `--version` probe.
pub fn floor_status(spec: &dyn SidecarSpec, bin: &Path, live: Option<&str>) -> FloorStatus {
    let required = spec.min_version();
    match installed_version(bin, live) {
        Some(v) if v >= required => FloorStatus::Ok(v),
        Some(v) => FloorStatus::BelowFloor {
            installed: v,
            required,
        },
        None if required == Semver::ZERO => FloorStatus::UnreadableNoFloor,
        None => FloorStatus::Unreadable { required },
    }
}

/// The user-facing refusal for a below-floor sidecar, e.g.
/// `travsr-embed v1.0.0 is below the required v1.2.0 - run: travsr embed init --reinstall`.
pub fn below_floor_message(
    install_name: &str,
    installed: &Semver,
    required: &Semver,
    remedy: &str,
) -> String {
    format!("{install_name} v{installed} is below the required v{required} - run: {remedy}")
}

/// The user-facing refusal for a sidecar whose version cannot be read while a
/// floor is declared. Its age cannot be proven, so it is refused conservatively.
pub fn unreadable_message(install_name: &str, required: &Semver, remedy: &str) -> String {
    format!(
        "{install_name} version could not be read, so it cannot be confirmed at or above the required v{required} - run: {remedy}"
    )
}

// ── Structured logging + transition-only de-dup (RFC-025 §7) ────────────────

/// Last version we logged a `below_floor` line for, keyed by `install_name`.
/// The health line holds current state; the log records only transitions, so a
/// below-floor sidecar spawned every ~5s produces exactly one WARN per distinct
/// `(install_name, installed_version)`.
fn below_floor_dedup() -> &'static Mutex<HashMap<String, Semver>> {
    static M: OnceLock<Mutex<HashMap<String, Semver>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Last `(installed, latest)` pair we logged a `stale` advisory for.
fn stale_dedup() -> &'static Mutex<HashMap<String, (Semver, Semver)>> {
    static M: OnceLock<Mutex<HashMap<String, (Semver, Semver)>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a below-floor observation and report whether it is a *new* transition
/// (i.e. this `(install_name, installed)` was not the last one logged). Returns
/// `true` at most once per distinct value, so a sidecar refused on every ~5s
/// scheduler tick logs exactly one WARN line — the transition-only contract
/// (RFC-025 §7.2). A reinstall that changes the installed version is a new
/// transition and may log again.
fn note_below_floor_transition(install_name: &str, installed: Semver) -> bool {
    let mut map = below_floor_dedup()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if map.get(install_name) == Some(&installed) {
        return false;
    }
    map.insert(install_name.to_string(), installed);
    true
}

/// As [`note_below_floor_transition`] but for the `stale` advisory, keyed on the
/// `(installed, latest)` pair.
fn note_stale_transition(install_name: &str, installed: Semver, latest: Semver) -> bool {
    let mut map = stale_dedup().lock().unwrap_or_else(|e| e.into_inner());
    let pair = (installed, latest);
    if map.get(install_name) == Some(&pair) {
        return false;
    }
    map.insert(install_name.to_string(), pair);
    true
}

/// Run the Point A floor check for a spawn/resolve, emit the stable structured
/// log events (RFC-025 §7.1), and return the [`FloorStatus`] so the caller can
/// refuse (CLI) or skip (daemon).
///
/// - `sidecar.version.checked` (DEBUG) fires every call so healthy spawns do not
///   flood at info.
/// - `sidecar.version.below_floor` (WARN) and `sidecar.version.stale` (INFO) are
///   **transition-only**: at most one line per distinct value.
/// - `sidecar.version.unreadable` (WARN) fires when the version cannot be parsed.
///
/// `family` is a coarse label (`"embed"` / `"phase_b"`). `remedy` is the exact
/// command string surfaced to the user. The `stale` advisory is emitted from the
/// local TTL cache only — this function never touches the network.
pub fn check_and_log(
    family: &str,
    spec: &dyn SidecarSpec,
    bin: &Path,
    live: Option<&str>,
    remedy: &str,
) -> FloorStatus {
    let status = floor_status(spec, bin, live);
    let install_name = spec.install_name();
    let min = spec.min_version();

    let (installed_str, disposition) = match &status {
        FloorStatus::Ok(v) => (v.to_string(), "ok"),
        FloorStatus::BelowFloor { installed, .. } => (installed.to_string(), "below_floor"),
        FloorStatus::Unreadable { .. } | FloorStatus::UnreadableNoFloor => {
            ("unreadable".to_string(), "unreadable")
        }
    };
    tracing::debug!(
        event = "sidecar.version.checked",
        family,
        install_name,
        installed_version = %installed_str,
        min_version = %min,
        disposition,
        "sidecar version checked"
    );

    match &status {
        FloorStatus::BelowFloor {
            installed,
            required,
        } => {
            if note_below_floor_transition(install_name, *installed) {
                tracing::warn!(
                    event = "sidecar.version.below_floor",
                    install_name,
                    installed = %installed,
                    required = %required,
                    remedy,
                    "sidecar is below the required version floor"
                );
            }
        }
        FloorStatus::Unreadable { required } => {
            tracing::warn!(
                event = "sidecar.version.unreadable",
                install_name,
                reason = "version output could not be parsed and a floor is declared",
                required = %required,
                "sidecar version could not be read"
            );
        }
        FloorStatus::UnreadableNoFloor => {
            tracing::warn!(
                event = "sidecar.version.unreadable",
                install_name,
                reason = "version output could not be parsed",
                "sidecar version could not be read (no floor declared, allowed)"
            );
        }
        FloorStatus::Ok(installed) => {
            // Point B / §7.1: re-surface a stale advisory from the local cache
            // only. A cold/expired cache means this simply does not fire.
            if let Some(latest) = read_cached_latest(install_name) {
                if latest > *installed && note_stale_transition(install_name, *installed, latest) {
                    tracing::info!(
                        event = "sidecar.version.stale",
                        install_name,
                        installed = %installed,
                        latest = %latest,
                        "a newer sidecar release is available"
                    );
                }
            }
        }
    }

    status
}

// ── Point B cache: ~/.travsr/.sidecar-latest.json (24h TTL) ─────────────────

/// How long a cached `latest` tag is trusted before it is treated as cold.
const LATEST_TTL_SECS: u64 = 24 * 60 * 60;

fn cache_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".travsr")
            .join(".sidecar-latest.json"),
    )
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct LatestCache {
    #[serde(default)]
    entries: HashMap<String, LatestEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LatestEntry {
    /// Latest release tag as fetched, e.g. `"v1.3.0"`.
    latest: String,
    /// Unix seconds at fetch time.
    fetched_at: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_cache_at(path: &Path) -> LatestCache {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => LatestCache::default(),
    }
}

/// Path-and-clock-injectable core of [`read_cached_latest`] so the TTL logic is
/// testable without touching the real `$HOME`.
fn read_cached_latest_at(path: &Path, install_name: &str, now: u64) -> Option<Semver> {
    let cache = load_cache_at(path);
    let entry = cache.entries.get(install_name)?;
    if now.saturating_sub(entry.fetched_at) > LATEST_TTL_SECS {
        return None; // expired -> cold
    }
    parse_sidecar_version(&entry.latest)
}

/// Path-injectable core of [`write_cached_latest`].
fn write_cached_latest_at(path: &Path, install_name: &str, latest_tag: &str, now: u64) {
    let mut cache = load_cache_at(path);
    cache.entries.insert(
        install_name.to_string(),
        LatestEntry {
            latest: latest_tag.to_string(),
            fetched_at: now,
        },
    );
    if let Ok(text) = serde_json::to_string_pretty(&cache) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, text);
    }
}

/// Read the cached latest release for `install_name`, honoring the 24h TTL.
/// Returns `None` when there is no entry, the entry is expired, or the tag does
/// not parse. The daemon uses this to re-surface a staleness advisory without
/// ever fetching (RFC-025 §5.4).
pub fn read_cached_latest(install_name: &str) -> Option<Semver> {
    let path = cache_path()?;
    read_cached_latest_at(&path, install_name, now_secs())
}

/// Record a freshly-fetched latest release tag for `install_name`. Best-effort:
/// any I/O error is swallowed (the advisory is a convenience, never load-bearing).
/// Only Point B's install-time fetch calls this — the daemon never fetches.
pub fn write_cached_latest(install_name: &str, latest_tag: &str) {
    let Some(path) = cache_path() else {
        return;
    };
    write_cached_latest_at(&path, install_name, latest_tag, now_secs());
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_and_prefixed_and_embedded_semvers() {
        assert_eq!(Semver::parse("1.2.0"), Some(Semver::new(1, 2, 0)));
        assert_eq!(Semver::parse("v1.2.0"), Some(Semver::new(1, 2, 0)));
        assert_eq!(
            Semver::parse("scip-ruby-v0.4.7"),
            Some(Semver::new(0, 4, 7))
        );
        assert_eq!(Semver::parse("1.3.13"), Some(Semver::new(1, 3, 13)));
        assert_eq!(Semver::parse("v0.12.3"), Some(Semver::new(0, 12, 3)));
    }

    #[test]
    fn first_semver_token_not_last() {
        // The exact real-world outputs the shipped `.last()` parser mis-read.
        assert_eq!(
            parse_sidecar_version("travsr-embed 1.2.0"),
            Some(Semver::new(1, 2, 0))
        );
        assert_eq!(
            parse_sidecar_version("scip-clang 0.4.0\nfeatures: parallel"),
            Some(Semver::new(0, 4, 0))
        );
        assert_eq!(
            parse_sidecar_version("scip-ruby 0.4.7 git deadbeef clean=1"),
            Some(Semver::new(0, 4, 7))
        );
    }

    #[test]
    fn rejects_output_with_no_semver() {
        assert_eq!(parse_sidecar_version(""), None);
        assert_eq!(parse_sidecar_version("travsr-embed-nomic"), None);
        assert_eq!(parse_sidecar_version("clean=1 x86_64"), None);
    }

    #[test]
    fn ordering_is_semver_correct() {
        assert!(Semver::new(1, 2, 0) > Semver::new(1, 0, 0));
        assert!(Semver::new(1, 10, 0) > Semver::new(1, 9, 9));
        assert!(Semver::new(0, 4, 7) < Semver::new(0, 12, 3));
        assert!(Semver::new(1, 2, 0) >= Semver::new(1, 2, 0));
    }

    struct FakeSpec {
        min: Semver,
    }
    impl SidecarSpec for FakeSpec {
        fn install_name(&self) -> &str {
            "fake"
        }
        fn github_repo(&self) -> &str {
            "acme/fake"
        }
        fn min_version(&self) -> Semver {
            self.min
        }
        fn version_fallback(&self) -> &str {
            "v1.0.0"
        }
        fn pinned(&self) -> bool {
            false
        }
    }

    #[test]
    fn floor_status_allows_at_or_above_and_refuses_below() {
        let spec = FakeSpec {
            min: Semver::new(1, 2, 0),
        };
        // A binary that does not exist -> --version fails -> unreadable.
        let missing = Path::new("/nonexistent/travsr-embed-xyz");
        assert_eq!(
            floor_status(&spec, missing, Some("1.2.0")),
            FloorStatus::Ok(Semver::new(1, 2, 0))
        );
        assert_eq!(
            floor_status(&spec, missing, Some("1.0.0")),
            FloorStatus::BelowFloor {
                installed: Semver::new(1, 0, 0),
                required: Semver::new(1, 2, 0),
            }
        );
        // #701: the OLD binary predates --version; live None + probe fails.
        assert_eq!(
            floor_status(&spec, missing, None),
            FloorStatus::Unreadable {
                required: Semver::new(1, 2, 0)
            }
        );
    }

    #[test]
    fn unreadable_with_no_floor_is_allowed() {
        let spec = FakeSpec { min: Semver::ZERO };
        let missing = Path::new("/nonexistent/tool-xyz");
        assert_eq!(
            floor_status(&spec, missing, None),
            FloorStatus::UnreadableNoFloor
        );
        assert!(floor_status(&spec, missing, None).is_usable());
    }

    /// RFC-025 §5.5 honesty test (a): every catalog entry's offline
    /// `version_fallback` must parse and be `>= min_version`. This makes a fresh
    /// *offline* install (latest fetch fails -> falls back) unable to land below
    /// the floor by construction - the latent #701-on-a-new-machine bug.
    #[test]
    fn every_catalog_fallback_is_at_or_above_its_floor() {
        fn check(spec: &dyn SidecarSpec) {
            let fb = Semver::parse(spec.version_fallback()).unwrap_or_else(|| {
                panic!(
                    "{}: version_fallback '{}' does not parse as X.Y.Z",
                    spec.install_name(),
                    spec.version_fallback()
                )
            });
            assert!(
                fb >= spec.min_version(),
                "{}: version_fallback {} is below the floor {} - a fresh offline install would land under the floor",
                spec.install_name(),
                fb,
                spec.min_version(),
            );
        }

        for b in crate::embed_catalog::backends() {
            check(b);
        }
        for e in crate::phase_b::catalog::CATALOG {
            check(e);
            match &e.scip_install {
                crate::phase_b::catalog::ScipInstall::GithubBinary(s) => check(s),
                crate::phase_b::catalog::ScipInstall::ZipBinary(z) => check(z),
                _ => {}
            }
        }
    }

    /// RFC-025 §7.2: a below-floor sidecar spawned repeatedly by the ~5s
    /// scheduler must log exactly one WARN per distinct `(install_name,
    /// installed_version)`; a reinstall that changes the version is a new
    /// transition.
    #[test]
    fn below_floor_logs_once_then_again_on_change() {
        let name = "dedup-test-embed"; // unique key so the global map is not shared
        let v100 = Semver::new(1, 0, 0);
        let v110 = Semver::new(1, 1, 0);
        // First observation logs; a storm of identical spawns does not.
        assert!(note_below_floor_transition(name, v100));
        assert!(!note_below_floor_transition(name, v100));
        assert!(!note_below_floor_transition(name, v100));
        // A reinstall to a different (still below-floor) version is a new transition.
        assert!(note_below_floor_transition(name, v110));
        assert!(!note_below_floor_transition(name, v110));
    }

    #[test]
    fn stale_logs_once_per_pair() {
        let name = "dedup-test-stale";
        let installed = Semver::new(1, 2, 0);
        let latest = Semver::new(1, 3, 0);
        assert!(note_stale_transition(name, installed, latest));
        assert!(!note_stale_transition(name, installed, latest));
        // A newer latest is a new advisory.
        assert!(note_stale_transition(name, installed, Semver::new(1, 4, 0)));
    }

    #[test]
    fn cache_roundtrips_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".sidecar-latest.json");
        let name = "cache-test-tool";
        let t0 = 1_000_000u64;

        // Cold cache -> None.
        assert_eq!(read_cached_latest_at(&path, name, t0), None);
        // Warm it, then read within TTL.
        write_cached_latest_at(&path, name, "v1.3.0", t0);
        assert_eq!(
            read_cached_latest_at(&path, name, t0 + 60),
            Some(Semver::new(1, 3, 0))
        );
        // Past the 24h TTL -> cold again (correctness axis unaffected upstream).
        assert_eq!(
            read_cached_latest_at(&path, name, t0 + LATEST_TTL_SECS + 1),
            None
        );
        // A missing binary key stays cold.
        assert_eq!(read_cached_latest_at(&path, "other", t0), None);
    }

    #[test]
    fn below_floor_message_shape() {
        assert_eq!(
            below_floor_message(
                "travsr-embed",
                &Semver::new(1, 0, 0),
                &Semver::new(1, 2, 0),
                "travsr embed init --reinstall"
            ),
            "travsr-embed v1.0.0 is below the required v1.2.0 - run: travsr embed init --reinstall"
        );
    }
}
