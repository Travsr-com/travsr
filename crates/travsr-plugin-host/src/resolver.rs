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
                    "CatalogResolver: '{}' not in registered list — skipping",
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
                    "Phase B catalog: binary not on PATH — skipping (install: {})",
                    catalog_entry.install_hint
                );
                continue;
            };

            tracing::debug!(lang, program = %program, "CatalogResolver: binary found");

            // Determine sandbox policy.
            let policy = match catalog_entry.sandbox {
                SandboxRequirement::Standard => SandboxPolicy::Standard,

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
                            "Phase B catalog: '{}' is RequiresElevated but no PSE approval \
                             recorded in lang.toml — skipping (fail-closed, ADR-017 Rule 2). \
                             Run: travsr lang approve {} --approved-by <pse> --reason \"...\" \
                             --permitted-hosts <hosts>",
                            lang,
                            lang
                        );
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
                            "Phase B catalog: Elevated policy validation failed for '{}': {} \
                             — skipping (fail-closed, ADR-017 Rule 2)",
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

        Self { entries }
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

    search_dirs
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
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
}
