//! RFC-025 §8: the `sidecars:` health block shown by `travsr status`,
//! `travsr embed status`, and `travsr lang list`.
//!
//! `installed` and `required` come from the offline path (`installed_version` +
//! `min_version`); `latest` is read from the 24h TTL cache and omitted when the
//! cache is cold/offline. This makes the compatibility-floor state legible
//! without reading daemon logs and always carries the exact remedy. It is
//! product-facing, so it names commands, never internal identifiers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use travsr_plugin_host::sidecar_version::{
    floor_status, read_cached_latest, FloorStatus, Semver, SidecarSpec,
};

/// One installed sidecar's version health.
pub struct SidecarHealth {
    pub install_name: String,
    pub status: FloorStatus,
    /// Latest release from the local cache, or `None` when the cache is cold.
    pub latest: Option<Semver>,
    /// Exact command that replaces the binary (e.g. `travsr embed init --reinstall`).
    pub remedy: String,
}

fn home_bin_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".travsr").join("bin"))
}

fn health_for(spec: &dyn SidecarSpec, bin_path: &Path, remedy: String) -> SidecarHealth {
    SidecarHealth {
        install_name: spec.install_name().to_string(),
        status: floor_status(spec, bin_path, None),
        latest: read_cached_latest(spec.install_name()),
        remedy,
    }
}

fn resolve_tool_path(bin_dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = bin_dir.join(name);
    if direct.exists() {
        return Some(direct);
    }
    travsr_core::exec::resolve_executable(name)
}

/// One installed sidecar the caller intends to probe: everything the bounded
/// `--version` probe needs, so the probes can run without borrowing shared
/// state. The spec is `'static` catalog data and `Sync`, so it crosses into a
/// scoped probe thread freely.
struct Pending {
    spec: &'static (dyn SidecarSpec + Sync),
    path: PathBuf,
    remedy: String,
}

/// Gather health for every installed sidecar (the embed binary plus the Phase B
/// underlying `scip-*` / zip tools), de-duplicated by `install_name`. Only
/// binaries actually present on disk are returned.
///
/// Each `--version` probe carries its own bounded deadline (`floor_status`), so
/// they are run concurrently rather than back to back: a status command must not
/// stall for the *sum* of every probe's timeout when several tools hang on
/// `--version`, only for the single slowest one. `floor_status` is pure (it
/// holds no shared logging/de-dup state), so the probes are independent.
pub fn gather() -> Vec<SidecarHealth> {
    let Some(bin_dir) = home_bin_dir() else {
        return Vec::new();
    };

    // Pass 1 (sequential, no I/O beyond `exists()`): resolve which sidecars to
    // probe, de-duplicated by install name.
    let mut pending: Vec<Pending> = Vec::new();
    let mut seen = HashSet::new();

    // Embed: every catalog entry shares the one `travsr-embed` binary.
    for b in travsr_plugin_host::embed_backends() {
        if !seen.insert(b.install_name().to_string()) {
            continue;
        }
        let path = bin_dir.join(b.binary_filename());
        if path.exists() {
            pending.push(Pending {
                spec: b,
                path,
                remedy: "travsr embed init --reinstall".to_string(),
            });
        }
    }

    // Phase B: the underlying scip-* / zip tools, one per distinct install_name.
    for e in travsr_plugin_host::phase_b::catalog::CATALOG {
        use travsr_plugin_host::phase_b::catalog::ScipInstall;
        let (spec, install_name): (&'static (dyn SidecarSpec + Sync), &str) = match &e.scip_install
        {
            ScipInstall::GithubBinary(s) => (s, s.install_name),
            ScipInstall::ZipBinary(z) => (z, z.install_name),
            _ => continue,
        };
        if !seen.insert(install_name.to_string()) {
            continue;
        }
        if let Some(path) = resolve_tool_path(&bin_dir, install_name) {
            pending.push(Pending {
                spec,
                path,
                remedy: format!("travsr lang install {} --reinstall", e.language),
            });
        }
    }

    // Pass 2 (concurrent): each sidecar's bounded `--version` probe, run in its
    // own scoped thread. Order is preserved so the rendered block is stable.
    std::thread::scope(|scope| {
        let handles: Vec<_> = pending
            .iter()
            .map(|p| scope.spawn(|| health_for(p.spec, &p.path, p.remedy.clone())))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("sidecar health probe thread panicked"))
            .collect()
    })
}

/// Render one health line. Examples:
/// ```text
///   travsr-embed    1.0.0  below required 1.2.0  - run: travsr embed init --reinstall
///   scip-java       0.12.3 ok  (newer 0.12.4 available)
/// ```
pub fn render_line(h: &SidecarHealth) -> String {
    let name = &h.install_name;
    match &h.status {
        FloorStatus::Ok(installed) => {
            let note = match &h.latest {
                Some(l) if *l > *installed => format!("  (newer {l} available)"),
                _ => String::new(),
            };
            format!("  {name:<15} {installed}  ok{note}")
        }
        FloorStatus::BelowFloor {
            installed,
            required,
        } => {
            format!(
                "  {name:<15} {installed}  below required {required}  - run: {}",
                h.remedy
            )
        }
        FloorStatus::Unreadable { required } => {
            format!(
                "  {name:<15} unknown  cannot confirm >= {required}  - run: {}",
                h.remedy
            )
        }
        FloorStatus::UnreadableNoFloor => {
            format!("  {name:<15} unknown  version not reported")
        }
        FloorStatus::ProbeTimeout { required } => {
            format!(
                "  {name:<15} unknown  version probe timed out (usable, cannot confirm >= {required})"
            )
        }
    }
}

/// Print the `sidecars:` block, or nothing when no sidecar is worth reporting.
/// Shared by `travsr status`, `travsr embed status`, and `travsr lang list`.
///
/// Tools with no declared floor whose version simply cannot be read (many Phase
/// B wrappers and emitters do not answer `--version`) carry nothing actionable,
/// so they are omitted to keep the block signal-dense: a line appears only when
/// it states a real version, a floor problem, or a staleness advisory.
pub fn print_block() {
    let items: Vec<SidecarHealth> = gather()
        .into_iter()
        .filter(|h| !matches!(h.status, FloorStatus::UnreadableNoFloor))
        .collect();
    if items.is_empty() {
        return;
    }
    println!("sidecars:");
    for h in &items {
        println!("{}", render_line(h));
    }
}
