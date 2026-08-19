//! PluginResolver abstraction — unified sandboxed spawn for Phase B providers.
//!
//! Replaces the two hardcoded spawn paths in `indexer.rs`:
//!   (a) Sidecar for builtins (rust/ts) — `<current_exe> __plugin <lang>`
//!   (b) Interim catalog runner — spawns scip-* tools directly, unsandboxed
//!       (ADR-017 violation)
//!
//! After this module is wired in, both paths go through a single `PluginSpec`
//! that carries the program, args, and `SandboxPolicy` needed to apply the
//! correct sandbox before any subprocess is spawned.
//!
//! # Design
//!
//! ```text
//! CompositeResolver
//!   ├── BuiltinResolver   (rust, typescript — compiled into the daemon binary)
//!   └── CatalogResolver   (everything else — travsr-lang-<lang> binaries on PATH)
//! ```
//!
//! `CompositeResolver::resolve` is called once per language per indexing run.
//! First hit wins — guarantees single provider per language, no double-index.
//!
//! Fail-closed per ADR-017 Rule 2: `resolve` returns `None` (and logs) rather
//! than returning an unsandboxed spec or panicking.

use crate::phase_b::catalog::{SandboxRequirement, CATALOG};
use crate::sandbox::policy::SandboxPolicy;
use crate::trust::registered_languages_from_disk;

// ── PluginSpec ────────────────────────────────────────────────────────────────

/// How to spawn one language's Phase B provider, sandboxed.
///
/// - **Builtins** (rust, typescript): `program = current_exe`,
///   `args = ["__plugin", lang]`.
/// - **External** (everything else): `program = travsr-lang-<lang>` binary on
///   PATH, `args = []`.
///
/// The caller (indexer) passes `program` + `args` to `Sidecar::spawn_with_spec`
/// together with `policy` so the correct sandbox is applied before execution.
#[derive(Debug, Clone)]
pub struct PluginSpec {
    /// Canonical language string (matches `CATALOG` and protocol).
    pub language: String,
    /// Absolute path (or PATH-resolved name) of the binary to spawn.
    pub program: String,
    /// Arguments to pass after `program`.
    pub args: Vec<String>,
    /// ADR-017 sandbox policy to apply before spawning.
    pub policy: SandboxPolicy,
}

// ── PluginResolver trait ──────────────────────────────────────────────────────

/// Strategy interface — the daemon depends on this, not on concrete spawn details.
///
/// Implementations are constructed once per indexing run and treated as
/// immutable after construction (reads from disk happen at `new()`).
pub trait PluginResolver: Send + Sync {
    /// Returns `None` if the language is not providable:
    ///   - binary not on PATH (external), or
    ///   - policy validation failed, or
    ///   - `RequiresElevated` without a recorded PSE approval in `lang.toml`.
    ///
    /// Callers log the skip and continue — fail-closed per ADR-017 Rule 2.
    fn resolve(&self, language: &str) -> Option<PluginSpec>;

    /// All languages this resolver can potentially provide (superset of what
    /// `resolve` will return `Some` for — binary-on-PATH is checked inside
    /// `resolve`).
    fn providable_languages(&self) -> Vec<String>;
}

// ── BuiltinResolver ───────────────────────────────────────────────────────────

/// Resolves built-in Phase B providers: spawns `<current_exe> __plugin <lang>`.
///
/// Used ONLY for `rust` and `typescript`, which are compiled directly into the
/// travsr binary. The policy is always `SandboxPolicy::Standard` — builtins do
/// not need network access.
pub struct BuiltinResolver {
    /// Absolute path to the currently-running travsr binary.
    current_exe: String,
    /// Canonical language strings handled by the built-in Phase B path.
    languages: Vec<String>,
}

impl BuiltinResolver {
    /// `current_exe` — path to the running binary (usually `std::env::current_exe()`).
    /// `languages`   — list of builtin phase_b language strings (from `Dispatcher::phase_b_languages()`).
    pub fn new(current_exe: String, languages: Vec<String>) -> Self {
        Self {
            current_exe,
            languages,
        }
    }
}

impl PluginResolver for BuiltinResolver {
    fn resolve(&self, language: &str) -> Option<PluginSpec> {
        if !self.languages.iter().any(|l| l == language) {
            return None;
        }
        Some(PluginSpec {
            language: language.to_string(),
            program: self.current_exe.clone(),
            args: vec!["__plugin".to_string(), language.to_string()],
            policy: SandboxPolicy::Standard,
        })
    }

    fn providable_languages(&self) -> Vec<String> {
        self.languages.clone()
    }
}

// ── CatalogResolver ───────────────────────────────────────────────────────────

/// Resolved entry for one external language, built at construction time.
struct ResolvedEntry {
    language: String,
    /// Absolute path to the binary (already confirmed on PATH at construction).
    program: String,
    policy: SandboxPolicy,
}

/// Resolves external `travsr-lang-<lang>` binaries from the static `CATALOG`.
///
/// Filters by:
/// 1. Language is registered in `~/.travsr/lang.toml` (written by `travsr lang add`).
/// 2. Binary named by `PhaseBEntry::command` is found on PATH.
/// 3. `RequiresElevated` languages: a valid PSE approval record exists in
///    `lang.toml` and passes `SandboxPolicy::validate()`.
///
/// All disk reads and PATH searches happen in `new()` — the resolver is then
/// immutable for the duration of one indexing run.
pub struct CatalogResolver {
    /// Pre-resolved entries keyed by language string.
    entries: Vec<ResolvedEntry>,
    /// H5: languages that are RequiresElevated but have no PSE approval in
    /// lang.toml. Surfaced in PhaseBOutcome so the CLI can print an actionable
    /// "run `travsr lang approve <lang>`" message rather than silently skipping.
    needs_approval: Vec<String>,
    /// #573: languages whose only resolvable provider is an npm `.cmd`/`.bat`
    /// shim with no native binary behind it. Dropping them here alone left the
    /// skip invisible to the user (log line only, no outcome bucket); the
    /// caller surfaces these as skipped_no_analyzer so `travsr init`/`status`
    /// print the `travsr lang install <lang>` hint.
    unresolvable_shims: Vec<String>,
}

impl CatalogResolver {
    /// Languages that were skipped due to missing PSE approval. Caller should
    /// copy these into `PhaseBOutcome::skipped_needs_approval`.
    pub fn needs_approval(&self) -> &[String] {
        &self.needs_approval
    }

    /// Languages skipped because their provider is an npm shim the Windows
    /// sandbox cannot execute (#573). Caller should copy these into
    /// `PhaseBOutcome::skipped_no_analyzer` — the native binary is missing and
    /// `travsr lang install <lang>` is the fix.
    pub fn unresolvable_shims(&self) -> &[String] {
        &self.unresolvable_shims
    }
}

impl CatalogResolver {
    /// Construct by reading `lang.toml` and searching PATH. Fails silently —
    /// missing config or PATH entries produce an empty resolver, not an error.
    pub fn new() -> Self {
        Self::from_disk_impl()
    }

    fn from_disk_impl() -> Self {
        let registered = registered_languages_from_disk();
        // Load lang.toml once for approval lookups.
        let lang_config = load_lang_config();

        let mut entries = Vec::new();
        let mut needs_approval = Vec::new();
        let mut unresolvable_shims = Vec::new();

        tracing::debug!(
            "CatalogResolver: registered languages from disk: {:?}",
            registered
        );

        for catalog_entry in CATALOG {
            let lang = catalog_entry.language;

            // Only external providers (provider_binary = Some). Builtins (rust,
            // typescript) are handled by BuiltinResolver — skip them here to avoid
            // double-indexing.
            let binary_name = match catalog_entry.provider_binary {
                Some(b) => b,
                None => continue,
            };

            // Must be registered by the user.
            if !registered.iter().any(|r| r == lang) {
                tracing::debug!(
                    lang,
                    "CatalogResolver: '{}' not in registered list, skipping",
                    lang
                );
                continue;
            }

            tracing::debug!(
                lang,
                binary = binary_name,
                "CatalogResolver: searching PATH for binary"
            );

            // The travsr-lang-<lang> binary must be on PATH.
            let Some(program) = which_binary(binary_name) else {
                tracing::info!(
                    lang,
                    binary = binary_name,
                    "Phase B catalog: binary not on PATH, skipping (install: {})",
                    catalog_entry.install_hint
                );
                continue;
            };

            tracing::debug!(lang, program = %program, "CatalogResolver: binary found");

            // #573: since #502 the PATHEXT-aware probe can resolve a provider
            // that exists only as an npm `.cmd` shim. The AppContainer spawn
            // runs PE images only (`CreateProcessW`, no `cmd.exe /c`), so a
            // shim handed to it fails and the language reports as crashed.
            // Resolve the shim to the packaged native binary npm installs next
            // to it; when there is none, skip the provider with an actionable
            // hint instead of spawning a guaranteed failure.
            let program = match resolve_npm_cmd_shim(
                std::path::Path::new(&program),
                catalog_entry.npm_package,
            ) {
                ShimResolution::NotAShim => program,
                ShimResolution::Exe(exe) => {
                    tracing::info!(
                        lang,
                        shim = %program,
                        exe = %exe.display(),
                        "Phase B catalog: npm shim resolved to its packaged native binary (#573)"
                    );
                    exe.to_string_lossy().into_owned()
                }
                ShimResolution::Unresolvable => {
                    tracing::warn!(
                        lang,
                        "Phase B catalog: provider '{}' for '{}' is installed as an npm \
                         shim ('{}'), which the Windows sandbox cannot execute — install \
                         the native binary via `travsr lang install {}`",
                        binary_name,
                        lang,
                        program,
                        lang
                    );
                    // Recorded so the caller can surface the skip in the
                    // outcome — a log line alone left it invisible to the user.
                    unresolvable_shims.push(lang.to_string());
                    continue;
                }
            };

            // Determine sandbox policy.
            let policy = match catalog_entry.sandbox {
                SandboxRequirement::Standard => SandboxPolicy::Standard,

                // NativeIpc: tool needs POSIX IPC queues/shm (e.g. scip-clang) but
                // not network. macOS sandbox-exec has no valid Seatbelt operation for
                // mq_open, so we skip sandbox-exec and rely on ulimit caps only.
                // No PSE approval required — this is a structural constraint, not a
                // network exception.
                SandboxRequirement::NativeIpc => SandboxPolicy::NativeIpc,

                SandboxRequirement::RequiresElevated => {
                    // Must have a recorded PSE approval in lang.toml.
                    // If the user provided an explicit permitted_hosts override in
                    // lang.toml, use that; otherwise fall back to the catalog
                    // defaults from `entry.elevated_hosts`. Either way the
                    // approved_by/approved_date fields must be non-empty.
                    let Some(approval) =
                        lang_config.as_ref().and_then(|cfg| cfg.get_approval(lang))
                    else {
                        tracing::info!(
                            lang,
                            "'{}' needs network access during indexing but has no security \
                             approval on file — skipping (its semantic analysis stays disabled \
                             until it is approved). Run: travsr lang approve {} \
                             --approved-by <approver-github-handle> --reason \"...\" \
                             --permitted-hosts <hosts>",
                            lang,
                            lang
                        );
                        needs_approval.push(lang.to_string());
                        continue;
                    };

                    // Use the user-supplied hosts if non-empty; otherwise fall back
                    // to the catalog-defined defaults for this language.
                    let permitted_hosts = if !approval.permitted_hosts.is_empty() {
                        approval.permitted_hosts.clone()
                    } else {
                        catalog_entry
                            .elevated_hosts
                            .iter()
                            .map(|h| h.to_string())
                            .collect()
                    };

                    let policy = SandboxPolicy::Elevated {
                        permitted_hosts,
                        reason: approval.reason.clone(),
                        approved_by: approval.approved_by.clone(),
                        approved_date: approval.approved_date.clone(),
                    };

                    // Validate the Elevated policy fields per ADR-017 Rule 1.
                    // This catches empty approved_by / approved_date.
                    if let Err(e) = policy.validate() {
                        tracing::warn!(
                            lang,
                            "the security approval for '{}' is incomplete: {} \
, skipping (its semantic analysis stays disabled)",
                            lang,
                            e
                        );
                        continue;
                    }

                    policy
                }
            };

            entries.push(ResolvedEntry {
                language: lang.to_string(),
                program,
                policy,
            });
        }

        Self {
            entries,
            needs_approval,
            unresolvable_shims,
        }
    }
}

impl Default for CatalogResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginResolver for CatalogResolver {
    fn resolve(&self, language: &str) -> Option<PluginSpec> {
        let entry = self.entries.iter().find(|e| e.language == language)?;
        Some(PluginSpec {
            language: entry.language.clone(),
            program: entry.program.clone(),
            args: vec![],
            policy: entry.policy.clone(),
        })
    }

    fn providable_languages(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.language.clone()).collect()
    }
}

// ── CompositeResolver ─────────────────────────────────────────────────────────

/// Chain of resolvers — `BuiltinResolver` first, then `CatalogResolver`.
///
/// `resolve` iterates in order and returns the first `Some`. This guarantees
/// a single provider per language: builtins always shadow catalog entries for
/// `rust` and `typescript`, avoiding double-indexing.
///
/// `providable_languages` returns the union of all resolvers, deduplicated.
pub struct CompositeResolver {
    resolvers: Vec<Box<dyn PluginResolver>>,
}

impl CompositeResolver {
    pub fn new(resolvers: Vec<Box<dyn PluginResolver>>) -> Self {
        Self { resolvers }
    }
}

impl PluginResolver for CompositeResolver {
    fn resolve(&self, language: &str) -> Option<PluginSpec> {
        for resolver in &self.resolvers {
            if let Some(spec) = resolver.resolve(language) {
                return Some(spec);
            }
        }
        None
    }

    fn providable_languages(&self) -> Vec<String> {
        // Union of all resolver languages, deduplicated (preserve insertion order).
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for resolver in &self.resolvers {
            for lang in resolver.providable_languages() {
                if seen.insert(lang.clone()) {
                    result.push(lang);
                }
            }
        }
        result
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Search PATH for a binary with the given name. Returns the absolute path as
/// a `String` if found, or `None` if not on PATH. Mirrors the `which()` helper
/// in `crates/travsr-cli/src/lang.rs` lines 398-401.
///
/// `~/.travsr/bin` is always prepended to the search path so that binaries
/// installed by `travsr lang install` (e.g. `travsr-lang-java`) are found even
/// when the daemon was launched with a stripped PATH (launchd, GUI launch, etc.).
fn which_binary(name: &str) -> Option<String> {
    let host_path = std::env::var_os("PATH").unwrap_or_default();
    let travsr_bin = dirs::home_dir().map(|h| h.join(".travsr").join("bin"));

    // Build the augmented search list: ~/.travsr/bin first, then host PATH.
    let mut search_dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(tb) = travsr_bin {
        search_dirs.push(tb);
    }
    search_dirs.extend(std::env::split_paths(&host_path));

    // #502: PATHEXT-aware resolution — on Windows the providers are
    // scip-go.exe / travsr-lang-java.exe, which a bare-name probe misses.
    travsr_core::exec::resolve_executable_in(search_dirs, name)
        .map(|p| p.to_string_lossy().into_owned())
}

// ── npm .cmd shim resolution (#573) ──────────────────────────────────────────

/// How a resolved provider path relates to the Windows npm-shim problem (#573).
#[derive(Debug)]
enum ShimResolution {
    /// Not a `.cmd`/`.bat` shim — spawnable as-is.
    NotAShim,
    /// The shim's packaged native binary, spawnable by the AppContainer.
    Exe(std::path::PathBuf),
    /// A shim with no packaged native binary next to it. `CreateProcessW`
    /// cannot execute batch scripts, so handing this to the sandbox spawn is a
    /// guaranteed crash — the caller should skip it with an actionable hint.
    Unresolvable,
}

/// Resolve an npm `.cmd`/`.bat` shim to the real native binary it wraps.
///
/// Two strategies, mirroring the vscode extension's `resolveNpmShimExe`
/// (installer.ts #486), which does the same for travsr's own npm package:
///
/// 1. npm's cmd-shim embeds its target as a `%~dp0`- or `%dp0%`-relative
///    quoted path. When that target is an existing PE image, spawn it
///    directly.
/// 2. House packaging convention: the native binary ships at
///    `<shim dir>/node_modules/<npm package>/bin/<binary>.exe`.
///
/// Deliberately does NOT fall back to running JS entry points via `node.exe`:
/// a provider whose only artifact is a script has no PE for the AppContainer
/// to execute, and speculatively spawning node inside the sandbox is exactly
/// the kind of silent behavior change ADR-017 gates. Those providers surface
/// the actionable skip instead.
fn resolve_npm_cmd_shim(program: &std::path::Path, npm_package: Option<&str>) -> ShimResolution {
    let is_shim = program
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    if !is_shim {
        return ShimResolution::NotAShim;
    }
    let Some(dir) = program.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return ShimResolution::Unresolvable;
    };

    // Strategy 1: the target path embedded in the shim script itself.
    if let Ok(text) = std::fs::read_to_string(program) {
        for token in ["%~dp0", "%dp0%"] {
            if let Some(exe) = embedded_shim_target(&text, token, dir) {
                return ShimResolution::Exe(exe);
            }
        }
    }

    // Strategy 2: the packaging convention for travsr npm packages.
    if let (Some(pkg), Some(stem)) = (npm_package, program.file_stem()) {
        let mut candidate = dir.join("node_modules");
        for seg in pkg.split('/') {
            candidate.push(seg);
        }
        candidate.push("bin");
        let mut name = stem.to_os_string();
        name.push(".exe");
        candidate.push(name);
        if candidate.is_file() {
            return ShimResolution::Exe(candidate);
        }
    }

    ShimResolution::Unresolvable
}

/// Extract the first existing PE image referenced in `text` as a quoted
/// `<token>`-relative path (npm cmd-shims quote their targets, so the path
/// runs from the token to the closing quote or end of line).
fn embedded_shim_target(
    text: &str,
    token: &str,
    dir: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let mut search_from = 0;
    while let Some(pos) = text[search_from..].find(token) {
        let rel_start = search_from + pos + token.len();
        let rest = &text[rel_start..];
        let rel_end = rest.find(['"', '\r', '\n']).unwrap_or(rest.len());
        let rel = rest[..rel_end].trim_start_matches(['\\', '/']);
        if !rel.is_empty() {
            let candidate = dir.join(rel);
            let is_pe = candidate
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("com"));
            if is_pe && candidate.is_file() {
                return Some(candidate);
            }
        }
        search_from = rel_start + rel_end.max(1);
        if search_from >= text.len() {
            break;
        }
    }
    None
}

// ── lang.toml approval reader ─────────────────────────────────────────────────
//
// We need the PSE approval fields (permitted_hosts, approved_by, reason,
// approved_date) from lang.toml to construct a SandboxPolicy::Elevated.
// Rather than depending on travsr-cli (which would create a crate cycle),
// we have a minimal private reader here that mirrors the `LangConfig` /
// `ElevatedApproval` structs from that crate.

#[derive(Debug, serde::Deserialize)]
struct LangConfigFile {
    #[serde(default)]
    elevated_approvals: Vec<ElevatedApprovalRecord>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ElevatedApprovalRecord {
    language: String,
    approved_by: String,
    reason: String,
    permitted_hosts: Vec<String>,
    approved_date: String,
}

impl LangConfigFile {
    fn get_approval(&self, language: &str) -> Option<&ElevatedApprovalRecord> {
        self.elevated_approvals
            .iter()
            .find(|a| a.language == language)
    }
}

fn load_lang_config() -> Option<LangConfigFile> {
    // TRAVSR_LANG_TOML overrides the path — used in tests to avoid reading the
    // real ~/.travsr/lang.toml and making tests dependent on local machine state.
    let path = if let Ok(p) = std::env::var("TRAVSR_LANG_TOML") {
        std::path::PathBuf::from(p)
    } else {
        let home = dirs::home_dir()?;
        home.join(".travsr").join("lang.toml")
    };
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── BuiltinResolver ───────────────────────────────────────────────────────

    #[test]
    fn builtin_resolves_known_language() {
        let resolver = BuiltinResolver::new(
            "/usr/local/bin/travsr".to_string(),
            vec!["rust".to_string(), "typescript".to_string()],
        );
        let spec = resolver.resolve("rust").expect("rust must resolve");
        assert_eq!(spec.language, "rust");
        assert_eq!(spec.program, "/usr/local/bin/travsr");
        assert_eq!(spec.args, vec!["__plugin", "rust"]);
        assert!(matches!(spec.policy, SandboxPolicy::Standard));
    }

    #[test]
    fn builtin_returns_none_for_unknown_language() {
        let resolver = BuiltinResolver::new(
            "/usr/local/bin/travsr".to_string(),
            vec!["rust".to_string()],
        );
        assert!(resolver.resolve("go").is_none());
    }

    #[test]
    fn builtin_providable_languages_matches_input() {
        let langs = vec!["rust".to_string(), "typescript".to_string()];
        let resolver = BuiltinResolver::new("/bin/travsr".to_string(), langs.clone());
        assert_eq!(resolver.providable_languages(), langs);
    }

    // ── CompositeResolver ─────────────────────────────────────────────────────

    #[test]
    fn composite_first_hit_wins() {
        // Builtin covers rust; catalog would also cover rust if it were present.
        // Composite must return the builtin spec (first resolver wins).
        let builtin: Box<dyn PluginResolver> = Box::new(BuiltinResolver::new(
            "/usr/local/bin/travsr".to_string(),
            vec!["rust".to_string()],
        ));
        // A second resolver that also claims rust (simulated with another BuiltinResolver).
        let shadow: Box<dyn PluginResolver> = Box::new(BuiltinResolver::new(
            "/other/travsr".to_string(),
            vec!["rust".to_string()],
        ));
        let composite = CompositeResolver::new(vec![builtin, shadow]);
        let spec = composite
            .resolve("rust")
            .expect("composite must resolve rust");
        assert_eq!(spec.program, "/usr/local/bin/travsr", "first resolver wins");
    }

    #[test]
    fn composite_falls_through_to_second_resolver() {
        let builtin: Box<dyn PluginResolver> = Box::new(BuiltinResolver::new(
            "/usr/local/bin/travsr".to_string(),
            vec!["rust".to_string()],
        ));
        let catalog: Box<dyn PluginResolver> = Box::new(BuiltinResolver::new(
            "/usr/local/bin/scip-go".to_string(),
            vec!["go".to_string()],
        ));
        let composite = CompositeResolver::new(vec![builtin, catalog]);
        let spec = composite
            .resolve("go")
            .expect("composite must fall through to go");
        assert_eq!(spec.program, "/usr/local/bin/scip-go");
    }

    #[test]
    fn composite_providable_languages_is_deduped_union() {
        let r1: Box<dyn PluginResolver> = Box::new(BuiltinResolver::new(
            "/bin/travsr".to_string(),
            vec!["rust".to_string(), "typescript".to_string()],
        ));
        let r2: Box<dyn PluginResolver> = Box::new(BuiltinResolver::new(
            "/bin/travsr".to_string(),
            vec!["typescript".to_string(), "go".to_string()],
        ));
        let composite = CompositeResolver::new(vec![r1, r2]);
        let langs = composite.providable_languages();
        // typescript appears in both — must be deduplicated.
        assert_eq!(langs.iter().filter(|l| *l == "typescript").count(), 1);
        // All three languages must be present.
        assert!(langs.contains(&"rust".to_string()));
        assert!(langs.contains(&"typescript".to_string()));
        assert!(langs.contains(&"go".to_string()));
    }

    #[test]
    fn composite_returns_none_for_unrecognised_language() {
        let builtin: Box<dyn PluginResolver> = Box::new(BuiltinResolver::new(
            "/bin/travsr".to_string(),
            vec!["rust".to_string()],
        ));
        let composite = CompositeResolver::new(vec![builtin]);
        assert!(composite.resolve("cobol").is_none());
    }

    // ── which_binary ──────────────────────────────────────────────────────────

    #[test]
    #[cfg(unix)]
    fn which_binary_finds_sh() {
        // /bin/sh is present on every POSIX system; skip on Windows.
        let result = which_binary("sh");
        assert!(result.is_some(), "sh must be on PATH");
    }

    #[test]
    fn which_binary_returns_none_for_nonexistent() {
        let result = which_binary("__travsr_nonexistent_binary_xyz__");
        assert!(result.is_none());
    }

    // ── npm .cmd shim resolution (#573) ──────────────────────────────────────
    // Pure path/text logic — runs on every platform even though only the
    // Windows PATHEXT probe can produce a .cmd hit in production.

    #[test]
    fn non_shim_paths_pass_through_untouched() {
        for name in ["travsr-lang-go.exe", "travsr-lang-go", "scip-go.EXE"] {
            let p = std::path::Path::new(name);
            assert!(
                matches!(
                    resolve_npm_cmd_shim(p, Some("@travsr-plugin/go")),
                    ShimResolution::NotAShim
                ),
                "{name} is not a shim"
            );
        }
    }

    #[test]
    fn shim_with_embedded_dp0_exe_target_resolves_to_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target_rel = std::path::Path::new("node_modules")
            .join("@travsr-plugin")
            .join("go")
            .join("bin")
            .join("real-provider.exe");
        let target = dir.path().join(&target_rel);
        std::fs::create_dir_all(target.parent().unwrap()).expect("mk target dir");
        std::fs::write(&target, b"MZ").expect("write target");

        // The two generations of npm's cmd-shim template.
        for token in ["%~dp0", "%dp0%"] {
            let shim = dir.path().join("travsr-lang-go.cmd");
            std::fs::write(
                &shim,
                format!(
                    "@ECHO off\r\nSETLOCAL\r\n\"{token}\\{}\"   %*\r\n",
                    target_rel.display()
                ),
            )
            .expect("write shim");
            match resolve_npm_cmd_shim(&shim, None) {
                ShimResolution::Exe(exe) => assert_eq!(exe, target, "token {token}"),
                other => panic!("token {token}: expected Exe, got {other:?}"),
            }
        }
    }

    #[test]
    fn shim_ignores_non_pe_targets_like_node_scripts() {
        // A JS-only provider: the shim points node at a script. There is no PE
        // to spawn, so this must be Unresolvable — never "run node.exe" (see
        // resolve_npm_cmd_shim docs) and never the raw shim.
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("cli.js");
        std::fs::write(&script, b"//").expect("write script");
        let shim = dir.path().join("travsr-lang-go.cmd");
        std::fs::write(&shim, "@ECHO off\r\nnode \"%~dp0\\cli.js\" %*\r\n").expect("write shim");
        assert!(matches!(
            resolve_npm_cmd_shim(&shim, None),
            ShimResolution::Unresolvable
        ));
    }

    #[test]
    fn unparseable_shim_falls_back_to_the_packaging_convention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir
            .path()
            .join("node_modules")
            .join("@travsr-plugin")
            .join("go")
            .join("bin")
            .join("travsr-lang-go.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).expect("mk bin dir");
        std::fs::write(&exe, b"MZ").expect("write exe");
        let shim = dir.path().join("travsr-lang-go.cmd");
        std::fs::write(&shim, "@ECHO off\r\nrem no dp0 reference here\r\n").expect("write shim");

        match resolve_npm_cmd_shim(&shim, Some("@travsr-plugin/go")) {
            ShimResolution::Exe(found) => assert_eq!(found, exe),
            other => panic!("expected the conventional sibling exe, got {other:?}"),
        }
    }

    #[test]
    fn shim_with_no_native_binary_is_unresolvable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("travsr-lang-go.bat");
        std::fs::write(&shim, "@ECHO off\r\n").expect("write shim");
        assert!(matches!(
            resolve_npm_cmd_shim(&shim, Some("@travsr-plugin/go")),
            ShimResolution::Unresolvable
        ));
    }
}
