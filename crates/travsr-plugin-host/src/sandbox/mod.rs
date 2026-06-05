#[cfg(target_os = "windows")]
pub mod windows;
pub mod linux;
pub mod macos;
pub mod policy;

pub use policy::{SandboxPolicy, SandboxUnavailable};

/// Platform-specific RAII resource guard. On Windows, holds the Job Object
/// handle with KILL_ON_JOB_CLOSE — drop when the child should be killable.
/// On Linux and macOS, always `None`.
pub type PlatformGuard = Option<Box<dyn std::any::Any + Send + Sync>>;

/// Result of `build_sandboxed_command`. Configure stdio, then call `spawn`.
pub enum SandboxedSpawn {
    /// Linux (bwrap) or macOS (sandbox-exec): wraps the outer wrapper process.
    Wrapped(std::process::Command),
    /// Windows: AppContainer + Job Object (RFC-014 / ADR-017 Rule 2).
    #[cfg(target_os = "windows")]
    AppContainer(windows::AppContainerSpawn),
}

impl SandboxedSpawn {
    pub fn stdin(&mut self, cfg: std::process::Stdio) -> &mut Self {
        match self {
            SandboxedSpawn::Wrapped(cmd) => {
                cmd.stdin(cfg);
            }
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(ac) => ac.set_stdin(cfg),
        }
        self
    }

    pub fn stdout(&mut self, cfg: std::process::Stdio) -> &mut Self {
        match self {
            SandboxedSpawn::Wrapped(cmd) => {
                cmd.stdout(cfg);
            }
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(ac) => ac.set_stdout(cfg),
        }
        self
    }

    pub fn stderr(&mut self, cfg: std::process::Stdio) -> &mut Self {
        match self {
            SandboxedSpawn::Wrapped(cmd) => {
                cmd.stderr(cfg);
            }
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(ac) => ac.set_stderr(cfg),
        }
        self
    }

    /// Spawns the sandboxed process. The returned `PlatformGuard` must be kept
    /// alive for the duration of the child — dropping it triggers cleanup
    /// (on Windows: KILL_ON_JOB_CLOSE fires, terminating the process).
    pub fn spawn(self) -> std::io::Result<(std::process::Child, PlatformGuard)> {
        match self {
            SandboxedSpawn::Wrapped(mut cmd) => Ok((cmd.spawn()?, None)),
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(ac) => ac.spawn(),
        }
    }

    /// Convenience: spawn with inherited stdio, wait, return exit status.
    /// Used by security tests on non-Windows platforms.
    pub fn status(self) -> std::io::Result<std::process::ExitStatus> {
        match self {
            SandboxedSpawn::Wrapped(mut cmd) => cmd.status(),
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(mut ac) => {
                ac.set_stdin(std::process::Stdio::inherit());
                ac.set_stdout(std::process::Stdio::inherit());
                ac.set_stderr(std::process::Stdio::inherit());
                ac.spawn().and_then(|(mut c, _g)| c.wait()) // Child::wait needs &mut self
            }
        }
    }

    /// Convenience: stdin=null, capture stdout+stderr, wait, return Output.
    pub fn output(self) -> std::io::Result<std::process::Output> {
        match self {
            SandboxedSpawn::Wrapped(mut cmd) => cmd.output(),
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(mut ac) => {
                ac.set_stdin(std::process::Stdio::null());
                ac.set_stdout(std::process::Stdio::piped());
                ac.set_stderr(std::process::Stdio::piped());
                ac.spawn().and_then(|(c, _g)| c.wait_with_output())
            }
        }
    }
}
