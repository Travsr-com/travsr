use crate::sandbox::policy::SandboxUnavailable;
use crate::sandbox::StdioCfg;
use crate::stderr_ring::StderrRing;
use std::io::{BufReader, BufWriter};
use std::sync::{Arc, Mutex};
use travsr_error::IndexError;
use travsr_plugin_protocol::{
    codec::{decode_message, write_message},
    HandshakeRequest, InvokeRequest, InvokeResponse, ParseRequest, ParseResponse, Plugin,
    PluginRequest, PluginResponse, PROTOCOL_VERSION,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginHealth {
    Ok,
    Degraded(String),
    Disabled(String),
}

pub trait Transport: Send + Sync {
    fn parse(&self, req: ParseRequest) -> Result<ParseResponse, IndexError>;
    /// InProcess: MUST return Err(IndexError::PhaseNotSupported).
    /// Sidecar: sends InvokeRequest over the wire.
    fn invoke_phase_b(&self, req: InvokeRequest) -> Result<InvokeResponse, IndexError>;
    fn health(&self) -> PluginHealth;
}

/// Zero-IPC. Calls plugin directly in the daemon's address space.
/// PERMITTED ONLY for first-party, pinned, fuzzed, fixture-gated Phase A grammars.
/// NEVER for Phase B. NEVER for --command community plugins. (ADR-017 Rule 4)
pub struct InProcess {
    plugin: Arc<dyn Plugin>,
}

impl InProcess {
    pub fn new(plugin: impl Plugin + 'static) -> Self {
        Self {
            plugin: Arc::new(plugin),
        }
    }
}

impl Transport for InProcess {
    fn parse(&self, req: ParseRequest) -> Result<ParseResponse, IndexError> {
        Ok(self.plugin.parse(&req))
    }

    /// Normative (RFC-011 §2): MUST return PhaseNotSupported.
    /// Returning Ok would silently mask an InProcess/PhaseB misconfiguration.
    fn invoke_phase_b(&self, _req: InvokeRequest) -> Result<InvokeResponse, IndexError> {
        Err(IndexError::PhaseNotSupported)
    }

    fn health(&self) -> PluginHealth {
        PluginHealth::Ok
    }
}

// ── Sidecar I/O types ─────────────────────────────────────────────────────────

type SidecarIo = (
    BufWriter<Box<dyn std::io::Write + Send>>,
    BufReader<Box<dyn std::io::Read + Send>>,
);

/// #388: handshake window. A hung plugin (e.g. one that deadlocks on startup
/// reading stdin) must not block `spawn` — and therefore the whole
/// `invoke_phase_b_all` scoped join — forever. 60 s covers a cold Node/JVM
/// plugin start; `with_io_watchdog` returns the instant the handshake decodes,
/// so a healthy spawn is not penalised the full window.
const HANDSHAKE_TIMEOUT_SECS: u64 = 60;

/// #388: per-invocation Phase B window (was the C1 watchdog in indexer.rs).
/// A plugin that wedges mid-invoke is killed after this so its language is
/// recorded as crashed instead of starving the scoped join. 5 min covers
/// large-repo SCIP passes (KLS/sbt).
const INVOKE_TIMEOUT_SECS: u64 = 300;

/// Subprocess transport. Spawns under ADR-017 SandboxPolicy::Standard.
/// Uses interior Mutex for I/O so the trait can stay `&self` (Send+Sync).
pub struct Sidecar {
    language: String,
    #[allow(dead_code)]
    plugin_version: String,
    /// None for stub instances (P5-S1 compatibility).
    #[allow(dead_code)] // held for process lifetime; `Drop for Sidecar` kills and reaps it
    _child: Option<Mutex<crate::sandbox::SandboxedChild>>,
    /// OS PID of the sidecar subprocess. Captured at spawn so the I/O watchdog
    /// can SIGTERM the child on timeout without locking `_child` (#388).
    /// `None` for stub instances.
    pid: Option<u32>,
    io: Option<Mutex<SidecarIo>>,
    health: Mutex<PluginHealth>,
    /// Per-invocation scratch tmpdir (ADR-017 Rule 1 — read-write, cleaned up on drop).
    _scratch: Option<tempfile::TempDir>,
    /// Bounded ring draining the sidecar's own stderr so a Phase B analyzer that
    /// fails silently (e.g. libclang denied a sandbox read and every translation
    /// unit fails to parse, yielding an empty index) is diagnosable via tracing
    /// instead of swallowed. Empty for stub instances.
    stderr_ring: StderrRing,
}

impl Sidecar {
    /// Returns the OS PID of the sidecar subprocess, used by the C1 watchdog
    /// to send SIGTERM if the sidecar exceeds the per-language timeout.
    pub fn child_pid(&self) -> Option<u32> {
        self._child
            .as_ref()
            .and_then(|m| m.lock().ok().map(|c| c.id()))
    }

    /// Kept for test compatibility (P5-S1 skeleton).
    pub fn stub(language: impl Into<String>) -> Self {
        Self {
            language: language.into(),
            plugin_version: String::new(),
            _child: None,
            pid: None,
            io: None,
            health: Mutex::new(PluginHealth::Disabled("stub sidecar".into())),
            _scratch: None,
            stderr_ring: StderrRing::spawn_empty(),
        }
    }

    /// Spawn the plugin binary under the ADR-017 sandbox described by `spec`.
    ///
    /// `spec.program` is the binary to execute; `spec.args` are its arguments;
    /// `spec.policy` controls the sandbox level applied before spawning.
    pub fn spawn(
        spec: &crate::resolver::PluginSpec,
        repo_root: &std::path::Path,
    ) -> Result<Self, IndexError> {
        let lang = &spec.language;
        // Create per-invocation scratch tmpdir (ADR-017 Rule 1).
        // Dropped when Sidecar drops — guarantees cleanup even on error paths.
        let scratch = tempfile::Builder::new()
            .prefix("travsr-plugin-")
            .tempdir()
            .map_err(|e| IndexError::Parse {
                file: format!("plugin:{lang}"),
                message: format!("failed to create scratch dir: {e}"),
            })?;
        let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
        tracing::debug!(
            lang = %lang,
            program = %spec.program,
            "Phase B: spawning sidecar"
        );
        let mut spawner = Self::build_cmd(
            &spec.program,
            &args,
            repo_root,
            scratch.path(),
            &spec.policy,
            &spec.language,
        )
        .map_err(|e| IndexError::Parse {
            file: format!("plugin:{lang}"),
            message: e.to_string(),
        })?;

        spawner
            .stdin(StdioCfg::Pipe)
            .stdout(StdioCfg::Pipe)
            .stderr(StdioCfg::Pipe);

        let mut child = spawner.spawn().map_err(|e| IndexError::Parse {
            file: format!("plugin:{lang}"),
            message: format!("spawn failed: {e}"),
        })?;

        // Drain the sidecar's stderr into a bounded ring so its own diagnostics
        // are recoverable on failure/empty-index instead of being discarded.
        let stderr_ring = match child.take_stderr() {
            Some(stderr) => StderrRing::spawn(stderr),
            None => StderrRing::spawn_empty(),
        };

        let (raw_stdin, raw_stdout) =
            child.take_ipc_streams().ok_or_else(|| IndexError::Parse {
                file: format!("plugin:{lang}"),
                message: "piped stdio not available after spawn".into(),
            })?;

        let pid = child.id();
        let mut writer = BufWriter::new(raw_stdin);
        let mut reader = BufReader::new(raw_stdout);

        // #388: guard the handshake with an I/O watchdog. A plugin that wedges
        // on startup (never writes a HandshakeResponse) would otherwise block
        // `decode_message` — and the caller's `thread::scope` join — forever.
        // The watchdog SIGKILLs the child on timeout, closing its stdout so the
        // pending decode returns an error and `spawn` fails cleanly instead of
        // hanging.
        let hs: PluginResponse = crate::watchdog::with_io_watchdog(
            pid,
            std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
            || {
                write_message(
                    &mut writer,
                    &PluginRequest::Handshake(HandshakeRequest {
                        daemon_protocol_version: PROTOCOL_VERSION,
                    }),
                )
                .map_err(|e| IndexError::Parse {
                    file: format!("plugin:{lang}"),
                    message: e.to_string(),
                })?;
                decode_message(&mut reader).map_err(|e| IndexError::Parse {
                    file: format!("plugin:{lang}"),
                    message: e.to_string(),
                })
            },
        )
        .map_err(|e| {
            // A sidecar that dies during startup (dyld/link failure, panic before
            // the handshake write) leaves only an EOF here; echo its stderr so the
            // real cause is visible instead of a bare "failed to fill whole buffer".
            let tail = stderr_ring.tail();
            if !tail.is_empty() {
                tracing::warn!(lang = %lang, stderr = %tail, "Phase B: sidecar failed during handshake");
            }
            e
        })?;

        let plugin_version = match hs {
            PluginResponse::Handshake(h) => {
                if h.protocol_version != PROTOCOL_VERSION {
                    return Err(IndexError::ProtocolVersionMismatch {
                        expected: PROTOCOL_VERSION,
                        got: h.protocol_version,
                    });
                }
                h.plugin_version
            }
            _ => {
                return Err(IndexError::Parse {
                    file: format!("plugin:{lang}"),
                    message: "expected HandshakeResponse".into(),
                })
            }
        };

        tracing::debug!(
            lang = %lang,
            plugin_version = %plugin_version,
            "Phase B: sidecar handshake ok"
        );

        Ok(Self {
            language: lang.to_string(),
            plugin_version,
            _child: Some(Mutex::new(child)),
            pid: Some(pid),
            io: Some(Mutex::new((writer, reader))),
            health: Mutex::new(PluginHealth::Ok),
            _scratch: Some(scratch),
            stderr_ring,
        })
    }

    #[cfg(target_os = "linux")]
    fn build_cmd(
        program: &str,
        args: &[&str],
        repo_root: &std::path::Path,
        scratch: &std::path::Path,
        policy: &crate::sandbox::policy::SandboxPolicy,
        language: &str,
    ) -> Result<crate::sandbox::SandboxedSpawn, SandboxUnavailable> {
        crate::sandbox::linux::build_sandboxed_command(
            program, args, repo_root, scratch, policy, language,
        )
    }

    #[cfg(target_os = "macos")]
    fn build_cmd(
        program: &str,
        args: &[&str],
        repo_root: &std::path::Path,
        scratch: &std::path::Path,
        policy: &crate::sandbox::policy::SandboxPolicy,
        language: &str,
    ) -> Result<crate::sandbox::SandboxedSpawn, SandboxUnavailable> {
        crate::sandbox::macos::build_sandboxed_command(
            program, args, repo_root, scratch, policy, language,
        )
    }

    #[cfg(target_os = "windows")]
    fn build_cmd(
        program: &str,
        args: &[&str],
        repo_root: &std::path::Path,
        scratch: &std::path::Path,
        policy: &crate::sandbox::policy::SandboxPolicy,
        language: &str,
    ) -> Result<crate::sandbox::SandboxedSpawn, SandboxUnavailable> {
        crate::sandbox::windows::build_sandboxed_command(
            program, args, repo_root, scratch, policy, language,
        )
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    fn build_cmd(
        _p: &str,
        _a: &[&str],
        _r: &std::path::Path,
        _s: &std::path::Path,
        _policy: &crate::sandbox::policy::SandboxPolicy,
        _language: &str,
    ) -> Result<crate::sandbox::SandboxedSpawn, SandboxUnavailable> {
        Err(SandboxUnavailable("unsupported platform".into()))
    }

    fn mark_crashed(&self) {
        // Surface the sidecar's own last words — a libclang/parse/link failure it
        // printed before dying is otherwise lost, leaving only a generic crash.
        let tail = self.stderr_ring.tail();
        if tail.is_empty() {
            tracing::warn!(lang = %self.language, "Phase B: sidecar crashed (no stderr captured)");
        } else {
            tracing::warn!(lang = %self.language, stderr = %tail, "Phase B: sidecar crashed");
        }
        if let Ok(mut h) = self.health.lock() {
            *h = PluginHealth::Disabled(format!("plugin {} crashed", self.language));
        }
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        // #715: kill and reap the child before any field drops. `stderr_ring`'s
        // Drop joins its reader thread, which only returns once the child's
        // stderr write end is closed, i.e. once the child is dead. On a normal
        // teardown a well-behaved sidecar exits on stdin EOF and the join is
        // instant, but a watchdog-timed-out invoke can drop this while the child
        // is still busy in a long parse and has not reached its stdin read; the
        // join would then block for as long as that takes. Explicitly killing
        // here (the Drop body runs before any field is dropped) guarantees the
        // ring's EOF assumption instead of relying on it incidentally, mirroring
        // `EmbedSidecar::drop`. `wait` reaps the child so it is not left a zombie.
        if let Some(child) = &self._child {
            if let Ok(mut c) = child.lock() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

impl Transport for Sidecar {
    fn parse(&self, req: ParseRequest) -> Result<ParseResponse, IndexError> {
        let io_lock = match &self.io {
            Some(m) => m,
            None => return Err(IndexError::PhaseNotSupported),
        };
        let mut io = io_lock.lock().map_err(|_| IndexError::PluginCrashed {
            language: self.language.clone(),
        })?;
        let (writer, reader) = &mut *io;

        if let Err(e) = write_message(writer, &PluginRequest::Parse(req)) {
            self.mark_crashed();
            return Err(IndexError::Parse {
                file: format!("plugin:{}", self.language),
                message: e.to_string(),
            });
        }

        match decode_message::<PluginResponse>(reader) {
            Ok(PluginResponse::Parse(resp)) => Ok(resp),
            Ok(PluginResponse::Error(e)) => Err(IndexError::Parse {
                file: e.file,
                message: e.message,
            }),
            Ok(_) => Err(IndexError::Parse {
                file: format!("plugin:{}", self.language),
                message: "unexpected response type".into(),
            }),
            Err(_e) => {
                self.mark_crashed();
                Err(IndexError::PluginCrashed {
                    language: self.language.clone(),
                })
            }
        }
    }

    fn invoke_phase_b(&self, mut req: InvokeRequest) -> Result<InvokeResponse, IndexError> {
        // Inject the sandbox-authorized scratch dir so the sidecar can write
        // temp files (SCIP output, etc.) inside the sandbox's allowed write area.
        if let Some(scratch) = self._scratch.as_ref() {
            req.scratch = scratch.path().to_path_buf();
        }
        let io_lock = match &self.io {
            Some(m) => m,
            None => return Err(IndexError::PhaseNotSupported),
        };
        let mut io = io_lock.lock().map_err(|_| IndexError::PluginCrashed {
            language: self.language.clone(),
        })?;
        let (writer, reader) = &mut *io;

        // #388: guard the invoke round-trip with an I/O watchdog so a plugin
        // that wedges mid-analysis (hung SCIP pass, blocked read) is killed
        // after INVOKE_TIMEOUT_SECS rather than starving the caller's scoped
        // join indefinitely. On kill the child's stdout closes and the pending
        // `decode_message` returns Err, which we map to PluginCrashed. When
        // `pid` is absent (never for a real spawn) we fall back to unguarded I/O.
        let write_res = match self.pid {
            Some(pid) => crate::watchdog::with_io_watchdog(
                pid,
                std::time::Duration::from_secs(INVOKE_TIMEOUT_SECS),
                || {
                    write_message(writer, &PluginRequest::Invoke(req))?;
                    Ok(decode_message::<PluginResponse>(reader))
                },
            ),
            None => write_message(writer, &PluginRequest::Invoke(req))
                .map(|_| decode_message::<PluginResponse>(reader)),
        };

        let decoded = match write_res {
            Ok(decoded) => decoded,
            Err(e) => {
                self.mark_crashed();
                return Err(IndexError::Parse {
                    file: format!("plugin:{}", self.language),
                    message: e.to_string(),
                });
            }
        };

        match decoded {
            Ok(PluginResponse::Invoke(resp)) => {
                // A clean handshake + invoke that nonetheless yields zero nodes is
                // the exact shape of a silent analyzer failure (e.g. libclang
                // denied a sandbox read → every TU fails to parse → empty index).
                // Echo the sidecar's own stderr at debug so the cause is
                // recoverable rather than surfacing only as a generic zero-node
                // warning with a misdirecting remedy.
                if resp.nodes.is_empty() {
                    let tail = self.stderr_ring.tail();
                    if !tail.is_empty() {
                        tracing::debug!(
                            lang = %self.language,
                            stderr = %tail,
                            "Phase B: sidecar returned zero nodes; sidecar stderr follows"
                        );
                    }
                }
                Ok(resp)
            }
            Ok(PluginResponse::Error(e)) => Err(IndexError::Parse {
                file: e.file,
                message: e.message,
            }),
            Ok(_) => Err(IndexError::Parse {
                file: format!("plugin:{}", self.language),
                message: "unexpected response type".into(),
            }),
            Err(_e) => {
                self.mark_crashed();
                Err(IndexError::PluginCrashed {
                    language: self.language.clone(),
                })
            }
        }
    }

    fn health(&self) -> PluginHealth {
        self.health
            .lock()
            .map(|h| h.clone())
            .unwrap_or_else(|_| PluginHealth::Disabled("health lock poisoned".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_plugin_protocol::{InvokeResponse, ParseResponse};

    struct NoOpPlugin;
    impl Plugin for NoOpPlugin {
        fn language(&self) -> travsr_core::Language {
            travsr_core::Language::TypeScript
        }
        fn extensions(&self) -> &[&str] {
            &["ts"]
        }
        fn supports_phase_b(&self) -> bool {
            false
        }
        fn parse(&self, _req: &travsr_plugin_protocol::ParseRequest) -> ParseResponse {
            ParseResponse::default()
        }
        fn invoke_phase_b(&self, _req: &travsr_plugin_protocol::InvokeRequest) -> InvokeResponse {
            InvokeResponse::default()
        }
    }

    #[test]
    fn in_process_invoke_phase_b_returns_phase_not_supported() {
        let t = InProcess::new(NoOpPlugin);
        let req = InvokeRequest {
            root: std::path::PathBuf::from("."),
            corpus: String::new(),
            scratch: std::path::PathBuf::default(),
            files: None,
        };
        assert!(matches!(
            t.invoke_phase_b(req),
            Err(IndexError::PhaseNotSupported)
        ));
    }

    #[test]
    fn sidecar_stub_is_disabled() {
        let s = Sidecar::stub("kotlin");
        assert!(matches!(s.health(), PluginHealth::Disabled(_)));
    }
}
