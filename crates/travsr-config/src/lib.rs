//! Layered, typed configuration for Travsr (WS1 of the reindex-governance epic, #422).
//!
//! # Layers (highest precedence first)
//! ```text
//! CLI flag  >  env var  >  per-repo .travsr/config.toml  >  global ~/.travsr/config.toml  >  built-in default
//! ```
//!
//! # Design
//! - **Pure resolver** ([`resolve`]) — no I/O; callers read files/env at the edges
//!   and pass `Option`s in. Deterministic and unit-testable (same inputs → same
//!   result), mirroring `derive_num_workers_inner` in `travsr-plugin-host`.
//! - **Key registry** ([`KEYS`]) — every configurable key declares a validator, an
//!   env-var mapping, a description, and a default. `set` validates against it;
//!   unknown keys are rejected on `set` but **ignored on load** (forward/back-compat).
//! - **Malformed files never brick a command** — a corrupt `config.toml` is logged
//!   and treated as empty.
//!
//! WS1 lands this framework and registers the three embed governance keys. WS2
//! (#419) consumes the resolved values in the reindex spawn path.

#![forbid(unsafe_code)]

use anyhow::{bail, Context as _, Result};
use std::path::{Path, PathBuf};

// ── Layered resolution (pure) ───────────────────────────────────────────────

/// Which layer supplied a resolved value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Cli,
    Env,
    RepoConfig,
    GlobalConfig,
    Default,
}

impl Source {
    /// Short human label for status output.
    pub fn label(self) -> &'static str {
        match self {
            Source::Cli => "cli",
            Source::Env => "env",
            Source::RepoConfig => "repo config",
            Source::GlobalConfig => "global config",
            Source::Default => "default",
        }
    }
}

/// A resolved value plus the layer that supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved<T> {
    pub value: T,
    pub source: Source,
}

/// Pure precedence resolution: `CLI > env > per-repo > global > default`.
///
/// No I/O — the caller reads each layer at the edges and passes `Option`s in.
/// The first `Some` wins; if all are `None` the `default` is used and the source
/// is [`Source::Default`]. Unit-testable and deterministic.
pub fn resolve<T>(
    cli: Option<T>,
    env: Option<T>,
    repo: Option<T>,
    global: Option<T>,
    default: T,
) -> Resolved<T> {
    if let Some(value) = cli {
        Resolved {
            value,
            source: Source::Cli,
        }
    } else if let Some(value) = env {
        Resolved {
            value,
            source: Source::Env,
        }
    } else if let Some(value) = repo {
        Resolved {
            value,
            source: Source::RepoConfig,
        }
    } else if let Some(value) = global {
        Resolved {
            value,
            source: Source::GlobalConfig,
        }
    } else {
        Resolved {
            value: default,
            source: Source::Default,
        }
    }
}

// ── Key registry ────────────────────────────────────────────────────────────

/// A configurable key: a dotted `section.name`, its documentation, the env var
/// that overrides it, a default shown when unset, and a validator that both
/// checks a user-supplied string and normalises it to the value stored on disk.
pub struct KeySpec {
    /// Dotted key, e.g. `"embed.capacity"`.
    pub key: &'static str,
    /// One-line human description (shown by `travsr config list`).
    pub description: &'static str,
    /// Environment variable that overrides this key, if any.
    pub env: Option<&'static str>,
    /// Value shown by `config list` when the key is unset at every layer.
    pub default_display: &'static str,
    /// Validate + normalise a user value into the canonical stored form.
    /// Returns a human-readable error on bad input. Never panics.
    validate: fn(&str) -> Result<toml::Value>,
}

/// All known keys. WS1 registers the three embed governance keys; WS2 (#419)
/// consumes them in the reindex path. Adding a key is a single entry here.
pub static KEYS: &[KeySpec] = &[
    KeySpec {
        key: "embed.capacity",
        description:
            "Embedding worker budget: `auto` (load-adaptive) or a percent 1-100. 100 = full speed.",
        env: Some("TRAVSR_EMBED_CAPACITY"),
        default_display: "100",
        validate: validate_capacity,
    },
    KeySpec {
        key: "embed.max_workers",
        description: "Absolute hard cap on embedding reader threads (>= 1). Overrides capacity.",
        env: Some("TRAVSR_EMBED_WORKERS"),
        default_display: "auto",
        validate: validate_positive_int,
    },
    KeySpec {
        key: "embed.priority",
        description: "OS scheduling priority for the embed sidecar: normal | low | idle.",
        env: Some("TRAVSR_EMBED_PRIORITY"),
        default_display: "normal",
        validate: validate_priority,
    },
];

/// Look up a key spec by its dotted name.
pub fn spec(key: &str) -> Option<&'static KeySpec> {
    KEYS.iter().find(|k| k.key == key)
}

/// Capacity accepts the adaptive sentinel `auto` or an integer percent 1-100.
/// `auto` is stored verbatim; the reindex path resolves it to a load-adjusted
/// worker count at each spawn (WS5).
fn validate_capacity(s: &str) -> Result<toml::Value> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("auto") {
        return Ok(toml::Value::String("auto".to_string()));
    }
    let n: i64 = t
        .parse()
        .map_err(|_| anyhow::anyhow!("expected `auto` or an integer 1-100, got '{s}'"))?;
    if !(1..=100).contains(&n) {
        bail!("capacity must be `auto` or between 1 and 100 (percent), got {n}");
    }
    Ok(toml::Value::Integer(n))
}

fn validate_positive_int(s: &str) -> Result<toml::Value> {
    let n: i64 = s
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("expected a positive integer, got '{s}'"))?;
    if n < 1 {
        bail!("value must be >= 1, got {n}");
    }
    Ok(toml::Value::Integer(n))
}

fn validate_priority(s: &str) -> Result<toml::Value> {
    match s.trim() {
        "normal" | "low" | "idle" => Ok(toml::Value::String(s.trim().to_string())),
        other => bail!("priority must be one of: normal, low, idle (got '{other}')"),
    }
}

// ── File layers ───────────────────────────────────────────────────────────────

/// Global config path: `~/.travsr/config.toml`. `None` when the home dir is
/// undiscoverable (rare; e.g. no `$HOME`).
pub fn global_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".travsr").join("config.toml"))
}

/// Per-repo config path: `<repo_root>/.travsr/config.toml`.
pub fn repo_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".travsr").join("config.toml")
}

/// Load a config file into a TOML table. A missing file yields an empty table;
/// a malformed file is logged and also treated as empty so it never bricks a
/// command (forward/back-compat + robustness, H1).
fn load_table(path: &Path) -> toml::Table {
    match std::fs::read_to_string(path) {
        Ok(text) => match text.parse::<toml::Table>() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "ignoring malformed config.toml");
                eprintln!("warning: ignoring malformed {} ({e})", path.display());
                toml::Table::new()
            }
        },
        Err(_) => toml::Table::new(),
    }
}

/// Read a dotted `section.name` from a table as a display string, if present.
/// Unknown/extra keys in the file are simply not matched (they are ignored).
fn table_get(table: &toml::Table, key: &str) -> Option<String> {
    let (section, name) = key.split_once('.')?;
    let value = table.get(section)?.as_table()?.get(name)?;
    Some(value_display(value))
}

/// Insert a dotted `section.name` into a table, creating the section as needed.
fn table_set(table: &mut toml::Table, key: &str, value: toml::Value) -> Result<()> {
    let (section, name) = key
        .split_once('.')
        .with_context(|| format!("key '{key}' is not a dotted section.name"))?;
    let entry = table
        .entry(section.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !entry.is_table() {
        *entry = toml::Value::Table(toml::Table::new());
    }
    if let Some(sec) = entry.as_table_mut() {
        sec.insert(name.to_string(), value);
    }
    Ok(())
}

/// Render a stored TOML scalar as the bare string a user typed (no quotes).
fn value_display(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ── Public read API ─────────────────────────────────────────────────────────

/// The resolved status of one key across the stored layers (env > repo > global),
/// for `travsr config get` / `list`. CLI-flag overrides are not represented here
/// because reading config is not a command invocation that carries them.
#[derive(Debug, Clone)]
pub struct KeyStatus {
    pub key: &'static str,
    pub description: &'static str,
    /// The active value, or `None` when unset at every layer (uses the default).
    pub value: Option<String>,
    /// Which layer supplied `value` (or [`Source::Default`] when unset).
    pub source: Source,
    /// Human default shown when `value` is `None`.
    pub default_display: &'static str,
}

/// Resolve one registered key across the stored layers for display.
/// Errors only when `key` is not a registered key.
pub fn get(key: &str, repo_root: Option<&Path>) -> Result<KeyStatus> {
    let spec = spec(key).with_context(|| unknown_key_msg(key))?;
    Ok(status_for(spec, repo_root))
}

/// Resolve every registered key across the stored layers, for `config list`.
pub fn list(repo_root: Option<&Path>) -> Vec<KeyStatus> {
    KEYS.iter().map(|s| status_for(s, repo_root)).collect()
}

/// Read the three stored layers for `spec` from the real environment and files,
/// then hand them to the pure [`resolve_status`]. All I/O lives here; the
/// precedence logic is pure and independently tested.
fn status_for(spec: &'static KeySpec, repo_root: Option<&Path>) -> KeyStatus {
    let env_val = spec
        .env
        .and_then(|e| std::env::var(e).ok())
        .filter(|s| !s.is_empty());
    let repo_val = repo_root
        .map(|r| load_table(&repo_path(r)))
        .and_then(|t| table_get(&t, spec.key));
    let global_val = global_path()
        .map(|p| load_table(&p))
        .and_then(|t| table_get(&t, spec.key));
    resolve_status(spec, env_val, repo_val, global_val)
}

/// Pure precedence for a stored key: `env > repo > global > default`. No I/O —
/// the caller supplies each layer's value. (The CLI layer is absent because
/// reading config is not a command invocation that carries a flag.)
fn resolve_status(
    spec: &'static KeySpec,
    env_val: Option<String>,
    repo_val: Option<String>,
    global_val: Option<String>,
) -> KeyStatus {
    let (value, source) = if let Some(v) = env_val {
        (Some(v), Source::Env)
    } else if let Some(v) = repo_val {
        (Some(v), Source::RepoConfig)
    } else if let Some(v) = global_val {
        (Some(v), Source::GlobalConfig)
    } else {
        (None, Source::Default)
    };
    KeyStatus {
        key: spec.key,
        description: spec.description,
        value,
        source,
        default_display: spec.default_display,
    }
}

/// Read a single dotted key from a specific config file (for callers that want
/// one layer explicitly, and for hermetic tests). `None` if absent or unreadable.
pub fn read_key_file(path: &Path, key: &str) -> Option<String> {
    table_get(&load_table(path), key)
}

// ── Public write API ────────────────────────────────────────────────────────

/// Where `set` writes: the machine-global config or a specific repo's config.
pub enum Scope {
    Global,
    Repo(PathBuf),
}

/// Validate `value` against the key's registered validator and persist it to the
/// chosen scope's `config.toml`. Rejects unknown keys and invalid values with a
/// clear error (never panics). Preserves other keys already in the file.
pub fn set(key: &str, value: &str, scope: Scope) -> Result<()> {
    let spec = spec(key).with_context(|| unknown_key_msg(key))?;
    let normalised =
        (spec.validate)(value).with_context(|| format!("invalid value for '{key}'"))?;

    let path = match scope {
        Scope::Global => global_path().context("cannot locate home directory for global config")?,
        Scope::Repo(root) => repo_path(&root),
    };

    let mut table = load_table(&path);
    table_set(&mut table, key, normalised)?;
    write_table_atomic(&path, &table)
}

/// Serialize a table and write it atomically (temp file + rename) so a crashed
/// or concurrent write never leaves a half-written config.
fn write_table_atomic(path: &Path, table: &toml::Table) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(table).context("serialising config")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

fn unknown_key_msg(key: &str) -> String {
    let known: Vec<&str> = KEYS.iter().map(|k| k.key).collect();
    format!(
        "unknown config key '{key}'. Known keys: {}",
        known.join(", ")
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_precedence_cli_wins() {
        let r = resolve(Some(1), Some(2), Some(3), Some(4), 0);
        assert_eq!(r.value, 1);
        assert_eq!(r.source, Source::Cli);
    }

    #[test]
    fn resolve_precedence_falls_through_layers() {
        assert_eq!(
            resolve(None, Some(2), Some(3), Some(4), 0).source,
            Source::Env
        );
        assert_eq!(
            resolve::<i32>(None, None, Some(3), Some(4), 0).source,
            Source::RepoConfig
        );
        assert_eq!(
            resolve::<i32>(None, None, None, Some(4), 0).source,
            Source::GlobalConfig
        );
        assert_eq!(
            resolve::<i32>(None, None, None, None, 0).source,
            Source::Default
        );
        assert_eq!(resolve::<i32>(None, None, None, None, 0).value, 0);
    }

    #[test]
    fn validate_capacity_bounds_and_auto() {
        assert!(validate_capacity("50").is_ok());
        assert!(validate_capacity("1").is_ok());
        assert!(validate_capacity("100").is_ok());
        assert!(validate_capacity("auto").is_ok());
        assert!(validate_capacity("AUTO").is_ok());
        assert_eq!(
            validate_capacity("auto").unwrap(),
            toml::Value::String("auto".into())
        );
        assert!(validate_capacity("0").is_err());
        assert!(validate_capacity("101").is_err());
        assert!(validate_capacity("-5").is_err());
        assert!(validate_capacity("abc").is_err());
    }

    #[test]
    fn validate_priority_enum() {
        for ok in ["normal", "low", "idle"] {
            assert!(validate_priority(ok).is_ok());
        }
        assert!(validate_priority("high").is_err());
        assert!(validate_priority("").is_err());
    }

    #[test]
    fn set_get_round_trip_repo_scope() {
        // Assert against the written file directly (via read_key_file) so the test
        // is hermetic — independent of the developer's real ~/.travsr and env.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".travsr")).expect("mk .travsr");
        let cfg = repo_path(root);

        set("embed.capacity", "40", Scope::Repo(root.to_path_buf())).expect("set");
        assert_eq!(read_key_file(&cfg, "embed.capacity").as_deref(), Some("40"));

        // A second key in the same file must not clobber the first.
        set("embed.priority", "low", Scope::Repo(root.to_path_buf())).expect("set2");
        assert_eq!(read_key_file(&cfg, "embed.capacity").as_deref(), Some("40"));
        assert_eq!(
            read_key_file(&cfg, "embed.priority").as_deref(),
            Some("low")
        );
    }

    #[test]
    fn resolve_status_precedence() {
        let s = spec("embed.capacity").expect("spec");
        // env wins over repo/global
        let r = resolve_status(s, Some("10".into()), Some("20".into()), Some("30".into()));
        assert_eq!(r.value.as_deref(), Some("10"));
        assert_eq!(r.source, Source::Env);
        // repo over global
        let r = resolve_status(s, None, Some("20".into()), Some("30".into()));
        assert_eq!(r.source, Source::RepoConfig);
        // global when only global set
        let r = resolve_status(s, None, None, Some("30".into()));
        assert_eq!(r.source, Source::GlobalConfig);
        // unset → default
        let r = resolve_status(s, None, None, None);
        assert!(r.value.is_none());
        assert_eq!(r.source, Source::Default);
        assert_eq!(r.default_display, "100");
    }

    #[test]
    fn set_rejects_unknown_key_and_bad_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        assert!(set("embed.bogus", "1", Scope::Repo(root.clone())).is_err());
        assert!(set("embed.capacity", "999", Scope::Repo(root)).is_err());
    }

    #[test]
    fn malformed_file_is_ignored_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is = not valid = toml =").expect("write");
        // Must not panic; yields an empty table → key reads as unset.
        let table = load_table(&path);
        assert!(table_get(&table, "embed.capacity").is_none());
    }

    #[test]
    fn table_set_creates_and_preserves_sections() {
        let mut t = toml::Table::new();
        table_set(&mut t, "embed.capacity", toml::Value::Integer(40)).expect("set1");
        table_set(&mut t, "embed.priority", toml::Value::String("low".into())).expect("set2");
        assert_eq!(table_get(&t, "embed.capacity").as_deref(), Some("40"));
        assert_eq!(table_get(&t, "embed.priority").as_deref(), Some("low"));
    }
}
