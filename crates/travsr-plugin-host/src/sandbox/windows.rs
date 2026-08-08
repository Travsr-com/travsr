//! Windows AppContainer + Job Object sandbox (ADR-017 Rules 1-2).
//! Safe wrapper — no `unsafe` code in this file; all unsafe is in `ffi.rs`.

mod ffi;

/// #500: process-liveness probe for the embed sidecar's shutdown grace poll.
/// ADR-017 A2 Invariant 1: the sizing probes live here too, so every unsafe
/// block in this crate stays confined to ffi.rs.
pub(crate) use ffi::{available_physical_memory_mb, pid_alive, windows_p_core_count};

use crate::sandbox::policy::{SandboxPolicy, SandboxUnavailable};
use crate::sandbox::StdioCfg;
use std::io;
use std::path::PathBuf;

fn profile_name(repo_root: &std::path::Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    repo_root.hash(&mut h);
    format!("travsr-{:016x}", h.finish())
}

fn to_mode(cfg: StdioCfg) -> ffi::StdioMode {
    match cfg {
        StdioCfg::Pipe => ffi::StdioMode::Pipe,
        StdioCfg::Null => ffi::StdioMode::Null,
        StdioCfg::Inherit => ffi::StdioMode::Inherit,
    }
}

// ── AppContainerChild ─────────────────────────────────────────────────────────

/// Live AppContainer child process. All handles owned here; `_job` keeps the
/// Job Object alive so `KILL_ON_JOB_CLOSE` fires on drop.
pub struct AppContainerChild {
    process: ffi::OwnedHandle,
    _job: ffi::OwnedJobHandle,
    pid: u32,
    stdin_write: Option<ffi::OwnedHandle>,
    stdout_read: Option<ffi::OwnedHandle>,
    stderr_read: Option<ffi::OwnedHandle>,
}

impl AppContainerChild {
    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn kill(&mut self) -> io::Result<()> {
        ffi::terminate_process(self.process.as_handle())
    }

    pub fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        let code = ffi::wait_for_process(self.process.as_handle())?;
        // ExitStatusExt::from_raw is stable since Rust 1.72; workspace requires 1.75+.
        use std::os::windows::process::ExitStatusExt;
        Ok(std::process::ExitStatus::from_raw(code))
    }

    pub fn wait_with_output(mut self) -> io::Result<std::process::Output> {
        // Close parent's stdin write end so the child sees EOF.
        drop(self.stdin_write.take());

        // Read stdout and stderr concurrently to prevent deadlock on full pipe buffers.
        let stdout_handle = self.stdout_read.take();
        let stderr_handle = self.stderr_read.take();

        let stdout_thread = stdout_handle.map(|h| {
            std::thread::spawn(move || -> io::Result<Vec<u8>> {
                use std::io::Read;
                let mut f = ffi::handle_into_read_file(h);
                let mut buf = Vec::new();
                f.read_to_end(&mut buf)?;
                Ok(buf)
            })
        });

        let stderr_thread = stderr_handle.map(|h| {
            std::thread::spawn(move || -> io::Result<Vec<u8>> {
                use std::io::Read;
                let mut f = ffi::handle_into_read_file(h);
                let mut buf = Vec::new();
                f.read_to_end(&mut buf)?;
                Ok(buf)
            })
        });

        let status = self.wait()?;

        let stdout = stdout_thread
            .map(|t| t.join().unwrap_or_else(|_| Ok(Vec::new())))
            .transpose()?
            .unwrap_or_default();

        let stderr = stderr_thread
            .map(|t| t.join().unwrap_or_else(|_| Ok(Vec::new())))
            .transpose()?
            .unwrap_or_default();

        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Extract IPC streams (stdin write, stdout read) for protocol communication.
    /// Returns `None` if the child was not spawned with `StdioCfg::Pipe` on both.
    pub fn take_ipc_streams(
        &mut self,
    ) -> Option<(Box<dyn io::Write + Send>, Box<dyn io::Read + Send>)> {
        let stdin_h = self.stdin_write.take()?;
        let stdout_h = self.stdout_read.take()?;
        let stdin_file = ffi::handle_into_write_file(stdin_h);
        let stdout_file = ffi::handle_into_read_file(stdout_h);
        Some((Box::new(stdin_file), Box::new(stdout_file)))
    }
}

// ── AppContainerSpawn ─────────────────────────────────────────────────────────

/// AppContainer spawn builder. Created by `build_sandboxed_command`; configure
/// stdio via `set_stdin/stdout/stderr`, then launch with `spawn()`.
pub struct AppContainerSpawn {
    program: String,
    args: Vec<String>,
    repo_root: PathBuf,
    scratch_dir: PathBuf,
    policy: SandboxPolicy,
    /// Per-language toolchain cache grants (read/write paths) and env.
    /// #501: the child env is an explicit allowlist block (`build_env_block`),
    /// NOT inherited — `toolchain.env` (JAVA_HOME/GOPATH/…) is forwarded into
    /// it and `~/.travsr/bin` is prepended to PATH, mirroring linux.rs/macos.rs.
    toolchain: crate::sandbox::toolchain::ToolchainAccess,
    stdin: StdioCfg,
    stdout: StdioCfg,
    stderr: StdioCfg,
}

impl AppContainerSpawn {
    pub(super) fn set_stdin(&mut self, cfg: StdioCfg) {
        self.stdin = cfg;
    }
    pub(super) fn set_stdout(&mut self, cfg: StdioCfg) {
        self.stdout = cfg;
    }
    pub(super) fn set_stderr(&mut self, cfg: StdioCfg) {
        self.stderr = cfg;
    }

    /// Spawn the plugin binary inside an AppContainer with a Job Object.
    /// Fail-closed (ADR-017 Rule 2): any setup failure returns `Err`.
    pub(super) fn spawn(self) -> io::Result<AppContainerChild> {
        let profile = profile_name(&self.repo_root);
        let elevated = matches!(self.policy, SandboxPolicy::Elevated { .. });

        // ── 1–4. AppContainer SID, profile, DACL grants, SECURITY_CAPABILITIES ─
        let sid = ffi::derive_appcontainer_sid(&profile)?;
        ffi::ensure_appcontainer_profile(&profile)?;
        ffi::grant_path_access(&self.repo_root, sid.as_psid(), ffi::ACCESS_GENERIC_READ)?;
        ffi::grant_path_access(&self.scratch_dir, sid.as_psid(), ffi::ACCESS_GENERIC_ALL)?;
        // Per-language toolchain caches (best-effort: a missing cache dir is not fatal).
        for path in &self.toolchain.read_paths {
            let _ = ffi::grant_path_access(path, sid.as_psid(), ffi::ACCESS_GENERIC_READ);
        }
        for path in &self.toolchain.write_paths {
            let _ = ffi::grant_path_access(path, sid.as_psid(), ffi::ACCESS_GENERIC_ALL);
        }
        // PR #577 review: an AppContainer token cannot map an image whose DACL
        // carries no AppContainer ACE — user-profile trees don't carry ALL
        // APPLICATION PACKAGES, and the owner-only hardening of ~/.travsr
        // (#507, travsr-store restrict_to_owner_windows) strips inherited ACEs
        // for anything created under it afterwards. The resolver hands us
        // plugin binaries from exactly ~/.travsr/bin, so grant read+execute on
        // that dir and on the program's own directory; without this the
        // sandbox spawn fails ERROR_ACCESS_DENIED at image load and the PATH
        // prepend (#501) points at files the child could not execute anyway.
        // Best-effort like the toolchain grants: the dir may not exist yet,
        // and a program under a machine-wide tree (Program Files) already
        // carries the needed ACEs. Idempotent per #505 — a repeat is one read.
        if let Some(home) = dirs::home_dir() {
            let travsr_bin = home.join(".travsr").join("bin");
            if travsr_bin.is_dir() {
                let _ = ffi::grant_path_access(
                    &travsr_bin,
                    sid.as_psid(),
                    ffi::ACCESS_GENERIC_READ_EXECUTE,
                );
            }
        }
        if let Some(program_dir) = std::path::Path::new(&self.program).parent() {
            if program_dir.is_dir() {
                let _ = ffi::grant_path_access(
                    program_dir,
                    sid.as_psid(),
                    ffi::ACCESS_GENERIC_READ_EXECUTE,
                );
            }
        }

        // PSE R5 (#499): capability storage is heap-pinned inside the owner;
        // the binding only needs to stay alive until CreateProcessW returns.
        let security_caps = ffi::build_security_capabilities(sid.as_psid(), elevated)?;

        if let SandboxPolicy::Elevated {
            permitted_hosts, ..
        } = &self.policy
        {
            tracing::warn!(
                permitted_hosts = ?permitted_hosts,
                "ADR-017 Elevated on Windows: AppContainer allows internet client; \
                 per-host filtering unavailable at OS level — enforce via egress proxy"
            );
        }

        // ── 5. Job Object ──────────────────────────────────────────────────────
        let job = ffi::create_job_with_limits()?;

        // ── 6. CreateProcessW inside AppContainer (P5-S3) ─────────────────────
        let handles = ffi::spawn_in_appcontainer(
            &self.program,
            &self.args,
            &self.scratch_dir,
            &self.toolchain.env,  // #501: forwarded into the child env block
            security_caps.caps(), // PSE R5: owner `security_caps` still live here
            job,
            to_mode(self.stdin),
            to_mode(self.stdout),
            to_mode(self.stderr),
        )?;

        Ok(AppContainerChild {
            process: handles.process,
            _job: handles._job,
            pid: handles.pid,
            stdin_write: handles.stdin_write,
            stdout_read: handles.stdout_read,
            stderr_read: handles.stderr_read,
        })
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Returns a `SandboxedSpawn::AppContainer` for the given arguments.
/// Fail-closed: validates Elevated policy before returning.
pub fn build_sandboxed_command(
    program: &str,
    args: &[&str],
    repo_root: &std::path::Path,
    scratch_dir: &std::path::Path,
    policy: &SandboxPolicy,
    language: &str,
) -> Result<super::SandboxedSpawn, SandboxUnavailable> {
    if let SandboxPolicy::Elevated { .. } = policy {
        policy.validate()?;
    }
    Ok(super::SandboxedSpawn::AppContainer(AppContainerSpawn {
        program: program.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        repo_root: repo_root.to_path_buf(),
        scratch_dir: scratch_dir.to_path_buf(),
        policy: policy.clone(),
        toolchain: crate::sandbox::toolchain::toolchain_access(language),
        stdin: StdioCfg::Inherit,
        stdout: StdioCfg::Inherit,
        stderr: StdioCfg::Inherit,
    }))
}
