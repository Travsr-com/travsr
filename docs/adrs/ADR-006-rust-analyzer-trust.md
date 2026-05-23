# ADR-006: rust-analyzer Subprocess Trust Model

**Date:** 2026-05-23
**Status:** Proposed
**Issue:** #122
**Phase:** 3 (Sprint 9)
**Author:** Principal Architect
**Supersedes:** N/A
**Related:** ADR-005 (per-language corpus naming), RFC-003 (multi-language indexer architecture)

---

## Context

Sprint 9 adds deep Rust semantic edges (call graphs, type references, CFG/DFG) by invoking `rust-analyzer --lsif` against indexed Rust repos. Running rust-analyzer is equivalent to running `cargo check`: it drives the compiler, which executes `build.rs` scripts and procedural macros. Both are **arbitrary code** that the indexer does not author and cannot audit.

A developer using Travsr to index an external dependency or an open-source repo they are reviewing — not running — is implicitly granting that repo's build scripts access to their machine. This is the same threat that `cargo audit` and supply-chain tooling address; Travsr must not make the attack surface larger.

This ADR decides:

1. Default user opt-in policy
2. Sandbox technology stack per OS
3. Network egress policy during indexing
4. Filesystem access policy during indexing
5. Resource limits (time + memory)
6. Fallback mode when sandboxing is unavailable or fails

---

## Decision

### Rule 1 — Default policy: opt-in, per-repo

rust-analyzer LSIF indexing is **disabled by default**. To enable it for a repo, the user must run:

```sh
travsr config set rust-analyzer.trust <repo-path> true
```

This writes a signed entry to `.travsr/config.toml` in the user's home directory (not in the indexed repo). The daemon never enables LSIF indexing based on content found inside the repo itself (e.g. a `.travsr.json` committed to the repo) — that would allow a malicious repo to self-authorize code execution.

**Rationale:** The cost of a false negative (missing semantic edges) is a degraded but correct graph. The cost of a false positive (running arbitrary code on a developer's machine without consent) is a security incident. Opt-in is the only safe default.

Tree-sitter structural indexing (`rust.rs`, Sprint 8) is **not** affected by this rule — it is a pure parser with no subprocess execution.

### Rule 2 — Sandbox technology stack

| Platform | Primary sandbox | Fallback (sandbox unavailable) |
|---|---|---|
| Linux | bubblewrap (`bwrap`) | seccomp-only via `prctl(PR_SET_SECCOMP)` |
| macOS | `sandbox-exec` with a deny-all profile | no fallback — refuse to run |
| Windows | AppContainer (via Job Objects) | no fallback — refuse to run |

The daemon checks for sandbox availability at startup and logs a warning if the primary sandbox is absent. If neither primary nor fallback is available and the user has enabled LSIF indexing for a repo, the daemon logs an error and skips LSIF for that repo rather than running unsandboxed.

**Linux bubblewrap invocation:**

```sh
bwrap \
  --ro-bind /usr /usr \
  --ro-bind /etc /etc \
  --ro-bind /nix /nix \           # omit if not present
  --ro-bind <repo-path> <repo-path> \
  --bind <scratch-dir> <scratch-dir> \
  --unshare-net \
  --unshare-pid \
  --unshare-ipc \
  --die-with-parent \
  rust-analyzer lsif <repo-path>
```

The `<scratch-dir>` is a per-repo temporary directory under `$XDG_CACHE_HOME/travsr/lsif/<corpus-hash>/` that rust-analyzer uses for its own target/incremental cache. It is the only writable location.

**macOS `sandbox-exec` profile:**

```scheme
(version 1)
(deny default)
(allow file-read*)
(allow file-write* (subpath "<scratch-dir>"))
(deny network*)
(allow process-exec)
(allow sysctl-read)
```

**Windows AppContainer:** Implemented via `CreateAppContainerProfile` with `SECURITY_CAPABILITY_INTERNET_CLIENT` and `SECURITY_CAPABILITY_PRIVATE_NETWORK_CLIENT_SERVER` capabilities both omitted (no network). The job object enforces CPU time and memory limits.

### Rule 3 — Network egress: deny by default

All network access is denied during rust-analyzer execution. This prevents:

- Exfiltration of source code or environment variables via HTTP
- Dependency fetching during indexing (cargo-fetch must have been run prior to indexing)
- DNS-based exfiltration

If a build script requires network access (uncommon, but legal in Cargo), indexing will fail. The error is surfaced to the user as:

```
LSIF indexing failed for <repo>: build script requires network access.
Run `cargo fetch` in the repo, or disable LSIF indexing for this repo.
```

The `--offline` flag is passed to rust-analyzer where the CLI supports it, as a defense-in-depth measure independent of the OS-level network denial.

### Rule 4 — Filesystem access policy

| Location | Access |
|---|---|
| `<repo-path>` (read-only) | Full read: source, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` |
| `<scratch-dir>` | Read-write: rust-analyzer target cache, LSIF output file |
| `~/.cargo/registry` (read-only) | Required for dependency sources (pre-fetched) |
| `~/.cargo/bin` (read-only) | Required to locate `rustc`, `rust-analyzer` |
| `~/.rustup` (read-only) | Required for toolchain discovery |
| All other paths | Deny |

Writing to the repo itself is forbidden. A build script that attempts to modify source files will be killed by the sandbox.

### Rule 5 — Resource limits

| Resource | Limit |
|---|---|
| Wall-clock time | 10 minutes per repo (configurable: `rust-analyzer.timeout-secs`) |
| CPU time | Same as wall-clock (no sleeping allowed via `SIGXCPU` on Linux) |
| Memory | 4 GB RSS (configurable: `rust-analyzer.memory-limit-mb`; default 4096) |
| Output file size | 500 MB LSIF file; abort if exceeded |

If the time or memory limit is exceeded, the daemon kills the subprocess, discards the partial LSIF output, and logs a warning. The existing Tree-sitter structural graph remains intact.

Limits are enforced via:
- **Linux:** `RLIMIT_CPU`, `RLIMIT_AS` via `setrlimit(2)` in the child before exec, plus a watchdog thread in the daemon
- **macOS:** Job object equivalent via `setrlimit` + `dispatch_after` watchdog
- **Windows:** Job object `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`

### Rule 6 — `--no-build-scripts` escape hatch

If sandboxing fails at the OS level (e.g. bubblewrap not installed, `sandbox-exec` profile load error, AppContainer API unavailable), the daemon may be configured to fall back to:

```sh
RUSTFLAGS="--cap-lints allow" rust-analyzer lsif --no-build-scripts <repo-path>
```

This mode skips build script and proc-macro execution. Semantic edges that depend on proc-macro-generated types (e.g. `#[derive(Serialize)]`) will be missing, but no arbitrary code runs.

This fallback is **never the default**: it requires explicit configuration:

```sh
travsr config set rust-analyzer.allow-unsandboxed-no-build-scripts true
```

The user must acknowledge the risk by setting this flag explicitly. The daemon logs a prominent warning on every indexing run in this mode.

---

## Threat Model

| Threat | Mitigation |
|---|---|
| Malicious `build.rs` exfiltrates env vars / source | Network deny + read-only repo mount |
| `build.rs` writes a backdoor binary to `$HOME/.cargo/bin` | Filesystem deny outside `<scratch-dir>` |
| `build.rs` forks a persistent background process | `--unshare-pid` (Linux) / AppContainer process isolation |
| Proc-macro crashes rust-analyzer (OOM) | 4 GB memory limit; daemon survives subprocess death |
| Proc-macro runs for hours (DoS) | 10-minute wall-clock limit |
| Malicious repo commits `.travsr.json` to self-authorize LSIF | Trust config lives outside the repo; only user-side config is honored |
| User unknowingly trusts a repo via `travsr config set` | Trust is per-repo-path, not global; explicit opt-in required each time |

---

## Consequences

### Positive

- **No silent code execution:** Users control which repos get LSIF indexing. Travsr cannot be used as an attack vector by a malicious repo.
- **Deep semantic edges where it matters:** Trusted repos (e.g. the user's own workspace) get full call graphs and type references.
- **Portable:** Bubblewrap + sandbox-exec + AppContainer cover the full set of developer platforms; seccomp/no-sandbox fallbacks degrade gracefully.
- **Tree-sitter graph is always available:** Structural edges from Sprint 8 are unaffected by this policy.

### Negative

- **Setup friction:** `bwrap` must be installed on Linux; it is not present by default on all distributions (Debian/Ubuntu ship it; some minimal images do not). The daemon surfaces a clear install instruction.
- **macOS SIP interaction:** `sandbox-exec` is deprecated in macOS 13+ and may be removed. If Apple removes it, the macOS fallback becomes "refuse to run LSIF" until a Virtualization.framework-based sandbox is implemented (tracked as a separate issue).
- **Windows complexity:** AppContainer requires the daemon to run as a non-elevated user. Elevated (Administrator) processes cannot create AppContainers. CI/CD environments that run as Administrator will fall back to `--no-build-scripts` mode or skip LSIF entirely.

---

## Implementation Notes

- Sandbox invocation lives in `travsr-daemon` (`crates/travsr-daemon/src/sandbox/`), not in `travsr-indexer`. The indexer only consumes the LSIF output file; it has no knowledge of how the file was produced.
- `travsr-indexer` already accepts an LSIF file path via `ingest_lsif()` — no indexer changes are needed for Sprint 9.
- The trust config key is `rust-analyzer.trust.<canonical-corpus>` using the ARCH-102 corpus form (`github.com/org/repo`). The `canonical_corpus()` function in `travsr-core` is the source of truth.
- Monitoring: the daemon emits a `tracing::warn!` span for every sandboxed invocation with fields `repo`, `duration_ms`, `exit_code`. The span is visible in structured logs and will feed the future telemetry pipeline.
