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
    // #376 O1: the docs lane's user-facing knobs. Every key here is read by the
    // process that actually performs retrieval (the daemon for `ask`, the MCP
    // server for `get_context`), which is why they are config keys and not only
    // env vars: `TRAVSR_DOCS_ENABLED` set on the CLI is a silent no-op because
    // the CLI is not the process that retrieves (plan §18.7, §20.3 F-D).
    //
    // Deliberately NOT registered: `TRAVSR_DOC_FLOOR` and
    // `TRAVSR_DOC_RERANK_FLOOR`. Both are raw model-space score thresholds whose
    // calibration is measured, not chosen (plan §8.3, §14) — surfacing them as
    // user config would invite exactly the cross-section score comparison §4.1
    // exists to prevent. They remain expert env-only overrides.
    KeySpec {
        key: "docs.enabled",
        description:
            "Retrieve documentation prose as a separate `docs` result section (true | false).",
        env: Some("TRAVSR_DOCS_ENABLED"),
        // #519: both bench repos cleared #376 §7's bar, so this is on by
        // default (travsr-mcp::seed::docs_enabled's own unwrap_or(true)).
        // Keep this string in sync with that fallback — it is display-only,
        // read by `config get`/`list`, and does not itself drive behavior.
        default_display: "true",
        validate: validate_bool,
    },
    KeySpec {
        key: "docs.max_results",
        description: "Maximum entries rendered in the `docs` section (>= 1).",
        env: Some("TRAVSR_DOCS_MAX_RESULTS"),
        default_display: "3",
        validate: validate_positive_int,
    },
    KeySpec {
        key: "docs.budget_pct",
        description: "Share of the token budget the `docs` section may claim, 1-100.",
        env: Some("TRAVSR_DOCS_BUDGET_PCT"),
        default_display: "20",
        validate: validate_percent,
    },
    KeySpec {
        key: "docs.exclude",
        description:
            "Extra comma-separated path substrings excluded from doc indexing, beyond the built-ins.",
        env: Some("TRAVSR_DOCS_EXCLUDE"),
        default_display: "(none)",
        validate: validate_string_list,
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

/// Accepts the spellings a shell env var realistically carries as well as TOML's
/// own `true`/`false`, and normalises to a real TOML boolean so a file written by
/// `set` reads naturally. `1`/`0` are accepted because `TRAVSR_DOCS_ENABLED=1` is
/// the form every existing script and the bench harness already use.
fn validate_bool(s: &str) -> Result<toml::Value> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(toml::Value::Boolean(true)),
        "0" | "false" | "no" | "off" => Ok(toml::Value::Boolean(false)),
        other => bail!("expected true or false (got '{other}')"),
    }
}

fn validate_percent(s: &str) -> Result<toml::Value> {
    let n: i64 = s
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("expected an integer percent 1-100, got '{s}'"))?;
    if !(1..=100).contains(&n) {
        bail!("value must be between 1 and 100 (percent), got {n}");
    }
    Ok(toml::Value::Integer(n))
}

/// The list-typed key shape (#376 O1). Accepts the comma-separated form the
/// matching env var uses, stores a real TOML array so a hand-edited
/// `config.toml` can use native list syntax, and round-trips back through
/// [`value_display`] as the same comma-separated string the consumer parses.
/// Empty entries are dropped rather than rejected, so a trailing comma is not an
/// error.
fn validate_string_list(s: &str) -> Result<toml::Value> {
    let items: Vec<toml::Value> = s
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| toml::Value::String(p.to_string()))
        .collect();
    Ok(toml::Value::Array(items))
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

/// Render a stored TOML value as the bare string a user typed (no quotes).
///
/// Arrays render comma-separated rather than in TOML literal syntax, so a
/// list-typed key round-trips: what `set` stored as `["a", "b"]` reads back as
/// `a,b`, which is byte-identical to what the matching env var would carry and
/// therefore parseable by one consumer-side splitter regardless of which layer
/// supplied it.
fn value_display(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Array(items) => items
            .iter()
            .map(value_display)
            .collect::<Vec<_>>()
            .join(","),
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
///
/// Loads each config file exactly once and shares the parsed tables across all
/// keys, so a malformed file warning prints once instead of once per key.
pub fn list(repo_root: Option<&Path>) -> Vec<KeyStatus> {
    let repo_table = repo_root.map(|r| load_table(&repo_path(r)));
    let global_table = global_path().map(|p| load_table(&p));
    KEYS.iter()
        .map(|s| status_for_tables(s, repo_table.as_ref(), global_table.as_ref()))
        .collect()
}

/// Read the three stored layers for `spec` from the real environment and files,
/// then hand them to the pure [`resolve_status`]. All I/O lives here; the
/// precedence logic is pure and independently tested. Used by `get` (single key).
fn status_for(spec: &'static KeySpec, repo_root: Option<&Path>) -> KeyStatus {
    let repo_table = repo_root.map(|r| load_table(&repo_path(r)));
    let global_table = global_path().map(|p| load_table(&p));
    status_for_tables(spec, repo_table.as_ref(), global_table.as_ref())
}

/// Like `status_for` but accepts already-loaded tables so `list` can share one
/// file-load across all keys — a malformed file warns exactly once, not N times.
fn status_for_tables(
    spec: &'static KeySpec,
    repo_table: Option<&toml::Table>,
    global_table: Option<&toml::Table>,
) -> KeyStatus {
    let env_val = spec
        .env
        .and_then(|e| std::env::var(e).ok())
        .filter(|s| !s.is_empty());
    let repo_val = repo_table.and_then(|t| table_get(t, spec.key));
    let global_val = global_table.and_then(|t| table_get(t, spec.key));
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

/// The active value of a registered key across `env > repo > global`, or `None`
/// when unset at every layer (the caller then applies its own built-in default).
///
/// This is the reader every *runtime* consumer should use, as opposed to [`get`]
/// which additionally carries the provenance and description `travsr config`
/// needs for display. An unknown key yields `None` rather than an error: a
/// runtime lookup for a key that was removed from the registry must degrade to
/// the built-in default, never fail a query.
pub fn effective(key: &str, repo_root: Option<&Path>) -> Option<String> {
    get(key, repo_root).ok().and_then(|s| s.value)
}

/// [`effective`], parsed as a boolean with the same spellings [`validate_bool`]
/// accepts. An unparseable value falls back to `None` (the caller's default)
/// rather than guessing, so a typo in `config.toml` cannot silently flip a
/// feature on.
pub fn effective_bool(key: &str, repo_root: Option<&Path>) -> Option<bool> {
    match effective(key, repo_root)?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// [`effective`], split on commas into the list form list-typed keys round-trip
/// through (see [`value_display`]). Absent and empty both yield an empty `Vec`,
/// which is the "no extra patterns" case.
pub fn effective_list(key: &str, repo_root: Option<&Path>) -> Vec<String> {
    effective(key, repo_root)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
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

/// Remove a key's override from the chosen scope's `config.toml`, so it falls
/// back to the next-lower layer (repo → global → env → default). Returns `true`
/// if the key was present and removed, `false` if it was not set in that scope.
/// An emptied section is pruned so no `[section]` husk is left behind. Rejects
/// unknown keys with the same message as [`set`]; never panics.
pub fn unset(key: &str, scope: Scope) -> Result<bool> {
    // Validate the key exists in the registry, matching `set`/`get` behavior.
    spec(key).with_context(|| unknown_key_msg(key))?;

    let path = match scope {
        Scope::Global => global_path().context("cannot locate home directory for global config")?,
        Scope::Repo(root) => repo_path(&root),
    };
    if !path.exists() {
        return Ok(false);
    }

    let mut table = load_table(&path);
    let removed = table_unset(&mut table, key);
    if removed {
        write_table_atomic(&path, &table)?;
    }
    Ok(removed)
}

/// Remove a dotted `section.name` from a table, pruning the section if it becomes
/// empty. Returns whether anything was removed.
fn table_unset(table: &mut toml::Table, key: &str) -> bool {
    let Some((section, name)) = key.split_once('.') else {
        return false;
    };
    let Some(entry) = table.get_mut(section).and_then(|e| e.as_table_mut()) else {
        return false;
    };
    let removed = entry.remove(name).is_some();
    if entry.is_empty() {
        table.remove(section);
    }
    removed
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
    fn validate_bool_accepts_env_and_toml_spellings() {
        for t in ["1", "true", "TRUE", "True", "yes", "on"] {
            assert_eq!(validate_bool(t).unwrap(), toml::Value::Boolean(true), "{t}");
        }
        for f in ["0", "false", "FALSE", "no", "off"] {
            assert_eq!(
                validate_bool(f).unwrap(),
                toml::Value::Boolean(false),
                "{f}"
            );
        }
        assert!(validate_bool("maybe").is_err());
        assert!(validate_bool("").is_err());
    }

    #[test]
    fn validate_percent_bounds() {
        assert!(validate_percent("1").is_ok());
        assert!(validate_percent("100").is_ok());
        assert!(validate_percent("0").is_err());
        assert!(validate_percent("101").is_err());
        assert!(validate_percent("abc").is_err());
    }

    /// The list-typed key must survive `set` -> file -> read as the same
    /// comma-separated string the matching env var carries, so one consumer-side
    /// splitter handles every layer. A trailing comma is tolerated, not an error.
    #[test]
    fn list_typed_key_round_trips_as_comma_separated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".travsr")).expect("mk .travsr");
        let cfg = repo_path(root);

        set(
            "docs.exclude",
            " vendor/ , third_party/ ,",
            Scope::Repo(root.to_path_buf()),
        )
        .expect("set");
        assert_eq!(
            read_key_file(&cfg, "docs.exclude").as_deref(),
            Some("vendor/,third_party/")
        );

        // Stored as a real TOML array, so a hand-edited file may use list syntax.
        let text = std::fs::read_to_string(&cfg).expect("read");
        assert!(
            text.contains('['),
            "expected TOML array syntax, got: {text}"
        );
    }

    /// Every docs key the runtime honours must be registered, or
    /// `travsr config set docs.enabled true` succeeds and changes nothing —
    /// the exact silent-failure class #376 G1 exists to close.
    #[test]
    fn docs_keys_are_registered_with_their_env_vars() {
        for (key, env) in [
            ("docs.enabled", "TRAVSR_DOCS_ENABLED"),
            ("docs.max_results", "TRAVSR_DOCS_MAX_RESULTS"),
            ("docs.budget_pct", "TRAVSR_DOCS_BUDGET_PCT"),
            ("docs.exclude", "TRAVSR_DOCS_EXCLUDE"),
        ] {
            let s = spec(key).unwrap_or_else(|| panic!("{key} not registered"));
            assert_eq!(s.env, Some(env), "{key}");
        }
    }

    #[test]
    fn effective_bool_rejects_garbage_instead_of_guessing() {
        // A typo must fall through to the caller's default, never flip a feature on.
        assert_eq!(parse_bool_str("true"), Some(true));
        assert_eq!(parse_bool_str("0"), Some(false));
        assert_eq!(parse_bool_str("ture"), None);
    }

    /// Mirror of `effective_bool`'s parse, testable without touching the real
    /// environment or `~/.travsr`.
    fn parse_bool_str(s: &str) -> Option<bool> {
        match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        }
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

    #[test]
    fn table_unset_removes_key_and_prunes_empty_section() {
        let mut t = toml::Table::new();
        table_set(&mut t, "embed.capacity", toml::Value::Integer(40)).expect("set1");
        table_set(&mut t, "embed.priority", toml::Value::String("low".into())).expect("set2");

        // Removing one key leaves the sibling and the section intact.
        assert!(table_unset(&mut t, "embed.capacity"));
        assert!(table_get(&t, "embed.capacity").is_none());
        assert_eq!(table_get(&t, "embed.priority").as_deref(), Some("low"));
        assert!(t.contains_key("embed"));

        // Removing the last key prunes the now-empty section husk.
        assert!(table_unset(&mut t, "embed.priority"));
        assert!(!t.contains_key("embed"));

        // Removing an absent key is a no-op that reports false.
        assert!(!table_unset(&mut t, "embed.capacity"));
    }
}
