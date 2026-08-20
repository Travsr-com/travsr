pub mod linux;
pub mod macos;
pub mod policy;
pub mod toolchain;
#[cfg(target_os = "windows")]
pub mod windows;

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

    /// Take the child's piped stderr, if it was spawned with `StdioCfg::Pipe`.
    /// Used to drain a Phase B sidecar's own diagnostics into a bounded ring so
    /// a libclang/parse failure that yields a silent empty index is surfaced via
    /// tracing instead of being swallowed. `None` for the Windows AppContainer
    /// path (stderr handled by the container) and for stub children.
    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        match self {
            SandboxedChild::Standard(child) => child.stderr.take(),
            #[cfg(target_os = "windows")]
            SandboxedChild::AppContainer(_) => None,
        }
    }
}

/// Build a plain (unsandboxed) child command for a Phase B analyzer.
///
/// Used ONLY on Windows, and ONLY for analyzers whose build tools cannot run
/// inside the isolation layer (`WindowsSandbox::Unsupported`), and ONLY after the
/// user has granted explicit permission (see `resolver`). The child runs with the
/// user's own privileges — the same trade-off the project already accepts for the
/// rust `--allow-unsandboxed` LSIF path.
///
/// The toolchain environment (JAVA_HOME, GRADLE_USER_HOME, HOME, …) is forwarded
/// and `~/.travsr/bin` is prepended to PATH, mirroring what the isolated path sets
/// up, so the analyzer and the build tool it drives resolve. `scratch` is the cwd
/// (matching the isolated spawn); the repo root reaches the sidecar via the
/// InvokeRequest, not the cwd.
pub fn build_unsandboxed_command(
    program: &str,
    args: &[&str],
    scratch: &std::path::Path,
    language: &str,
) -> SandboxedSpawn {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    cmd.current_dir(scratch);

    let access = toolchain::toolchain_access(language);
    for (k, v) in &access.env {
        cmd.env(k, v);
    }

    if let Some(home) = dirs::home_dir() {
        // The toolchain env helpers key their HOME/cache paths off the `HOME`
        // variable, which is unset on Windows (it uses `USERPROFILE`), so JVM build
        // tools like Gradle end up with nowhere to place their home/temp dir. An
        // unsandboxed child runs as the user, so give it the real home explicitly —
        // set `HOME` and Gradle's home unless the toolchain env already provided
        // them. Harmless off Windows, where `HOME` is already correct.
        let home_str = home.to_string_lossy().into_owned();
        if !access.env.iter().any(|(k, _)| k == "HOME") {
            cmd.env("HOME", &home_str);
        }
        if !access.env.iter().any(|(k, _)| k == "GRADLE_USER_HOME") {
            cmd.env("GRADLE_USER_HOME", home.join(".gradle"));
        }

        // Prepend ~/.travsr/bin so the wrapper's installed siblings (and analyzers
        // the wrapper shells out to) resolve, matching the isolated path's PATH
        // handling.
        let travsr_bin = home.join(".travsr").join("bin");
        let existing = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs_vec = vec![travsr_bin];
        dirs_vec.extend(std::env::split_paths(&existing));
        if let Ok(joined) = std::env::join_paths(dirs_vec) {
            cmd.env("PATH", joined);
        }
    }

    SandboxedSpawn::Wrapped(cmd)
}

/// Result of `build_sandboxed_command`. Configure stdio, then call `spawn`.
pub enum SandboxedSpawn {
    /// Linux (bwrap) or macOS (sandbox-exec): wraps the outer wrapper process.
    Wrapped(std::process::Command),
    /// Windows: AppContainer + Job Object (ADR-017 Rules 1-2).
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
            SandboxedSpawn::AppContainer(ac) => Ok(SandboxedChild::AppContainer(ac.spawn()?)),
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
