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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

// ── Installed-version reader (offline, bounded) ─────────────────────────────

/// How long a `<bin> --version` probe may run before the watchdog kills it.
///
/// The probe sits on `resolve_backend`, which the daemon runs every ~5s, so it
/// cannot be allowed to block forever: a sidecar that hangs on `--version`
/// (macOS Gatekeeper verifying a freshly-downloaded binary is the realistic
/// case, a wedged process the pathological one) would otherwise stall the
/// daemon scheduler and the foreground `travsr status` / `embed status` /
/// `lang list`. Three seconds is generous for a first-exec verification yet well
/// under the scheduler cadence; a healthy `--version` returns in milliseconds
/// and is never penalised (the watchdog is notified the instant the child
/// exits). This mirrors the crate's existing subprocess-watchdog discipline
/// (`watchdog::with_io_watchdog`, `probe_top1_cosines`).
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Outcome of a bounded `<bin> --version` probe.
enum ProbeOutcome {
    /// The probe exited cleanly and its output carried a parseable `X.Y.Z`.
    Parsed(Semver),
    /// The probe ran to completion but produced no parseable version (an ancient
    /// binary that predates `--version`, a non-zero exit, or a spawn failure).
    Unreadable,
    /// The probe did not finish within [`VERSION_PROBE_TIMEOUT`] and was killed.
    /// Distinct from `Unreadable` on purpose: a timeout is a transient/infra
    /// condition (slow first exec), not evidence that the binary is old, so the
    /// caller degrades rather than hard-refusing (see [`FloorStatus`]).
    TimedOut,
}

/// Run `<bin> --version` with a hard deadline. Spawns the child, waits up to
/// `timeout` on a `Condvar`, and kills it on expiry (via the shared
/// [`crate::watchdog::kill_pid`]). A healthy probe returns the instant the child
/// exits — the watchdog is notified immediately, so a fast `--version` is never
/// slowed to the full `timeout` (a bare `sleep(timeout)` would penalise every
/// call). 100% local, no network.
///
/// The critical section is bounded on the child's **exit**, not on stdout EOF:
/// stdout is drained on a detached thread, and the main path guards
/// [`std::process::Child::wait`], which returns as soon as the direct child
/// dies (killed → bounded) regardless of any grandchild still holding the pipe
/// open. Using `wait_with_output` here would reintroduce the very unbounded
/// block this exists to prevent, since a lingering grandchild keeps the pipe's
/// write end open past the deadline.
fn probe_version_bounded(bin: &Path, timeout: Duration) -> ProbeOutcome {
    let mut child = match Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return ProbeOutcome::Unreadable, // missing / not executable
    };
    let pid = child.id();

    // Drain stdout off the critical path so nothing holding the pipe open can
    // block us; we bound the child's exit, not the pipe's EOF.
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });

    let state = Arc::new((Mutex::new(false), Condvar::new()));
    let killed = Arc::new(AtomicBool::new(false));
    let state_wg = Arc::clone(&state);
    let killed_wg = Arc::clone(&killed);
    let watchdog = std::thread::spawn(move || {
        let (lock, cv) = &*state_wg;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (done, res) = cv
            .wait_timeout_while(guard, timeout, |d| !*d)
            .unwrap_or_else(|e| e.into_inner());
        if res.timed_out() && !*done {
            killed_wg.store(true, Ordering::SeqCst);
            crate::watchdog::kill_pid(pid);
        }
    });

    let status = child.wait();

    let (lock, cv) = &*state;
    *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
    cv.notify_all();
    drop(watchdog.join());

    if killed.load(Ordering::SeqCst) {
        return ProbeOutcome::TimedOut;
    }
    match status {
        Ok(s) if s.success() => {
            // The child exited on its own, so its stdout is closed and the
            // reader has finished; a short bounded recv keeps this defensive
            // against a self-exited child that left a grandchild on the pipe.
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(buf) => match parse_sidecar_version(&String::from_utf8_lossy(&buf)) {
                    Some(v) => ProbeOutcome::Parsed(v),
                    None => ProbeOutcome::Unreadable,
                },
                Err(_) => ProbeOutcome::Unreadable,
            }
        }
        _ => ProbeOutcome::Unreadable,
    }
}

/// Read a sidecar's version, preferring the negotiated handshake and falling
/// back to a bounded `--version` probe. Strictly local, no network.
///
/// Resolution order (RFC-025 §5.2):
/// 1. An already-negotiated handshake `plugin_version` (`live`) — for the embed
///    sidecar this is live at zero cost once spawned, and skips the probe (and
///    its timeout) entirely.
/// 2. Else a bounded `<bin> --version` probe ([`probe_version_bounded`]).
fn read_version(bin: &Path, live: Option<&str>) -> ProbeOutcome {
    if let Some(v) = live {
        if let Some(s) = parse_sidecar_version(v) {
            return ProbeOutcome::Parsed(s);
        }
    }
    // #712: some tools do not report a usable version from `--version`. scip-java
    // is a coursier launcher whose `--version` prints `0.0.0` even when a specific
    // tagged release was installed (the version is only in the downloaded asset
    // name, `scip-java-<tag>`). `travsr lang install` records the resolved version
    // in a `<bin>.version` sidecar file; prefer it so status shows the real one.
    if let Some(s) = read_version_file(bin) {
        return ProbeOutcome::Parsed(s);
    }
    match probe_version_bounded(bin, VERSION_PROBE_TIMEOUT) {
        // A probed `0.0.0` is the "unset" sentinel these launchers emit, never a
        // real release. Report it as unreadable rather than a bogus `0.0.0 ok`
        // that misrepresents an installed tool (#712).
        ProbeOutcome::Parsed(s) if s == Semver::new(0, 0, 0) => ProbeOutcome::Unreadable,
        other => other,
    }
}

/// #712: read a `<bin>.version` sidecar file written by `travsr lang install`
/// (and the embed installer), holding the resolved release tag for a tool whose
/// own `--version` is unreliable. Returns the parsed version, or `None` when the
/// file is absent or unparseable.
fn read_version_file(bin: &Path) -> Option<Semver> {
    let name = bin.file_name()?;
    let mut fname = name.to_os_string();
    fname.push(".version");
    let path = bin.with_file_name(fname);
    let text = std::fs::read_to_string(path).ok()?;
    parse_sidecar_version(&text)
}

/// #712: the path of the `<bin>.version` sidecar file for a given installed
/// binary. Exposed so the installer writes the exact file `read_version_file`
/// reads. Returns `None` only for a path with no file name.
pub fn version_sidecar_path(bin: &Path) -> Option<std::path::PathBuf> {
    let name = bin.file_name()?;
    let mut fname = name.to_os_string();
    fname.push(".version");
    Some(bin.with_file_name(fname))
}

/// Read the installed version of a sidecar binary. Strictly local, no network,
/// and bounded (see [`VERSION_PROBE_TIMEOUT`]).
///
/// Returns `None` when neither the handshake nor the probe yields a parseable
/// `X.Y.Z` — an ancient binary that predates `--version`, one whose probe exits
/// non-zero, or one whose probe timed out. Callers that need to distinguish a
/// timeout from an unreadable version use [`floor_status`] instead; this reader
/// serves the advisory / feature-detection sites, where all three collapse to
/// "no usable version".
pub fn installed_version(bin: &Path, live: Option<&str>) -> Option<Semver> {
    match read_version(bin, live) {
        ProbeOutcome::Parsed(s) => Some(s),
        ProbeOutcome::Unreadable | ProbeOutcome::TimedOut => None,
    }
}

// ── Point A: the compatibility floor ────────────────────────────────────────

/// Outcome of the offline floor check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorStatus {
    /// Installed version satisfies the declared floor.
    Ok(Semver),
    /// Installed version is below the required floor — hard refuse with remedy.
    BelowFloor { installed: Semver, required: Semver },
    /// Version could not be read (probe exited non-zero / unparseable) AND a
    /// floor is declared: compliance cannot be proven, so this is treated
    /// conservatively as a refusal (§9).
    Unreadable { required: Semver },
    /// Version could not be read but no floor is declared (`min_version` ==
    /// `ZERO`): the floor is trivially met, so this is allowed (§9). Also covers
    /// a probe timeout with no floor, which likewise cannot violate anything.
    UnreadableNoFloor,
    /// The `--version` probe did not finish within [`VERSION_PROBE_TIMEOUT`] and
    /// a floor is declared. A timeout is transient/infra (a slow first exec, e.g.
    /// macOS Gatekeeper verifying a freshly-downloaded binary), not proof the
    /// binary is below the floor — so this **degrades to usable** rather than
    /// hard-refusing, and the real work downstream is already bounded by its own
    /// per-call watchdog (`watchdog::with_io_watchdog`). Distinct from
    /// `Unreadable` so the timeout case is explicit rather than inherited (§9).
    ProbeTimeout { required: Semver },
}

impl FloorStatus {
    /// True when the sidecar may be used: floor satisfied, no floor declared, or
    /// a probe timeout (a transient condition that is not evidence of an old
    /// binary — see [`FloorStatus::ProbeTimeout`]).
    pub fn is_usable(&self) -> bool {
        matches!(
            self,
            FloorStatus::Ok(_) | FloorStatus::UnreadableNoFloor | FloorStatus::ProbeTimeout { .. }
        )
    }
}

/// Compare a binary's installed version against its spec's declared floor.
/// 100% offline and bounded: reads the handshake `live` value or a local,
/// timeout-guarded `--version` probe.
pub fn floor_status(spec: &dyn SidecarSpec, bin: &Path, live: Option<&str>) -> FloorStatus {
    let required = spec.min_version();
    match read_version(bin, live) {
        ProbeOutcome::Parsed(v) if v >= required => FloorStatus::Ok(v),
        ProbeOutcome::Parsed(v) => FloorStatus::BelowFloor {
            installed: v,
            required,
        },
        _ if required == Semver::ZERO => FloorStatus::UnreadableNoFloor,
        ProbeOutcome::TimedOut => FloorStatus::ProbeTimeout { required },
        ProbeOutcome::Unreadable => FloorStatus::Unreadable { required },
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

/// Names already WARN-logged for a version-less event (`unreadable`,
/// `probe_timeout`), keyed on `(kind, install_name)`. These states carry no
/// version to key on, so they de-dup on the name alone.
fn name_only_dedup() -> &'static Mutex<HashSet<(&'static str, String)>> {
    static M: OnceLock<Mutex<HashSet<(&'static str, String)>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Transition gate for the version-less WARN events. Returns `true` at most once
/// per `(kind, install_name)`, so an unparseable-or-timed-out sidecar refused on
/// every ~5s scheduler tick logs exactly one WARN line — the same
/// transition-only contract the `below_floor`/`stale` events already honour
/// (RFC-025 §7.2). Without this, `sidecar.version.unreadable` was the one
/// repeating event with no de-dup.
fn note_name_transition(kind: &'static str, install_name: &str) -> bool {
    let mut set = name_only_dedup().lock().unwrap_or_else(|e| e.into_inner());
    set.insert((kind, install_name.to_string()))
}

/// Run the Point A floor check for a spawn/resolve, emit the stable structured
/// log events (RFC-025 §7.1), and return the [`FloorStatus`] so the caller can
/// refuse (CLI) or skip (daemon).
///
/// - `sidecar.version.checked` (DEBUG) fires every call so healthy spawns do not
///   flood at info.
/// - `sidecar.version.below_floor` (WARN), `sidecar.version.stale` (INFO),
///   `sidecar.version.unreadable` (WARN) and `sidecar.version.probe_timeout`
///   (WARN) are all **transition-only**: at most one line per distinct value.
///   The two version-less events de-dup on the name alone.
/// - An unreadable version with *no floor declared* is a permitted state
///   (`is_usable()` is true), so it is logged at DEBUG next to `checked`, not
///   WARN — a warning that fires forever on an allowed state trains readers to
///   ignore warnings.
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
        FloorStatus::Unreadable { .. } => ("unreadable".to_string(), "unreadable"),
        // A permitted state (no floor to violate). It rides the single DEBUG
        // `checked` line below with its own disposition — no separate WARN, since
        // a warning that fires forever on an allowed state trains readers to
        // ignore warnings.
        FloorStatus::UnreadableNoFloor => ("unreadable".to_string(), "unreadable_no_floor"),
        FloorStatus::ProbeTimeout { .. } => ("timeout".to_string(), "probe_timeout"),
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
            if note_name_transition("unreadable", install_name) {
                tracing::warn!(
                    event = "sidecar.version.unreadable",
                    install_name,
                    reason = "version output could not be parsed and a floor is declared",
                    required = %required,
                    "sidecar version could not be read"
                );
            }
        }
        FloorStatus::ProbeTimeout { required } => {
            if note_name_transition("probe_timeout", install_name) {
                tracing::warn!(
                    event = "sidecar.version.probe_timeout",
                    install_name,
                    required = %required,
                    timeout_secs = VERSION_PROBE_TIMEOUT.as_secs(),
                    "sidecar --version probe timed out; degrading to usable (transient)"
                );
            }
        }
        // Permitted state: nothing to refuse and no action for the reader, so it
        // carries no WARN — it is already recorded by the DEBUG `checked` line
        // above (disposition `unreadable_no_floor`).
        FloorStatus::UnreadableNoFloor => {}
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

    /// The handshake `live` value short-circuits the probe entirely, so a
    /// wedged binary never even gets exec'd when a version is already known.
    #[test]
    fn live_handshake_skips_the_probe() {
        let spec = FakeSpec {
            min: Semver::new(1, 2, 0),
        };
        // Path does not exist, but `live` is present -> no probe, no I/O.
        let missing = Path::new("/nonexistent/travsr-embed-xyz");
        assert_eq!(
            floor_status(&spec, missing, Some("travsr-embed 1.3.0")),
            FloorStatus::Ok(Semver::new(1, 3, 0))
        );
    }

    /// A probe timeout is a transient/infra condition, not evidence of an old
    /// binary, so it degrades to usable rather than hard-refusing.
    #[test]
    fn probe_timeout_status_is_usable() {
        let s = FloorStatus::ProbeTimeout {
            required: Semver::new(1, 2, 0),
        };
        assert!(s.is_usable());
    }

    /// The version-less WARN events (`unreadable`, `probe_timeout`) de-dup on the
    /// install name alone, and the two kinds are independent keys.
    #[test]
    fn name_only_events_log_once_per_kind_and_name() {
        let name = "dedup-name-only-tool"; // unique key: the global set is shared
        assert!(note_name_transition("unreadable", name));
        assert!(!note_name_transition("unreadable", name));
        // A different kind for the same name is a distinct key.
        assert!(note_name_transition("probe_timeout", name));
        assert!(!note_name_transition("probe_timeout", name));
        // A different name under the same kind is distinct too.
        assert!(note_name_transition("unreadable", "other-tool"));
    }

    /// The `--version` probe is bounded: a binary that hangs on `--version` is
    /// killed at the deadline and reported as `TimedOut`, in well under the time
    /// it would take to run to completion. Regression for the review finding that
    /// `floor_status` sat unbounded on the daemon's ~5s path.
    ///
    /// Unix-only: relies on a shell script and `kill`-style termination, exactly
    /// like `watchdog::watchdog_kills_hung_child_and_unblocks_read`.
    #[cfg(unix)]
    #[test]
    fn probe_version_is_bounded_and_reports_timeout() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::time::Instant;

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("hang-on-version");
        {
            let mut f = std::fs::File::create(&bin).unwrap();
            // Ignores its args and blocks far longer than the probe deadline.
            writeln!(f, "#!/bin/sh\nsleep 30").unwrap();
        }
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let start = Instant::now();
        let outcome = probe_version_bounded(&bin, Duration::from_millis(500));
        let elapsed = start.elapsed();

        assert!(
            matches!(outcome, ProbeOutcome::TimedOut),
            "a hung probe must report TimedOut"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "probe must be bounded by its deadline, took {elapsed:?}"
        );
    }

    /// A healthy `--version` is read promptly and is not penalised the full
    /// deadline — the watchdog is notified the instant the child exits.
    #[cfg(unix)]
    #[test]
    fn probe_version_reads_a_fast_healthy_version() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::time::Instant;

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fast-version");
        {
            let mut f = std::fs::File::create(&bin).unwrap();
            writeln!(f, "#!/bin/sh\necho 'fake-sidecar 1.4.2'").unwrap();
        }
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let start = Instant::now();
        let outcome = probe_version_bounded(&bin, Duration::from_secs(30));
        assert!(
            matches!(outcome, ProbeOutcome::Parsed(v) if v == Semver::new(1, 4, 2)),
            "a healthy --version must parse"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "a healthy probe must not wait the full deadline"
        );
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
