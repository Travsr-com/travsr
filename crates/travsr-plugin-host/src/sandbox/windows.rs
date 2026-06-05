//! Windows AppContainer + Job Object sandbox (RFC-014 / ADR-017 Rule 2).
//! Safe wrapper — no `unsafe` code in this file; all unsafe is in `ffi.rs`.

mod ffi;

use crate::sandbox::policy::{SandboxPolicy, SandboxUnavailable};
use crate::sandbox::PlatformGuard;
use std::path::PathBuf;

/// ENV vars propagated into the sandboxed process (matches Linux/macOS).
#[allow(dead_code)] // used when CreateProcessW spawn is implemented (P5-S3)
const ENV_ALLOWLIST: &[&str] = &["PATH", "LANG", "LC_ALL"];

/// Derive a short stable profile name from the repo root path.
fn profile_name(repo_root: &std::path::Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    repo_root.hash(&mut h);
    format!("travsr-{:016x}", h.finish())
}

/// AppContainer spawn builder. Created by `build_sandboxed_command`; stdio is
/// configured via `set_stdin/stdout/stderr`, then launched with `spawn()`.
#[allow(dead_code)] // fields used when CreateProcessW integration lands (P5-S3)
pub struct AppContainerSpawn {
    program: String,
    args: Vec<String>,
    repo_root: PathBuf,
    scratch_dir: PathBuf,
    policy: SandboxPolicy,
    stdin: Option<std::process::Stdio>,
    stdout: Option<std::process::Stdio>,
    stderr: Option<std::process::Stdio>,
}

impl AppContainerSpawn {
    pub(super) fn set_stdin(&mut self, cfg: std::process::Stdio) {
        self.stdin = Some(cfg);
    }
    pub(super) fn set_stdout(&mut self, cfg: std::process::Stdio) {
        self.stdout = Some(cfg);
    }
    pub(super) fn set_stderr(&mut self, cfg: std::process::Stdio) {
        self.stderr = Some(cfg);
    }

    /// Spawn the plugin binary inside an AppContainer with a Job Object.
    /// Fail-closed: any setup failure returns `Err` (ADR-017 Rule 2).
    ///
    /// # Implementation status
    /// The AppContainer profile, ACLs, and Job Object setup are complete.
    /// The final `CreateProcessW` integration (PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES
    /// via `STARTUPINFOEXW`) is pending Phase B / P5-S3. Until then this returns
    /// `Err` — which is fail-closed: the sidecar is disabled, not unsandboxed.
    pub(super) fn spawn(self) -> std::io::Result<(std::process::Child, PlatformGuard)> {
        let profile = profile_name(&self.repo_root);
        let elevated = matches!(self.policy, SandboxPolicy::Elevated { .. });

        // ── 1. Derive AppContainer SID for this profile ───────────────────────
        let sid = ffi::derive_appcontainer_sid(&profile)?;

        // ── 2. Ensure the AppContainer profile exists ─────────────────────────
        ffi::ensure_appcontainer_profile(&profile)?;

        // ── 3. Grant directory access to the AppContainer SID ────────────────
        ffi::grant_path_access(&self.repo_root, sid.as_psid(), ffi::ACCESS_GENERIC_READ)?;
        ffi::grant_path_access(&self.scratch_dir, sid.as_psid(), ffi::ACCESS_GENERIC_ALL)?;

        // ── 4. Build SECURITY_CAPABILITIES ───────────────────────────────────
        let (_cap_sid_buf, _cap_attr, _security_caps) =
            ffi::build_security_capabilities(sid.as_psid(), elevated)?;

        if let SandboxPolicy::Elevated { permitted_hosts, .. } = &self.policy {
            tracing::warn!(
                permitted_hosts = ?permitted_hosts,
                "ADR-017 Elevated on Windows: AppContainer allows internet client; \
                 per-host filtering unavailable at OS level — enforce via egress proxy"
            );
        }

        // ── 5. Create Job Object with resource limits ─────────────────────────
        let _job = ffi::create_job_with_limits()?;

        // ── 6. Pending: CreateProcessW with PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES ──
        // `CommandExt::raw_attribute` is not yet stable in this toolchain.
        // Full implementation via STARTUPINFOEXW / CreateProcessW is tracked
        // for Phase B (P5-S3). Returning Err here is fail-closed per ADR-017 Rule 2:
        // the plugin sidecar is disabled rather than running unsandboxed.
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "AppContainer process creation via CreateProcessW pending P5-S3 implementation",
        ))
    }
}

/// Returns a `SandboxedSpawn::AppContainer` for the given arguments.
/// Fail-closed: validates Elevated policy before returning.
pub fn build_sandboxed_command(
    program: &str,
    args: &[&str],
    repo_root: &std::path::Path,
    scratch_dir: &std::path::Path,
    policy: &SandboxPolicy,
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
        stdin: None,
        stdout: None,
        stderr: None,
    }))
}
