//! `travsr connect` — wire detected AI coding tools to the Travsr MCP server and
//! drop an always-on "use Travsr first" rules file for each. Also invoked by
//! `travsr init` (RFC-026).
//!
//! Two co-equal outputs per detected tool:
//!   1. register `travsr mcp --stdio` in the tool's MCP config, and
//!   2. write an always-on rules/instructions file directing the agent to query
//!      Travsr before grep/find. Wiring alone does not change agent behavior — the
//!      rules are what make the agent actually use Travsr.
//!
//! Safety (RFC-026): generated files are local and git-ignored by default (never
//! committed, since a committed MCP server definition is an RCE-on-clone vector); the
//! server command is the bare `travsr` when it is on PATH (no absolute-path /
//! username leak); existing non-strict-JSON configs are skipped, never clobbered;
//! markdown rules use a single balanced managed block. All failures are non-fatal.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

const MD_BEGIN: &str = "<!-- travsr:begin -->";
const MD_END: &str = "<!-- travsr:end -->";
const GI_BEGIN: &str =
    "# travsr:begin (generated AI-tool config — `travsr connect --remove` to undo)";
const GI_END: &str = "# travsr:end";

/// Options controlling a connect run. `auto()` is the zero-config path used by
/// `travsr init`.
pub struct ConnectOpts {
    /// Restrict to a single tool id (e.g. "cursor"). `None` = all detected.
    pub only: Option<String>,
    /// Show what would be written / printed without touching the filesystem.
    pub dry_run: bool,
    /// Remove previously generated Travsr config instead of writing it.
    pub remove: bool,
    /// Do NOT git-ignore the generated files (opt in to committing them).
    pub commit: bool,
    /// Do the wiring without printing a report. Used by `travsr init --quiet`
    /// and `--json`, where the report would be noise or would corrupt stdout.
    pub quiet: bool,
}

impl ConnectOpts {
    pub fn auto() -> Self {
        Self {
            only: None,
            dry_run: false,
            remove: false,
            commit: false,
            quiet: false,
        }
    }
}

/// The resolved MCP server command written into tool configs.
struct McpCommand {
    command: String,
    args: Vec<String>,
}

impl McpCommand {
    /// Prefer the bare `travsr` command when `~/.travsr/bin` is on PATH (portable,
    /// no username leak); fall back to the absolute current exe otherwise.
    fn resolve() -> Self {
        let command = if crate::install::path_contains_travsr_bin() {
            "travsr".to_string()
        } else {
            std::env::current_exe()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "travsr".to_string())
        };
        Self {
            command,
            args: vec!["mcp".to_string(), "--stdio".to_string()],
        }
    }

    fn on_path(&self) -> bool {
        self.command == "travsr"
    }
}

// ---------------------------------------------------------------------------
// Canonical agent guidance (shared text, wrapped per tool).
// ---------------------------------------------------------------------------

const GUIDE_TITLE: &str = "Use Travsr first for all code questions";

fn guide_body() -> String {
    "This repository has a Travsr code graph served over MCP. For ANY question \
about code structure (definitions, callers, dependencies, impact/blast radius, \
call paths, or repo overview) ALWAYS query Travsr's MCP tools BEFORE \
grep/find/ripgrep or reading whole files. Travsr is the token-efficient, \
hallucination-free path.

- search_symbol(name)         find a definition
- get_callers(symbol)         who calls this
- get_dependencies(file)      what this depends on
- get_blast_radius(file)      what a change here affects
- get_execution_path(a, b)    how a reaches b
- get_repo_map(repo)          high-level structure
- get_context(query, budget)  full PPR + knapsack retrieval

Only fall back to text search when Travsr returns nothing or is unavailable."
        .to_string()
}

/// Markdown rules body (heading + directive) for managed blocks and owned files.
fn markdown_rules() -> String {
    format!("# {GUIDE_TITLE}\n\n{}\n", guide_body())
}

/// Cursor `.mdc` file: YAML frontmatter with `alwaysApply: true` is what makes the
/// rule load on every turn; a plain markdown block would never activate.
fn cursor_mdc() -> String {
    format!(
        "---\ndescription: Use Travsr's code graph (MCP) before grep/find\nalwaysApply: true\n---\n\n{}",
        markdown_rules()
    )
}

// ---------------------------------------------------------------------------
// Tools.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    ClaudeCode,
    Cursor,
    VsCodeCopilot,
    Windsurf,
    Zed,
}

/// How a tool was detected, which decides whether we auto-write project files or
/// just print a snippet (never auto-write into a repo for a tool only known from a
/// global/home marker).
enum Detection {
    /// Project-local marker present — safe to write project-scoped config.
    Auto,
    /// Only a global marker (or no project MCP file) — print a snippet instead.
    Print,
    /// Not present.
    None,
}

/// A planned write. `JsonServer` upserts one server under `top_key`; `ManagedMd`
/// maintains a balanced block in a shared file; `Owned` fully owns a travsr file.
enum Content {
    JsonServer { top_key: &'static str, entry: Value },
    ManagedMd { body: String },
    Owned { text: String },
}

struct Planned {
    path: PathBuf,
    content: Content,
    /// Whether to git-ignore this file by default. True only for dedicated MCP
    /// config files (the committed-server RCE-on-clone vector). Shared files the
    /// user owns and commits — `CLAUDE.md`, `copilot-instructions.md`, Zed's
    /// general `settings.json`, rules files — are never git-ignored.
    gitignore: bool,
}

enum Outcome {
    Written,
    Unchanged,
    Removed,
    Skipped(String),
    Absent,
}

impl Tool {
    const ALL: [Tool; 5] = [
        Tool::ClaudeCode,
        Tool::Cursor,
        Tool::VsCodeCopilot,
        Tool::Windsurf,
        Tool::Zed,
    ];

    fn id(&self) -> &'static str {
        match self {
            Tool::ClaudeCode => "claude-code",
            Tool::Cursor => "cursor",
            Tool::VsCodeCopilot => "vscode-copilot",
            Tool::Windsurf => "windsurf",
            Tool::Zed => "zed",
        }
    }

    fn detect(&self, repo: &Path, home: Option<&Path>) -> Detection {
        let has = |p: PathBuf| p.exists();
        match self {
            Tool::ClaudeCode => {
                if has(repo.join(".claude")) || has(repo.join("CLAUDE.md")) {
                    Detection::Auto
                } else if home.is_some_and(|h| has(h.join(".claude"))) {
                    Detection::Print
                } else {
                    Detection::None
                }
            }
            Tool::Cursor => {
                if has(repo.join(".cursor")) {
                    Detection::Auto
                } else if home.is_some_and(|h| has(h.join(".cursor"))) {
                    Detection::Print
                } else {
                    Detection::None
                }
            }
            // Bare `.vscode/` is in most repos without Copilot — only auto-write
            // when an mcp.json already exists; otherwise print.
            Tool::VsCodeCopilot => {
                if has(repo.join(".vscode/mcp.json")) {
                    Detection::Auto
                } else if has(repo.join(".vscode")) {
                    Detection::Print
                } else {
                    Detection::None
                }
            }
            // Windsurf MCP config is global-only; never auto-write the home dir.
            Tool::Windsurf => {
                if home.is_some_and(|h| has(h.join(".codeium/windsurf"))) {
                    Detection::Print
                } else {
                    Detection::None
                }
            }
            Tool::Zed => {
                if has(repo.join(".zed")) {
                    Detection::Auto
                } else if home.is_some_and(|h| {
                    has(h.join(".config/zed")) || has(h.join("Library/Application Support/Zed"))
                }) {
                    Detection::Print
                } else {
                    Detection::None
                }
            }
        }
    }

    /// Project files to write when auto-detected.
    fn plan(&self, repo: &Path, cmd: &McpCommand) -> Vec<Planned> {
        let flat = json!({ "command": cmd.command, "args": cmd.args });
        match self {
            Tool::ClaudeCode => vec![
                Planned {
                    path: repo.join(".mcp.json"),
                    content: Content::JsonServer {
                        top_key: "mcpServers",
                        entry: flat,
                    },
                    gitignore: true,
                },
                Planned {
                    path: repo.join("CLAUDE.md"),
                    content: Content::ManagedMd {
                        body: markdown_rules(),
                    },
                    gitignore: false,
                },
            ],
            Tool::Cursor => vec![
                Planned {
                    path: repo.join(".cursor/mcp.json"),
                    content: Content::JsonServer {
                        top_key: "mcpServers",
                        entry: flat,
                    },
                    gitignore: true,
                },
                Planned {
                    path: repo.join(".cursor/rules/travsr.mdc"),
                    content: Content::Owned { text: cursor_mdc() },
                    gitignore: false,
                },
            ],
            Tool::VsCodeCopilot => vec![
                Planned {
                    path: repo.join(".vscode/mcp.json"),
                    content: Content::JsonServer {
                        top_key: "servers",
                        // VS Code requires an explicit transport type.
                        entry: json!({ "type": "stdio", "command": cmd.command, "args": cmd.args }),
                    },
                    gitignore: true,
                },
                Planned {
                    path: repo.join(".github/copilot-instructions.md"),
                    content: Content::ManagedMd {
                        body: markdown_rules(),
                    },
                    gitignore: false,
                },
            ],
            Tool::Zed => vec![
                Planned {
                    // Shared general settings file — not git-ignored.
                    path: repo.join(".zed/settings.json"),
                    content: Content::JsonServer {
                        top_key: "context_servers",
                        entry: flat,
                    },
                    gitignore: false,
                },
                Planned {
                    path: repo.join(".rules"),
                    content: Content::ManagedMd {
                        body: markdown_rules(),
                    },
                    gitignore: false,
                },
            ],
            // Global-only; handled via `snippet()` not `plan()`.
            Tool::Windsurf => vec![],
        }
    }

    /// Snippet printed when only a global marker is present.
    fn snippet(&self, repo: &Path, cmd: &McpCommand) -> String {
        let server = serde_json::to_string_pretty(
            &json!({ "mcpServers": { "travsr": { "command": cmd.command, "args": cmd.args } } }),
        )
        .unwrap_or_default();
        match self {
            Tool::Windsurf => format!(
                "  add to ~/.codeium/windsurf/mcp_config.json:\n{server}\n  \
                 and create {}/.windsurf/rules/travsr.md with the Travsr guidance",
                repo.display()
            ),
            _ => format!("  MCP server config:\n{server}"),
        }
    }
}

// ---------------------------------------------------------------------------
// File operations.
// ---------------------------------------------------------------------------

/// Atomically replace `path` with `content`: write a sibling temp file, then
/// rename over the target. Because these helpers do read-modify-write of the
/// user's own files (CLAUDE.md, an existing mcp.json), a crash mid-write must
/// never truncate the original — same temp+rename pattern as `install.rs`.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("travsr-config");
    let tmp = match path.parent() {
        Some(dir) => dir.join(format!(".{file_name}.tmp.{}", std::process::id())),
        None => path.with_extension("tmp"),
    };
    std::fs::write(&tmp, content).with_context(|| format!("writing temp {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("renaming into {}", path.display()));
    }
    Ok(())
}

/// Upsert one server under `root[top_key]["travsr"]`. Skips (never clobbers) a
/// file that does not parse as strict JSON or whose shape is unexpected.
fn merge_json_server(path: &Path, top_key: &str, entry: &Value) -> Result<Outcome> {
    let mut root: Value = if path.exists() {
        let text = std::fs::read_to_string(path)?;
        if text.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => {
                    return Ok(Outcome::Skipped(
                        "existing file is not strict JSON (left untouched)".into(),
                    ))
                }
            }
        }
    } else {
        json!({})
    };

    let Some(obj) = root.as_object_mut() else {
        return Ok(Outcome::Skipped("top level is not a JSON object".into()));
    };
    let servers = obj.entry(top_key.to_string()).or_insert_with(|| json!({}));
    let Some(map) = servers.as_object_mut() else {
        return Ok(Outcome::Skipped(format!(
            "`{top_key}` is not a JSON object"
        )));
    };

    if map.get("travsr") == Some(entry) {
        return Ok(Outcome::Unchanged);
    }
    map.insert("travsr".to_string(), entry.clone());

    let pretty = serde_json::to_string_pretty(&root)? + "\n";
    write_atomic(path, &pretty)?;
    Ok(Outcome::Written)
}

fn remove_json_server(path: &Path, top_key: &str) -> Result<Outcome> {
    if !path.exists() {
        return Ok(Outcome::Absent);
    }
    let text = std::fs::read_to_string(path)?;
    let mut root: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Ok(Outcome::Skipped("not strict JSON (left untouched)".into())),
    };
    let removed = root
        .as_object_mut()
        .and_then(|o| o.get_mut(top_key))
        .and_then(|s| s.as_object_mut())
        .map(|m| m.remove("travsr").is_some())
        .unwrap_or(false);
    if !removed {
        return Ok(Outcome::Absent);
    }
    let pretty = serde_json::to_string_pretty(&root)? + "\n";
    write_atomic(path, &pretty)?;
    Ok(Outcome::Removed)
}

/// Replace or append a single balanced `begin/end` block. Skips on malformed,
/// duplicate, or nested markers rather than risk a destructive edit.
fn upsert_block(path: &Path, begin: &str, end: &str, body: &str) -> Result<Outcome> {
    let block = format!("{begin}\n{body}\n{end}\n");
    if !path.exists() {
        write_atomic(path, &block)?;
        return Ok(Outcome::Written);
    }
    let text = std::fs::read_to_string(path)?;
    let nb = text.matches(begin).count();
    let ne = text.matches(end).count();
    if nb > 1 || ne > 1 || nb != ne {
        return Ok(Outcome::Skipped(
            "malformed/duplicate travsr markers (left untouched)".into(),
        ));
    }
    if nb == 0 {
        let sep = if text.is_empty() || text.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        let new = format!("{text}{sep}{block}");
        write_atomic(path, &new)?;
        return Ok(Outcome::Written);
    }
    // Exactly one balanced pair — replace the region.
    let (Some(start), Some(estart)) = (text.find(begin), text.find(end)) else {
        return Ok(Outcome::Skipped("markers in unexpected order".into()));
    };
    let stop = estart + end.len();
    if estart < start {
        return Ok(Outcome::Skipped("markers in unexpected order".into()));
    }
    let new = format!("{}{}{}", &text[..start], block.trim_end(), &text[stop..]);
    if new == text {
        return Ok(Outcome::Unchanged);
    }
    write_atomic(path, &new)?;
    Ok(Outcome::Written)
}

fn remove_block(path: &Path, begin: &str, end: &str) -> Result<Outcome> {
    if !path.exists() {
        return Ok(Outcome::Absent);
    }
    let text = std::fs::read_to_string(path)?;
    let nb = text.matches(begin).count();
    let ne = text.matches(end).count();
    if nb == 0 && ne == 0 {
        return Ok(Outcome::Absent);
    }
    if nb != 1 || ne != 1 {
        return Ok(Outcome::Skipped(
            "malformed travsr markers (left untouched)".into(),
        ));
    }
    let (Some(start), Some(estart)) = (text.find(begin), text.find(end)) else {
        return Ok(Outcome::Absent);
    };
    let mut stop = estart + end.len();
    if text[stop..].starts_with('\n') {
        stop += 1;
    }
    let mut head = text[..start].to_string();
    while head.ends_with('\n') {
        head.pop();
    }
    let tail = &text[stop..];
    let new = if head.is_empty() && tail.trim().is_empty() {
        String::new()
    } else if tail.is_empty() {
        format!("{head}\n")
    } else {
        format!("{head}\n{tail}")
    };
    if new.trim().is_empty() {
        // Whole file was our block — remove it.
        std::fs::remove_file(path).ok();
        return Ok(Outcome::Removed);
    }
    write_atomic(path, &new)?;
    Ok(Outcome::Removed)
}

fn execute(p: &Planned, remove: bool) -> Result<Outcome> {
    match &p.content {
        Content::JsonServer { top_key, entry } => {
            if remove {
                remove_json_server(&p.path, top_key)
            } else {
                merge_json_server(&p.path, top_key, entry)
            }
        }
        Content::ManagedMd { body } => {
            if remove {
                remove_block(&p.path, MD_BEGIN, MD_END)
            } else {
                upsert_block(&p.path, MD_BEGIN, MD_END, body)
            }
        }
        Content::Owned { text } => {
            if remove {
                if p.path.exists() {
                    std::fs::remove_file(&p.path)
                        .with_context(|| format!("removing {}", p.path.display()))?;
                    Ok(Outcome::Removed)
                } else {
                    Ok(Outcome::Absent)
                }
            } else if p.path.exists()
                && std::fs::read_to_string(&p.path).ok().as_deref() == Some(text)
            {
                Ok(Outcome::Unchanged)
            } else {
                write_atomic(&p.path, text)?;
                Ok(Outcome::Written)
            }
        }
    }
}

/// Maintain a `.gitignore` block listing the generated repo-relative paths.
fn ensure_gitignored(repo: &Path, rels: &[String], remove: bool) -> Result<Outcome> {
    let path = repo.join(".gitignore");
    if remove {
        return remove_block(&path, GI_BEGIN, GI_END);
    }
    if rels.is_empty() {
        return Ok(Outcome::Absent);
    }
    let body = rels.join("\n");
    upsert_block(&path, GI_BEGIN, GI_END, &body)
}

fn rel(repo: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(repo)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Detect AI tools under `repo_root` and wire each to Travsr. Never returns an
/// error to the caller for routine skips; the bool indicates whether anything was
/// detected. Used by both `travsr init` and `travsr connect`.
pub fn run(repo_root: &Path, opts: &ConnectOpts) -> Result<()> {
    let home = dirs::home_dir();
    let cmd = McpCommand::resolve();
    let verb = if opts.remove { "removed" } else { "configured" };

    // `--quiet` suppresses the report, never the wiring.
    macro_rules! say {
        ($($arg:tt)*) => {
            if !opts.quiet {
                println!($($arg)*);
            }
        };
    }

    let mut detected = false;
    let mut gitignore: Vec<String> = Vec::new();

    for tool in Tool::ALL {
        if let Some(only) = &opts.only {
            if tool.id() != only {
                continue;
            }
        }
        match tool.detect(repo_root, home.as_deref()) {
            Detection::Auto => {
                detected = true;
                say!("{} ({verb}):", tool.id());
                for planned in tool.plan(repo_root, &cmd) {
                    let disp = rel(repo_root, &planned.path)
                        .unwrap_or_else(|| planned.path.display().to_string());
                    if opts.dry_run {
                        say!("  would write {disp}");
                        if planned.gitignore {
                            if let Some(r) = rel(repo_root, &planned.path) {
                                gitignore.push(r);
                            }
                        }
                        continue;
                    }
                    match execute(&planned, opts.remove) {
                        Ok(outcome) => {
                            match &outcome {
                                Outcome::Skipped(reason) => {
                                    say!("  skipped {disp}: {reason}")
                                }
                                other => say!("  {} {disp}", label(other)),
                            }
                            if planned.gitignore
                                && matches!(outcome, Outcome::Written | Outcome::Unchanged)
                            {
                                if let Some(r) = rel(repo_root, &planned.path) {
                                    gitignore.push(r);
                                }
                            }
                        }
                        // Per-file failure is non-fatal; report and continue.
                        Err(e) => say!("  error {disp}: {e}"),
                    }
                }
            }
            Detection::Print => {
                detected = true;
                say!("{} detected (global) — add manually:", tool.id());
                say!("{}", tool.snippet(repo_root, &cmd));
            }
            Detection::None => {}
        }
    }

    if !detected {
        say!(
            "tip: no AI coding tool detected — run `travsr connect` after installing \
             Claude Code, Cursor, Copilot, Windsurf, or Zed"
        );
        return Ok(());
    }

    if !opts.dry_run && !opts.commit {
        match ensure_gitignored(repo_root, &gitignore, opts.remove) {
            Ok(Outcome::Written) => say!(
                "  {} .gitignore (generated files are local-only)",
                label(&Outcome::Written)
            ),
            Ok(Outcome::Removed) => say!("  {} .gitignore", label(&Outcome::Removed)),
            _ => {}
        }
    }

    if !opts.remove && !cmd.on_path() {
        say!(
            "note: `travsr` is not on PATH; configs use an absolute path. Add \
             ~/.travsr/bin to PATH so the wiring survives moves."
        );
    }

    Ok(())
}

fn label(o: &Outcome) -> &'static str {
    match o {
        Outcome::Written => "wrote",
        Outcome::Unchanged => "ok",
        Outcome::Removed => "removed",
        Outcome::Skipped(_) => "skipped",
        Outcome::Absent => "absent",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cmd() -> McpCommand {
        McpCommand {
            command: "travsr".into(),
            args: vec!["mcp".into(), "--stdio".into()],
        }
    }

    #[test]
    fn merge_preserves_other_servers_and_adds_travsr() {
        let dir = tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        std::fs::write(&p, r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        let entry = json!({ "command": "travsr", "args": ["mcp","--stdio"] });
        merge_json_server(&p, "mcpServers", &entry).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["travsr"]["command"], "travsr");
    }

    #[test]
    fn merge_is_idempotent() {
        let dir = tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        let entry = json!({ "command": "travsr", "args": ["mcp","--stdio"] });
        assert!(matches!(
            merge_json_server(&p, "mcpServers", &entry).unwrap(),
            Outcome::Written
        ));
        assert!(matches!(
            merge_json_server(&p, "mcpServers", &entry).unwrap(),
            Outcome::Unchanged
        ));
    }

    #[test]
    fn merge_skips_non_strict_json_without_clobber() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let original = "{\n  // a JSONC comment\n  \"context_servers\": {}\n}";
        std::fs::write(&p, original).unwrap();
        let entry = json!({ "command": "travsr" });
        assert!(matches!(
            merge_json_server(&p, "context_servers", &entry).unwrap(),
            Outcome::Skipped(_)
        ));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), original);
    }

    #[test]
    fn managed_block_replaces_not_duplicates() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("CLAUDE.md");
        std::fs::write(&p, "# My rules\n\nkeep me\n").unwrap();
        upsert_block(&p, MD_BEGIN, MD_END, "v1").unwrap();
        upsert_block(&p, MD_BEGIN, MD_END, "v2").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches(MD_BEGIN).count(), 1);
        assert!(text.contains("v2"));
        assert!(!text.contains("v1"));
        assert!(text.contains("keep me"));
    }

    #[test]
    fn managed_block_skips_malformed_markers() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("CLAUDE.md");
        std::fs::write(&p, format!("{MD_BEGIN}\nx\n{MD_BEGIN}\ny\n{MD_END}\n")).unwrap();
        assert!(matches!(
            upsert_block(&p, MD_BEGIN, MD_END, "v2").unwrap(),
            Outcome::Skipped(_)
        ));
    }

    #[test]
    fn remove_block_cleans_up() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("CLAUDE.md");
        std::fs::write(&p, "head\n").unwrap();
        upsert_block(&p, MD_BEGIN, MD_END, "body").unwrap();
        remove_block(&p, MD_BEGIN, MD_END).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains(MD_BEGIN));
        assert!(text.contains("head"));
    }

    #[test]
    fn remove_json_server_keeps_other_servers() {
        let dir = tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        std::fs::write(
            &p,
            r#"{"mcpServers":{"other":{"command":"x"},"travsr":{"command":"travsr"}}}"#,
        )
        .unwrap();
        assert!(matches!(
            remove_json_server(&p, "mcpServers").unwrap(),
            Outcome::Removed
        ));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert!(v["mcpServers"].get("travsr").is_none());
        // Removing again is a no-op.
        assert!(matches!(
            remove_json_server(&p, "mcpServers").unwrap(),
            Outcome::Absent
        ));
    }

    #[test]
    fn managed_block_appends_with_separator_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let p = dir.path().join(".rules");
        // No trailing newline — must not glue the block onto the last line.
        std::fs::write(&p, "last line").unwrap();
        upsert_block(&p, MD_BEGIN, MD_END, "body").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("last line\n"));
        assert!(text.contains(&format!("{MD_BEGIN}\nbody\n{MD_END}")));
        // Re-running with the same body changes nothing.
        assert!(matches!(
            upsert_block(&p, MD_BEGIN, MD_END, "body").unwrap(),
            Outcome::Unchanged
        ));
    }

    #[test]
    fn write_atomic_replaces_existing_content() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("nested/file.json");
        write_atomic(&p, "v1").unwrap();
        write_atomic(&p, "v2").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "v2");
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp file not cleaned up");
    }

    #[test]
    fn detect_uses_project_markers() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        assert!(matches!(Tool::Cursor.detect(repo, None), Detection::None));
        std::fs::create_dir(repo.join(".cursor")).unwrap();
        assert!(matches!(Tool::Cursor.detect(repo, None), Detection::Auto));
    }

    #[test]
    fn bare_vscode_is_not_a_copilot_auto_signal() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        std::fs::create_dir(repo.join(".vscode")).unwrap();
        assert!(matches!(
            Tool::VsCodeCopilot.detect(repo, None),
            Detection::Print
        ));
        std::fs::write(repo.join(".vscode/mcp.json"), "{}").unwrap();
        assert!(matches!(
            Tool::VsCodeCopilot.detect(repo, None),
            Detection::Auto
        ));
    }

    #[test]
    fn copilot_entry_has_stdio_type() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        let planned = Tool::VsCodeCopilot.plan(repo, &cmd());
        let json_plan = planned
            .iter()
            .find_map(|p| match &p.content {
                Content::JsonServer { entry, .. } => Some(entry),
                _ => None,
            })
            .unwrap();
        assert_eq!(json_plan["type"], "stdio");
    }

    #[test]
    fn only_dedicated_mcp_files_are_gitignored() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        for tool in Tool::ALL {
            for p in tool.plan(repo, &cmd()) {
                let name = p.path.file_name().unwrap().to_string_lossy().into_owned();
                let is_dedicated_mcp = matches!(name.as_str(), "mcp.json" | ".mcp.json");
                // CLAUDE.md, copilot-instructions.md, .rules, Zed settings.json,
                // and the cursor .mdc must never be git-ignored.
                assert_eq!(
                    p.gitignore, is_dedicated_mcp,
                    "wrong gitignore policy for {name}"
                );
            }
        }
    }

    #[test]
    fn cursor_mdc_has_always_apply_frontmatter() {
        assert!(cursor_mdc().starts_with("---\n"));
        assert!(cursor_mdc().contains("alwaysApply: true"));
    }
}
