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
        Self::spawn_with_handshake(spec, repo_root).map(|(s, _)| s)
    }

    /// Like [`Sidecar::spawn`] but also returns the validated handshake —
    /// used by the verification probe (ADR-019), which needs the plugin's
    /// declared LanguageId, capabilities, and golden fixture.
    pub fn spawn_with_handshake(
        spec: &crate::resolver::PluginSpec,
        repo_root: &std::path::Path,
    ) -> Result<(Self, travsr_plugin_protocol::HandshakeResponse), IndexError> {
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

        let handshake = match hs {
            PluginResponse::Handshake(h) => {
                if h.protocol_version != PROTOCOL_VERSION {
                    return Err(IndexError::ProtocolVersionMismatch {
                        expected: PROTOCOL_VERSION,
                        got: h.protocol_version,
                    });
                }
                h
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
            plugin_version = %handshake.plugin_version,
            "sidecar handshake ok"
        );

        let sidecar = Self {
            language: lang.to_string(),
            plugin_version: handshake.plugin_version.clone(),
            _child: Some(Mutex::new(child)),
            io: Some(Mutex::new((writer, reader))),
            health: Mutex::new(PluginHealth::Ok),
            _scratch: Some(scratch),
        };
        Ok((sidecar, handshake))
    }

    /// Kill the child process. Used by the deadline enforcement and the
    /// resource watchdog (ADR-018) — unblocks any thread waiting on the pipe.
    pub fn kill(&self) {
        if let Some(child) = &self._child {
            if let Ok(mut c) = child.lock() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        if let Ok(mut h) = self.health.lock() {
            *h = PluginHealth::Disabled(format!("plugin {} killed (deadline)", self.language));
        }
    }

    /// OS pid of the child process (None for stubs). Used by the ADR-018 RSS
    /// watchdog to sample memory.
    pub fn pid(&self) -> Option<u32> {
        self._child
            .as_ref()
            .and_then(|c| c.lock().ok().map(|c| c.id()))
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

        // ADR-018 output_max (T16): a ParseResponse over the cap is rejected
        // before allocation and the plugin is marked crashed — the daemon must
        // never absorb a forged giant frame.
        match travsr_plugin_protocol::decode_message_limited::<PluginResponse>(
            reader,
            crate::sandbox::policy::resource_limits::OUTPUT_MAX_BYTES,
        ) {
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

// ── LazySidecar ───────────────────────────────────────────────────────────────

/// Sidecar transport that defers the subprocess spawn until the first parse
/// request for its language (RFC-013 Direction A, Phase A dispatch).
///
/// With twelve Phase A sidecar languages, eagerly spawning every registered
/// sidecar at `PluginIndexer` construction would cost ~12 sandboxed spawns per
/// indexer — and the daemon's parallel init creates one indexer per worker
/// thread. `LazySidecar` registers extensions from the static catalog and only
/// pays the spawn + handshake cost when a file of that language is actually
/// dispatched.
///
/// Fail-closed per ADR-017 Rule 2: a spawn failure or crash loop disables the
/// language for the run (no respawn storm); every subsequent parse returns
/// `PluginCrashed` for the caller to log.
///
/// Resource governance (ADR-018): each spawned process is watched by a
/// supervisor thread that samples RSS and idle time (`WATCHDOG_INTERVAL`):
/// - RSS > `MEMORY_MAX` → killed (SOFT — the cached verdict is untouched).
/// - RSS > `MEMORY_HIGH` or `RECYCLE_AFTER_FILES` parses → drained and
///   respawned at the next parse boundary (the PHP-FPM `max_requests`
///   pattern; bounds slow leaks in grammar code we cannot fix).
/// - idle > `IDLE_TTL` → spun down; the next request respawns on demand.
/// Respawn reuses the cached subscription verdict — never a re-probe
/// (ADR-018 Rule 3 / ADR-019).
pub struct LazySidecar {
    spec: crate::resolver::PluginSpec,
    repo_root: std::path::PathBuf,
    state: Arc<Mutex<LazyState>>,
}

struct LazyState {
    sidecar: Option<Arc<Sidecar>>,
    /// Bumped on every (re)spawn so a stale watchdog thread can tell its
    /// process was replaced and exit.
    generation: u64,
    files_since_spawn: u32,
    last_used: std::time::Instant,
    /// Set by the watchdog on `MEMORY_HIGH`; honoured at the next parse
    /// boundary (graceful drain).
    recycle_pending: bool,
    consecutive_crashes: u32,
    /// Fail-closed reason (spawn failure / crash loop). Sticky for the run.
    disabled: Option<String>,
}

impl LazySidecar {
    pub fn new(spec: crate::resolver::PluginSpec, repo_root: &std::path::Path) -> Self {
        Self {
            spec,
            repo_root: repo_root.to_path_buf(),
            state: Arc::new(Mutex::new(LazyState {
                sidecar: None,
                generation: 0,
                files_since_spawn: 0,
                last_used: std::time::Instant::now(),
                recycle_pending: false,
                consecutive_crashes: 0,
                disabled: None,
            })),
        }
    }

    /// Acquire a live sidecar handle, spawning or recycling as needed.
    fn acquire(&self) -> Result<Arc<Sidecar>, IndexError> {
        use crate::sandbox::policy::resource_limits as limits;

        let mut st = self.state.lock().expect("lazy sidecar state poisoned");

        if let Some(reason) = &st.disabled {
            return Err(IndexError::Parse {
                file: format!("plugin:{}", self.spec.language),
                message: reason.clone(),
            });
        }

        // Recycle boundary (ADR-018 Rule 3): drain the old process before the
        // next parse. Dropping our Arc closes stdin once in-flight users
        // finish; the child exits on EOF.
        if st.sidecar.is_some()
            && (st.recycle_pending || st.files_since_spawn >= limits::RECYCLE_AFTER_FILES)
        {
            tracing::info!(
                lang = %self.spec.language,
                files = st.files_since_spawn,
                mem_pressure = st.recycle_pending,
                "recycling plugin process (cached verdict reused — no re-probe)"
            );
            st.sidecar = None;
            st.recycle_pending = false;
        }

        if st.sidecar.is_none() {
            match Sidecar::spawn(&self.spec, &self.repo_root) {
                Ok(sidecar) => {
                    tracing::info!(
                        lang = %self.spec.language,
                        binary = %self.spec.program,
                        "plugin process spawned"
                    );
                    let sidecar = Arc::new(sidecar);
                    st.sidecar = Some(Arc::clone(&sidecar));
                    st.generation += 1;
                    st.files_since_spawn = 0;
                    spawn_watchdog(
                        self.spec.language.clone(),
                        Arc::clone(&self.state),
                        sidecar,
                        st.generation,
                    );
                }
                Err(e) => {
                    let reason = format!(
                        "plugin spawn failed: {e} — language disabled for this run \
                         (ADR-017 Rule 2 fail-closed)"
                    );
                    tracing::warn!(lang = %self.spec.language, binary = %self.spec.program, %reason);
                    st.disabled = Some(reason.clone());
                    return Err(IndexError::Parse {
                        file: format!("plugin:{}", self.spec.language),
                        message: reason,
                    });
                }
            }
        }

        st.files_since_spawn += 1;
        st.last_used = std::time::Instant::now();
        Ok(Arc::clone(st.sidecar.as_ref().expect("sidecar just ensured")))
    }

    /// Record a parse crash; a crash loop disables the language for the run.
    fn record_crash(&self) {
        use crate::sandbox::policy::resource_limits as limits;
        let mut st = self.state.lock().expect("lazy sidecar state poisoned");
        st.sidecar = None; // respawn fresh on the next parse
        st.consecutive_crashes += 1;
        if st.consecutive_crashes >= limits::MAX_CONSECUTIVE_CRASHES {
            st.disabled = Some(format!(
                "plugin crashed {} times in a row — disabled for this run",
                st.consecutive_crashes
            ));
            tracing::warn!(
                lang = %self.spec.language,
                "plugin crash loop — language disabled for this run"
            );
        }
    }
}

/// Per-process supervisor thread (ADR-018 Rules 2–4): samples idle time and
/// RSS every `WATCHDOG_INTERVAL`; exits when its generation is replaced.
fn spawn_watchdog(
    language: String,
    state: Arc<Mutex<LazyState>>,
    sidecar: Arc<Sidecar>,
    my_generation: u64,
) {
    use crate::sandbox::policy::resource_limits as limits;

    std::thread::Builder::new()
        .name(format!("travsr-watchdog-{language}"))
        .spawn(move || loop {
            std::thread::sleep(limits::WATCHDOG_INTERVAL);

            let idle = {
                let st = match state.lock() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                if st.generation != my_generation || st.sidecar.is_none() {
                    return; // replaced or drained — this watchdog is stale
                }
                st.last_used.elapsed()
            };

            // Idle spin-down (Rule 4): release memory; next request respawns
            // on demand using the cached verdict.
            if idle > limits::IDLE_TTL {
                tracing::info!(lang = %language, idle_secs = idle.as_secs(), "plugin idle — spun down");
                sidecar.kill();
                if let Ok(mut st) = state.lock() {
                    if st.generation == my_generation {
                        st.sidecar = None;
                    }
                }
                return;
            }

            // RSS sampling (Rule 2 — the sampled-enforcement path; a
            // sub-interval spike can transiently overshoot, accepted).
            let Some(rss) = sidecar.pid().and_then(sample_rss_bytes) else {
                continue;
            };
            if rss > limits::MEMORY_MAX_BYTES {
                // Kill is SOFT (Rule 5): environmental, never a HARD verdict;
                // the in-flight file fails, the index run continues.
                tracing::warn!(
                    lang = %language,
                    rss_mib = rss / (1024 * 1024),
                    "plugin exceeded memory_max — killed (verdict unaffected)"
                );
                sidecar.kill();
                if let Ok(mut st) = state.lock() {
                    if st.generation == my_generation {
                        st.sidecar = None;
                    }
                }
                return;
            }
            if rss > limits::MEMORY_HIGH_BYTES {
                if let Ok(mut st) = state.lock() {
                    if st.generation == my_generation && !st.recycle_pending {
                        tracing::info!(
                            lang = %language,
                            rss_mib = rss / (1024 * 1024),
                            "plugin over memory_high — recycle scheduled at next parse boundary"
                        );
                        st.recycle_pending = true;
                    }
                }
            }
        })
        .ok();
}

/// Sample a process's resident set size in bytes. Linux reads
/// `/proc/<pid>/statm`; macOS shells out to `ps -o rss=` (KiB). Other
/// platforms return None (watchdog memory enforcement disabled — documented
/// ADR-018 gap; the deadline and recycle-by-count paths still apply).
fn sample_rss_bytes(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        Some(resident_pages * 4096)
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let kib: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(kib * 1024)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

impl Transport for LazySidecar {
    fn parse(&self, req: ParseRequest) -> Result<ParseResponse, IndexError> {
        use crate::sandbox::policy::resource_limits as limits;

        let sidecar = self.acquire()?;

        // ADR-018 parse_deadline: the parse runs on a helper thread; on
        // deadline the child is killed (unblocking the helper) and the file
        // fails without hanging the index run (T15).
        let (tx, rx) = std::sync::mpsc::channel();
        let side = Arc::clone(&sidecar);
        std::thread::spawn(move || {
            let _ = tx.send(side.parse(req));
        });

        match rx.recv_timeout(limits::PARSE_DEADLINE) {
            Ok(Ok(resp)) => {
                if let Ok(mut st) = self.state.lock() {
                    st.consecutive_crashes = 0;
                }
                Ok(resp)
            }
            Ok(Err(e)) => {
                self.record_crash();
                Err(e)
            }
            Err(_) => {
                sidecar.kill();
                if let Ok(mut st) = self.state.lock() {
                    st.sidecar = None; // respawn on next parse; SOFT, not disabled
                }
                Err(IndexError::Parse {
                    file: format!("plugin:{}", self.spec.language),
                    message: format!(
                        "parse exceeded the {}s deadline — plugin killed, file failed \
                         (ADR-018; index run continues)",
                        limits::PARSE_DEADLINE.as_secs()
                    ),
                })
            }
        }
    }

    /// Phase B for sidecar languages routes through the resolver + Phase B
    /// catalog (with its per-language sandbox policy), never through the
    /// Phase A dispatcher registration.
    fn invoke_phase_b(&self, _req: InvokeRequest) -> Result<InvokeResponse, IndexError> {
        Err(IndexError::PhaseNotSupported)
    }

    fn health(&self) -> PluginHealth {
        let st = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return PluginHealth::Disabled("state lock poisoned".into()),
        };
        if let Some(reason) = &st.disabled {
            return PluginHealth::Disabled(reason.clone());
        }
        match &st.sidecar {
            Some(s) => s.health(),
            None => PluginHealth::Ok, // not yet spawned / spun down — healthy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use travsr_plugin_protocol::{InvokeResponse, ParseResponse};

    struct NoOpPlugin;
    impl Plugin for NoOpPlugin {
        fn language(&self) -> travsr_core::LanguageId {
            travsr_core::Language::TypeScript.into()
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

    #[test]
    fn lazy_sidecar_fails_closed_when_binary_missing() {
        let spec = crate::resolver::PluginSpec {
            language: "cpp".into(),
            program: "/nonexistent/travsr-lang-cpp".into(),
            args: vec![],
            policy: crate::sandbox::policy::SandboxPolicy::Standard,
        };
        let lazy = LazySidecar::new(spec, std::path::Path::new("."));

        // Healthy before first use — spawn has not been attempted.
        assert!(matches!(lazy.health(), PluginHealth::Ok));

        let req = ParseRequest {
            path: std::path::PathBuf::from("a.cpp"),
            vname_path: "a.cpp".into(),
            corpus: String::new(),
            package: String::new(),
            source: None,
        };
        // First parse triggers the spawn attempt, which fails (missing binary).
        assert!(lazy.parse(req.clone()).is_err());
        // Failure is cached: disabled from now on, no respawn attempts.
        assert!(matches!(lazy.health(), PluginHealth::Disabled(_)));
        assert!(lazy.parse(req).is_err());
    }
}
