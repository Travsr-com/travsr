#![deny(unsafe_code)] // overridden only in sandbox/windows/ffi.rs (ADR-017 Amendment A2)
//! travsr-plugin-host — owns the trust boundary between the daemon and plugins.
//!
//! The ONLY crate in the indexer tier that depends on travsr-plugin-protocol.
//! Neither travsr-indexer nor travsr-ingest reference it directly (CLAUDE.md).

pub mod cache;
pub mod dispatcher;
pub mod embed_catalog;
pub mod embed_sidecar;
pub mod embed_supervisor;
pub mod governance;
pub mod indexer;
pub mod phase_b;
pub mod plugins;
pub mod registry;
pub mod resolver;
pub mod resource_limits;
pub mod sandbox;
pub mod sidecar_version;
mod stderr_ring;
pub mod supervisor;
pub mod transport;
pub mod trust;
pub mod watchdog;

pub use dispatcher::Dispatcher;
pub use embed_catalog::{
    active_backend_id, backends as embed_backends, cancel_sentinel_path,
    derive_num_workers_for_cli, derive_phase1_threshold_for_status, embed_paused,
    embed_reindex_in_flight, embeddings_by_model, ensure_reindex_backend_ready, gc_embeddings,
    lookup as lookup_embed_backend, pause_embed, probe_top1_cosines, repo_backend_id,
    resolve_governance_for_db, resume_embed, run_parallel_reindex_blocking,
    run_parallel_reindex_blocking_quiet, spawn_background_reindex_all,
    spawn_background_reindex_phase1, spawn_background_reindex_phase2, terminate_inflight_reindex,
    write_model_descriptor, write_repo_backend_id, EmbedBackend, EmbedModelFile, EmbedOpLock,
    ModelUsage, MAX_EMBED_WORKERS,
};
pub use embed_sidecar::{EmbedCapabilities, EmbedError, EmbedSidecar};
pub use embed_supervisor::{EmbedQueryHook, EmbedSupervisor};
pub use governance::{Capacity, EmbedGovernance, EmbedOverrides, Priority};
pub use indexer::{PhaseBInputs, PhaseBLiveness, PhaseBOutcome, PluginIndexer};
pub use phase_b::{
    lookup as lookup_phase_b, OutputFormat, PhaseBEntry, SandboxRequirement,
    CATALOG as PHASE_B_CATALOG,
};
pub use registry::probe_sandbox;
pub use sandbox::policy::{SandboxPolicy, SandboxUnavailable};
pub use sidecar_version::{
    below_floor_message, floor_status, installed_version, read_cached_latest, unreadable_message,
    write_cached_latest, FloorStatus, Semver, SidecarSpec,
};
pub use transport::{InProcess, PluginHealth, Sidecar, Transport};

/// Native Windows process-liveness probe (`OpenProcess` +
/// `GetExitCodeProcess`, no signal sent), for callers outside this crate
/// that need it and cannot themselves hold `unsafe` (`travsr-mcp` is
/// `#![forbid(unsafe_code)]`).
///
/// Delegates to [`sandbox::windows::pid_alive`], the same probe
/// `embed_catalog::pid_liveness` uses for the daemon shutdown grace
/// poll; `unsafe` stays confined to `sandbox/windows/ffi.rs` per ADR-017
/// Amendment A2 Invariant 1, this is a safe wrapper only.
///
/// #636 round-2 review: `travsr-mcp`'s `observability::pid_is_alive`
/// previously shelled out to `tasklist` on every call, which measurably
/// failed under the process-spawn contention a full `cargo test --workspace`
/// run puts on Windows CI (`CreateProcess` is comparatively expensive under
/// load there). A syscall has no such failure mode. See
/// [`unix_pid_is_alive`] for the Unix counterpart.
#[cfg(target_os = "windows")]
pub fn windows_pid_is_alive(pid: u32) -> bool {
    sandbox::windows::pid_alive(pid)
}

/// Native Unix process-liveness probe: `kill(pid, 0)`, which sends no signal
/// and only performs the permission-and-existence check. Unix counterpart to
/// [`windows_pid_is_alive`], living here for the same reason, so callers that
/// are `#![forbid(unsafe_code)]` (`travsr-mcp`) still get a syscall rather
/// than a subprocess.
///
/// **`EPERM` means alive.** This is the correctness point, not an
/// optimisation (#636 round-3 review; filed independently as #759). `kill -0`
/// as a shell command collapses `EPERM` and `ESRCH` into the same non-zero
/// exit status, so the previous subprocess implementation reported any live
/// process the calling user cannot signal as *dead*:
///
/// ```text
/// $ ps -p 1 -o pid,user,comm
///     1 root systemd
/// $ kill -0 1 ; echo $?
/// kill: (1) - Operation not permitted
/// 1
/// ```
///
/// A daemon started under a different uid (sudo, a shared machine, a
/// container where the MCP server runs as another user) read as down. That
/// fails in the *unsafe* direction for a status probe, unlike the
/// recycled-PID case, which merely reports a daemon one poll too long.
/// Distinguishing the two errno values is the whole fix: `EPERM` proves the
/// process exists (the kernel had something to refuse permission *on*),
/// `ESRCH` proves it does not.
///
/// Uses `nix`'s safe wrapper rather than `libc::kill` deliberately: a raw
/// call needs an `unsafe` block, and ADR-017 Amendment A2 Invariant 1 allows
/// exactly one override site in this crate, noting that "any second override
/// site re-opens this amendment". `unsafe` stays confined to
/// `sandbox/windows/ffi.rs`.
#[cfg(unix)]
/// 0 is defined as not alive, and so is any value too large for `pid_t`.
/// External callers get "is this a live process" and "is this a process id at
/// all" folded into the same `false`, which is what every current consumer
/// wants; a caller that needs to tell them apart has to check the value itself.
pub fn unix_pid_is_alive(pid: u32) -> bool {
    // A PID that does not fit in i32 cannot name a real process on any Unix.
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    // 0 is not a process id here, it is "every process in my process group", so
    // `kill(0, None)` always succeeds and 0 would read as alive. Nothing we write
    // ever stores it (`std::process::id()` cannot return 0), so a 0 in the info
    // file means the file is corrupt, truncated or externally written, and the
    // one thing it must not do is name a live holder.
    //
    // Only 0 needs excluding. `raw` comes from `i32::try_from(pid)` on a `u32`,
    // which succeeds only for a non-negative value, so a negative `raw` cannot
    // reach here and the guard does not pretend to handle one.
    if raw == 0 {
        return false;
    }
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None) {
        Ok(()) => true,
        // The process exists; this user just may not signal it.
        Err(nix::errno::Errno::EPERM) => true,
        // ESRCH (no such process) and anything else: treat as not alive.
        Err(_) => false,
    }
}

#[cfg(all(test, unix))]
mod unix_pid_tests {
    /// #745 review: `kill(0, None)` addresses every process in the caller's
    /// process group rather than a process named 0, so the permission check
    /// trivially succeeds and 0 read as alive. That fed the one input still
    /// reaching the outcome this PR set out to remove: a pid named as the
    /// holder without having been established as one.
    #[test]
    fn pid_zero_and_negatives_are_never_alive() {
        assert!(
            !super::unix_pid_is_alive(0),
            "0 is a process group, not a process, and must not read as a holder"
        );
        // i32::MAX + 1 does not fit in a pid_t; anything past it is the same class.
        assert!(!super::unix_pid_is_alive(u32::MAX));
    }

    /// The regression the round-3 review reported: PID 1 is always alive and
    /// (outside a root-run container) never signallable by the test user, so
    /// it is the canonical `EPERM`-means-alive case. Skips itself when the
    /// suite happens to run as root, where the call returns `Ok` instead and
    /// the assertion would pass without exercising the `EPERM` arm at all.
    ///
    /// #759: this is the regression test for that issue. Naming it here so the
    /// issue is findable by number; the report and this test are the same bug.
    #[test]
    fn pid_one_reads_as_alive_even_when_not_signallable() {
        // The skip the comment above promises, which until now was only
        // promised: root may signal PID 1, so `kill` returns Ok, the assert
        // passes, and the EPERM arm is never reached. Container and
        // self-hosted runners are routinely root, so without this the
        // coverage this test advertises is absent exactly where it is
        // least visible (#768 review).
        if nix::unistd::Uid::effective().is_root() {
            return;
        }
        assert!(
            super::unix_pid_is_alive(1),
            "PID 1 is alive; EPERM must not be read as dead"
        );
    }

    #[test]
    fn own_pid_is_alive_and_an_impossible_pid_is_not() {
        assert!(super::unix_pid_is_alive(std::process::id()));
        // u32::MAX - 1 is above every platform's pid_max, so it cannot exist.
        assert!(!super::unix_pid_is_alive(u32::MAX - 1));
    }
}

/// Formal plugin state — reported at startup and queryable by the daemon.
/// ADR-017 Rule 2: `Disabled` means no subprocess runs, not "degraded mode".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    /// Plugin is registered, sandbox available, tool on PATH — ready.
    Active,
    /// Sandbox mechanism absent on this host — fail-closed per ADR-017 Rule 2.
    SandboxUnavailableDisabled { mechanism: &'static str },
    /// Phase B tool not on PATH — registered but inactive until installed.
    ToolNotFound { command: String },
    /// Language not registered via `travsr lang add`.
    NotRegistered,
}
