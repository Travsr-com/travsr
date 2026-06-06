#[cfg(target_os = "windows")]
pub mod windows;
pub mod linux;
pub mod macos;
pub mod policy;

pub use policy::{SandboxPolicy, SandboxUnavailable};

/// How a single stdio stream is configured for a sandboxed process.
#[derive(Clone, Copy)]
pub enum StdioCfg {
    /// Inherit the parent process's handle.
    Inherit,
    /// Redirect to the null device (discard).
    Null,
    /// Create an anonymous pipe; the parent end is accessible on the child.
    Pipe,
}

impl StdioCfg {
    fn into_stdio(self) -> std::process::Stdio {
        match self {
            StdioCfg::Inherit => std::process::Stdio::inherit(),
            StdioCfg::Null => std::process::Stdio::null(),
            StdioCfg::Pipe => std::process::Stdio::piped(),
        }
    }
}

/// Completed sandboxed child process.
pub enum SandboxedChild {
    Standard(std::process::Child),
    #[cfg(target_os = "windows")]
    AppContainer(windows::AppContainerChild),
}

impl SandboxedChild {
    pub fn id(&self) -> u32 {
        match self {
            SandboxedChild::Standard(c) => c.id(),
            #[cfg(target_os = "windows")]
            SandboxedChild::AppContainer(c) => c.id(),
        }
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        match self {
            SandboxedChild::Standard(c) => c.kill(),
            #[cfg(target_os = "windows")]
            SandboxedChild::AppContainer(c) => c.kill(),
        }
    }

    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match self {
            SandboxedChild::Standard(c) => c.wait(),
            #[cfg(target_os = "windows")]
            SandboxedChild::AppContainer(c) => c.wait(),
        }
    }

    pub fn wait_with_output(self) -> std::io::Result<std::process::Output> {
        match self {
            SandboxedChild::Standard(c) => c.wait_with_output(),
            #[cfg(target_os = "windows")]
            SandboxedChild::AppContainer(c) => c.wait_with_output(),
        }
    }

    /// Extract IPC streams (stdin write, stdout read) for protocol use.
    /// Returns `None` if the child was not spawned with `StdioCfg::Pipe` on both.
    pub fn take_ipc_streams(
        &mut self,
    ) -> Option<(
        Box<dyn std::io::Write + Send>,
        Box<dyn std::io::Read + Send>,
    )> {
        match self {
            SandboxedChild::Standard(child) => {
                let stdin = child.stdin.take()?;
                let stdout = child.stdout.take()?;
                Some((
                    Box::new(stdin) as Box<dyn std::io::Write + Send>,
                    Box::new(stdout) as Box<dyn std::io::Read + Send>,
                ))
            }
            #[cfg(target_os = "windows")]
            SandboxedChild::AppContainer(ac) => ac.take_ipc_streams(),
        }
    }
}

/// Result of `build_sandboxed_command`. Configure stdio, then call `spawn`.
pub enum SandboxedSpawn {
    /// Linux (bwrap) or macOS (sandbox-exec): wraps the outer wrapper process.
    Wrapped(std::process::Command),
    /// Windows: AppContainer + Job Object (RFC-014 / ADR-017 Rule 2).
    #[cfg(target_os = "windows")]
    AppContainer(windows::AppContainerSpawn),
}

impl SandboxedSpawn {
    pub fn stdin(&mut self, cfg: StdioCfg) -> &mut Self {
        match self {
            SandboxedSpawn::Wrapped(cmd) => {
                cmd.stdin(cfg.into_stdio());
            }
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(ac) => ac.set_stdin(cfg),
        }
        self
    }

    pub fn stdout(&mut self, cfg: StdioCfg) -> &mut Self {
        match self {
            SandboxedSpawn::Wrapped(cmd) => {
                cmd.stdout(cfg.into_stdio());
            }
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(ac) => ac.set_stdout(cfg),
        }
        self
    }

    pub fn stderr(&mut self, cfg: StdioCfg) -> &mut Self {
        match self {
            SandboxedSpawn::Wrapped(cmd) => {
                cmd.stderr(cfg.into_stdio());
            }
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(ac) => ac.set_stderr(cfg),
        }
        self
    }

    /// Spawn the sandboxed process.
    pub fn spawn(self) -> std::io::Result<SandboxedChild> {
        match self {
            SandboxedSpawn::Wrapped(mut cmd) => Ok(SandboxedChild::Standard(cmd.spawn()?)),
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(ac) => {
                Ok(SandboxedChild::AppContainer(ac.spawn()?))
            }
        }
    }

    /// Convenience: spawn with inherited stdio, wait, return exit status.
    pub fn status(self) -> std::io::Result<std::process::ExitStatus> {
        match self {
            SandboxedSpawn::Wrapped(mut cmd) => cmd.status(),
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(mut ac) => {
                ac.set_stdin(StdioCfg::Inherit);
                ac.set_stdout(StdioCfg::Inherit);
                ac.set_stderr(StdioCfg::Inherit);
                ac.spawn()?.wait()
            }
        }
    }

    /// Convenience: stdin=null, capture stdout+stderr, wait, return Output.
    pub fn output(self) -> std::io::Result<std::process::Output> {
        match self {
            SandboxedSpawn::Wrapped(mut cmd) => cmd.output(),
            #[cfg(target_os = "windows")]
            SandboxedSpawn::AppContainer(mut ac) => {
                ac.set_stdin(StdioCfg::Null);
                ac.set_stdout(StdioCfg::Pipe);
                ac.set_stderr(StdioCfg::Pipe);
                ac.spawn()?.wait_with_output()
            }
        }
    }
}
