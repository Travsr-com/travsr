use crate::sandbox::policy::SandboxUnavailable;
use crate::sandbox::StdioCfg;
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

/// Subprocess transport. Spawns under ADR-017 SandboxPolicy::Standard.
/// Uses interior Mutex for I/O so the trait can stay `&self` (Send+Sync).
pub struct Sidecar {
    language: String,
    #[allow(dead_code)]
    plugin_version: String,
    /// None for stub instances (P5-S1 compatibility).
    #[allow(dead_code)] // held for process lifetime; drop kills the subprocess
    _child: Option<Mutex<crate::sandbox::SandboxedChild>>,
    io: Option<Mutex<SidecarIo>>,
    health: Mutex<PluginHealth>,
    /// Per-invocation scratch tmpdir (ADR-017 Rule 1 — read-write, cleaned up on drop).
    _scratch: Option<tempfile::TempDir>,
}

impl Sidecar {
    /// Kept for test compatibility (P5-S1 skeleton).
    pub fn stub(language: impl Into<String>) -> Self {
        Self {
            language: language.into(),
            plugin_version: String::new(),
            _child: None,
            io: None,
            health: Mutex::new(PluginHealth::Disabled("stub sidecar".into())),
            _scratch: None,
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
            .stderr(StdioCfg::Null);

        let mut child = spawner.spawn().map_err(|e| IndexError::Parse {
            file: format!("plugin:{lang}"),
            message: format!("spawn failed: {e}"),
        })?;

        let (raw_stdin, raw_stdout) =
            child.take_ipc_streams().ok_or_else(|| IndexError::Parse {
                file: format!("plugin:{lang}"),
                message: "piped stdio not available after spawn".into(),
            })?;

        let mut writer = BufWriter::new(raw_stdin);
        let mut reader = BufReader::new(raw_stdout);

        // Handshake
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

        let hs: PluginResponse = decode_message(&mut reader).map_err(|e| IndexError::Parse {
            file: format!("plugin:{lang}"),
            message: e.to_string(),
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
            io: Some(Mutex::new((writer, reader))),
            health: Mutex::new(PluginHealth::Ok),
            _scratch: Some(scratch),
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
        if let Ok(mut h) = self.health.lock() {
            *h = PluginHealth::Disabled(format!("plugin {} crashed", self.language));
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

        if let Err(e) = write_message(writer, &PluginRequest::Invoke(req)) {
            self.mark_crashed();
            return Err(IndexError::Parse {
                file: format!("plugin:{}", self.language),
                message: e.to_string(),
            });
        }

        match decode_message::<PluginResponse>(reader) {
            Ok(PluginResponse::Invoke(resp)) => Ok(resp),
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
