//! `travsr connect`: wire detected AI coding tools to the Travsr MCP server and
//! drop an always-on "use Travsr first" rules file for each. Also invoked by
//! `travsr init` (RFC-026).
//!
//! Two co-equal outputs per detected tool:
//!   1. register `travsr mcp --stdio` in the tool's MCP config, and
//!   2. write an always-on rules/instructions file directing the agent to query
//!      Travsr before grep/find. Wiring alone does not change agent behavior, the
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
    "# travsr:begin (generated AI-tool config, `travsr connect --remove` to undo)";
const GI_END: &str = "# travsr:end";

/// Where a run's report goes.
///
/// This is not cosmetic. Connect writes into tracked, user-authored files
/// (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `.github/copilot-instructions.md`)
/// which are deliberately not git-ignored, so a plain `travsr init` leaves the
/// working tree dirty. RFC-026 promises the wiring is "non-fatal, but visible";
/// dropping the report entirely because stdout is busy would keep the writes and
/// lose the visibility.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Report {
    /// Interactive run: report on stdout.
    Stdout,
    /// `travsr init --json`: stdout carries the machine-readable summary, so the
    /// report goes to stderr instead of being discarded.
    Stderr,
    /// `travsr init --quiet`: the user asked for silence explicitly.
    Silent,
}

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
    /// Also write the always-on rules file that tells an agent to prefer the
    /// graph over grep. Off by default: that file is re-sent on every turn of
    /// every conversation, so it is the one part of travsr with a recurring
    /// token cost, and it is not needed for the tools to work. MCP already
    /// hands the model every tool name and description.
    pub rules: bool,
    /// Where the report goes. Never affects what is written.
    pub report: Report,
}

impl ConnectOpts {
    pub fn auto() -> Self {
        Self {
            only: None,
            dry_run: false,
            remove: false,
            commit: false,
            rules: false,
            report: Report::Stdout,
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

/// The directive itself. Kept deliberately short: this text is prepended to every
/// agent turn, so each line has to earn its tokens. It maps questions to tools
/// rather than listing signatures, because the agent already receives the full
/// schemas from `tools/list`, what it lacks is the routing.
///
/// Mirrors the tool set advertised by `travsr-mcp`'s **single-repo** `tools_list`,
/// which is what the command we write (`travsr mcp --stdio`, no `--global`)
/// serves. That session is bound to one `graph.db`, so none of these tools takes
/// a `repo` argument and every schema is closed (`additionalProperties: false`).
/// The multi-repo surface behind `travsr mcp --global` is a different tool list.
/// When a tool is added or renamed in single-repo mode, update this table.
/// The text written into every agent's always-on rules file.
///
/// Kept deliberately small. This is loaded on every turn of every conversation,
/// so each line is paid for again and again, and it competes for attention with
/// the user's own rules. The earlier version carried a question-to-tool routing
/// table: eleven rows naming the tool to call for each kind of question. That
/// was the bulk of it and it was redundant, because the MCP client already
/// hands the model all twenty-six tool names with their descriptions before the
/// conversation starts. Restating them here bought nothing and cost the tokens
/// twice.
///
/// What stays is the part a tool schema cannot say: query the graph *before*
/// grepping, and the handful of rules about how this server behaves. Detail
/// that a reader wants once, rather than on every turn, lives in the FAQ and is
/// one command away.
fn guide_body() -> String {
    "This repository is indexed by Travsr, a code graph served over MCP. For any \
question about where code lives or how it connects, query Travsr before grep, \
find or reading whole files. It answers from the graph, so it is token-cheap and \
does not invent structure that is not there.

The tool names and their descriptions arrive over MCP, so choose from those.

1. This server is bound to this repository alone, so no tool takes a `repo` \
   argument. Every schema is closed: pass only the arguments it names.
2. Start open-ended questions with get_context and include_snippets=true. It \
   returns the source inline, so there is no follow-up read.
3. Prefer find_pattern over your own grep: the same regex search, already scoped \
   to the indexed files, and `scope` narrows it further.
4. Read a whole file only after Travsr has told you which file, and only for \
   what the graph does not carry.
5. Fall back to plain text search when Travsr returns nothing, or reports the \
   index unavailable or stale.

For anything about Travsr itself, run: travsr ask \"travsr: <question>\""
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

/// Zed loads exactly one project instruction file: the first that exists from
/// this list, and it stops there.
///
/// That makes creating `.rules` unconditionally a destructive act. `.rules` sits
/// at the top, so writing one into a repo whose rules live in `CLAUDE.md` moves
/// Zed off `CLAUDE.md` entirely, and the user silently loses every rule they had
/// in exchange for ours. So we append our block to the file Zed already reads,
/// and fall back to creating `.rules` only when the repo has none of them.
const ZED_INSTRUCTION_FILES: [&str; 9] = [
    ".rules",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    ".github/copilot-instructions.md",
    "AGENT.md",
    "AGENTS.md",
    "CLAUDE.md",
    "GEMINI.md",
];

fn zed_instruction_file(repo: &Path) -> PathBuf {
    ZED_INSTRUCTION_FILES
        .iter()
        .map(|f| repo.join(f))
        .find(|p| p.exists())
        .unwrap_or_else(|| repo.join(".rules"))
}

// ---------------------------------------------------------------------------
// Tools.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    ClaudeCode,
    Cursor,
    VsCodeCopilot,
    GeminiCli,
    Antigravity,
    Codex,
    Windsurf,
    Zed,
}

/// How a tool was detected, which decides whether we auto-write project files or
/// just print a snippet (never auto-write into a repo for a tool only known from a
/// global/home marker).
enum Detection {
    /// Project-local marker present, safe to write project-scoped config.
    Auto,
    /// Only a global marker (or no project MCP file), print a snippet instead.
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

impl Content {
    /// Whether this is guidance text rather than MCP wiring.
    ///
    /// The wiring is what makes the tools reachable; the guidance is prose an
    /// agent re-reads every turn. They have different costs, so `connect` treats
    /// them differently and this is the line between them.
    fn is_guidance(&self) -> bool {
        !matches!(self, Content::JsonServer { .. })
    }
}

struct Planned {
    path: PathBuf,
    content: Content,
    /// Whether to git-ignore this file by default. True only for dedicated MCP
    /// config files (the committed-server RCE-on-clone vector). Shared files the
    /// user owns and commits (`CLAUDE.md`, `copilot-instructions.md`, Zed's
    /// general `settings.json`, rules files) are never git-ignored.
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
    const ALL: [Tool; 8] = [
        Tool::ClaudeCode,
        Tool::Cursor,
        Tool::VsCodeCopilot,
        Tool::GeminiCli,
        Tool::Antigravity,
        Tool::Codex,
        Tool::Windsurf,
        Tool::Zed,
    ];

    fn id(&self) -> &'static str {
        match self {
            Tool::ClaudeCode => "claude-code",
            Tool::Cursor => "cursor",
            Tool::VsCodeCopilot => "vscode-copilot",
            Tool::GeminiCli => "gemini-cli",
            Tool::Antigravity => "antigravity",
            Tool::Codex => "codex",
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
            // Bare `.vscode/` is in most repos without Copilot, so only auto-write
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
            // Look for `settings.json` by name, never a bare `.gemini/`. Antigravity
            // also lives under `~/.gemini` (its own config is
            // `~/.gemini/config/mcp_config.json`), so the directory alone is not
            // evidence of Gemini CLI, and treating it as such told Antigravity users
            // to edit a file their tool never reads. `GEMINI.md` is likewise shared
            // between the two and cannot identify either.
            Tool::GeminiCli => {
                if has(repo.join(".gemini/settings.json")) {
                    Detection::Auto
                } else if home.is_some_and(|h| has(h.join(".gemini/settings.json"))) {
                    Detection::Print
                } else {
                    Detection::None
                }
            }
            // Antigravity reads GEMINI.md / AGENTS.md for rules, but its MCP servers
            // live in a global `~/.gemini/config/mcp_config.json` (see `note`).
            Tool::Antigravity => {
                if has(repo.join(".antigravitycli")) || has(repo.join(".antigravity")) {
                    Detection::Auto
                } else if home.is_some_and(|h| {
                    has(h.join(".gemini/antigravity")) || has(h.join(".antigravity"))
                }) {
                    Detection::Print
                } else {
                    Detection::None
                }
            }
            // Codex keeps its MCP servers in a global TOML we do not edit, so an
            // auto-detect here writes the AGENTS.md rules only (see `note`).
            //
            // `AGENTS.md` is NOT a marker: it is a cross-tool convention, read by
            // Zed (it is in ZED_INSTRUCTION_FILES above) among others. Treating it
            // as evidence of Codex reports `codex (configured)` in any repo that
            // adopted the convention, and prints a note telling the user to edit
            // `~/.codex/config.toml` for a tool they do not have. That is the same
            // shared-file-as-marker error that made an Antigravity-only `~/.gemini`
            // read as Gemini CLI. `.codex/` identifies exactly one tool.
            Tool::Codex => {
                if has(repo.join(".codex")) {
                    Detection::Auto
                } else if home.is_some_and(|h| has(h.join(".codex"))) {
                    Detection::Print
                } else {
                    Detection::None
                }
            }
            // Windsurf keeps MCP config in the home dir, which we never write, but
            // rules are project-scoped, so a project marker is still worth an
            // auto-write (plus the `note` telling the user about the MCP half).
            Tool::Windsurf => {
                if has(repo.join(".windsurf")) {
                    Detection::Auto
                } else if home.is_some_and(|h| has(h.join(".codeium/windsurf"))) {
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
                        // Cursor documents `type` as required for a local server,
                        // the same as VS Code. Omitting it is not a tolerated
                        // default, so the flat shape does not work here.
                        entry: json!({ "type": "stdio", "command": cmd.command, "args": cmd.args }),
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
            Tool::GeminiCli => vec![
                Planned {
                    path: repo.join(".gemini/settings.json"),
                    content: Content::JsonServer {
                        top_key: "mcpServers",
                        entry: flat,
                    },
                    // Gemini's project settings.json is a general config file the
                    // user owns, but it is also where the server definition lands,
                    // so it takes the same local-only treatment as an mcp.json.
                    gitignore: true,
                },
                Planned {
                    path: repo.join("GEMINI.md"),
                    content: Content::ManagedMd {
                        body: markdown_rules(),
                    },
                    gitignore: false,
                },
            ],
            // GEMINI.md only: Antigravity's MCP servers live in the global
            // ~/.gemini/config/mcp_config.json. `note` prints that step.
            Tool::Antigravity => vec![Planned {
                path: repo.join("GEMINI.md"),
                content: Content::ManagedMd {
                    body: markdown_rules(),
                },
                gitignore: false,
            }],
            // AGENTS.md only: Codex reads MCP servers from ~/.codex/config.toml,
            // a global TOML outside this repo. `note` prints that step.
            Tool::Codex => vec![Planned {
                path: repo.join("AGENTS.md"),
                content: Content::ManagedMd {
                    body: markdown_rules(),
                },
                gitignore: false,
            }],
            // Rules only, for the same reason as Codex: Windsurf's MCP config is
            // ~/.codeium/windsurf/mcp_config.json.
            Tool::Windsurf => vec![Planned {
                path: repo.join(".windsurf/rules/travsr.md"),
                content: Content::Owned {
                    text: markdown_rules(),
                },
                gitignore: false,
            }],
            Tool::Zed => vec![
                Planned {
                    // Shared general settings file, not git-ignored.
                    path: repo.join(".zed/settings.json"),
                    content: Content::JsonServer {
                        top_key: "context_servers",
                        // Zed's documented shape is flat, not the nested
                        // {command: {path, args}} form the RFC left open.
                        entry: flat,
                    },
                    gitignore: false,
                },
                Planned {
                    path: zed_instruction_file(repo),
                    content: Content::ManagedMd {
                        body: markdown_rules(),
                    },
                    gitignore: false,
                },
            ],
        }
    }

    /// Extra manual step printed after an auto-write, for tools whose MCP config
    /// lives in a global file this command does not touch. Without it the user
    /// gets the rules half of the wiring and no server, with no hint why.
    fn note(&self, cmd: &McpCommand) -> Option<String> {
        match self {
            Tool::Antigravity => Some(format!(
                "  rules only. Antigravity reads MCP servers from \
                 ~/.gemini/config/mcp_config.json:\n{}",
                indent(&mcp_servers_json(cmd))
            )),
            Tool::Codex => Some(format!(
                "  rules only. Codex reads MCP servers from ~/.codex/config.toml \
                 (or .codex/config.toml in this repo), add:\n    \
                 [mcp_servers.travsr]\n    command = \"{}\"\n    args = [\"mcp\", \"--stdio\"]",
                cmd.command
            )),
            Tool::Windsurf => Some(format!(
                "  rules only. Add the server to ~/.codeium/windsurf/mcp_config.json:\n{}",
                indent(&mcp_servers_json(cmd))
            )),
            _ => None,
        }
    }

    /// A follow-up step a tool needs after its server file is written but before
    /// the server actually loads. Distinct from `note`: `note` marks a rules-only
    /// adapter whose MCP config lives in a global file travsr does not write,
    /// whereas this is for a tool travsr *does* wire, that still gates the server
    /// behind a user action.
    ///
    /// Claude Code will not load a project-scoped `.mcp.json` until it is approved
    /// once (`enabledMcpjsonServers`). Writing the file reports `ok`, but the
    /// travsr tools stay inert until that approval, so a bare `ok` reads as done
    /// when it is not (#829).
    fn approval_hint(&self) -> Option<&'static str> {
        match self {
            Tool::ClaudeCode => Some(
                "  note: Claude Code loads a project .mcp.json only after a one-time \
                 approval. If the travsr tools are not available, restart Claude Code \
                 and accept the trust prompt, or run /mcp to enable the travsr server.",
            ),
            _ => None,
        }
    }

    /// Snippet printed when only a global marker is present.
    fn snippet(&self, repo: &Path, cmd: &McpCommand) -> String {
        let server = indent(&mcp_servers_json(cmd));
        match self {
            Tool::Antigravity => format!(
                "  add to ~/.gemini/config/mcp_config.json:\n{server}\n  \
                 and put the Travsr guidance in {}/GEMINI.md",
                repo.display()
            ),
            Tool::Codex => format!(
                "  add [mcp_servers.travsr] to ~/.codex/config.toml, and put the Travsr \
                 guidance in {}/AGENTS.md",
                repo.display()
            ),
            Tool::Windsurf => format!(
                "  add to ~/.codeium/windsurf/mcp_config.json:\n{server}\n  \
                 and create {}/.windsurf/rules/travsr.md with the Travsr guidance",
                repo.display()
            ),
            _ => format!("  MCP server config:\n{server}"),
        }
    }
}

fn mcp_servers_json(cmd: &McpCommand) -> String {
    serde_json::to_string_pretty(
        &json!({ "mcpServers": { "travsr": { "command": cmd.command, "args": cmd.args } } }),
    )
    .unwrap_or_default()
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// File operations.
// ---------------------------------------------------------------------------

/// Atomically replace `path` with `content`: write a sibling temp file, fsync it,
/// then rename over the target. Because these helpers do read-modify-write of the
/// user's own files (CLAUDE.md, an existing mcp.json), a crash mid-write must
/// never truncate the original. Same temp+rename pattern as `install.rs`.
///
/// The fsync is what makes that claim true across a power loss rather than only
/// against concurrent readers: rename is atomic in the directory entry, but
/// without flushing the temp file first a crash can leave the renamed name
/// pointing at zero bytes on several filesystems. The parent directory is synced
/// too, best-effort, so the rename itself survives.
///
/// When the target already exists its mode is carried over. The temp file is
/// created under the process umask, so replacing a `0600` `CLAUDE.md` without
/// this would silently widen it to `0644`.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    use std::io::Write as _;

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

    let write = || -> Result<()> {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("creating temp {}", tmp.display()))?;
        f.write_all(content.as_bytes())
            .with_context(|| format!("writing temp {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Ok(meta) = std::fs::metadata(path) {
                let mode = meta.permissions().mode();
                let _ = f.set_permissions(std::fs::Permissions::from_mode(mode));
            }
        }
        f.sync_all()
            .with_context(|| format!("syncing temp {}", tmp.display()))?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("renaming into {}", path.display()));
    }
    // Best-effort: makes the rename itself durable. Not all platforms allow
    // opening a directory, and a failure here does not invalidate the write.
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Upsert one server under `root[top_key]["travsr"]`. Skips (never clobbers) a
/// file that does not parse as strict JSON or whose shape is unexpected.
///
/// `refuse_new` declines to introduce the entry at all: set for a git-tracked
/// config, where writing the server definition puts it in the next commit and
/// `.gitignore` cannot take it back. Refusing is what makes `--commit` the
/// consent gate for a shared server definition rather than an acknowledgement
/// collected after the modification already sits in the working tree. An entry
/// that already matches is left alone and reported `Unchanged`, since re-running
/// over an already-committed config adds no exposure that is not already there.
fn merge_json_server(
    path: &Path,
    top_key: &str,
    entry: &Value,
    refuse_new: bool,
) -> Result<Outcome> {
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
    if refuse_new {
        return Ok(Outcome::Skipped(
            "tracked by git, so .gitignore cannot keep it local (re-run with \
             --commit to share it)"
                .into(),
        ));
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
    // Exactly one balanced pair, replace the region.
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
        // Whole file was our block, remove it.
        std::fs::remove_file(path).ok();
        return Ok(Outcome::Removed);
    }
    write_atomic(path, &new)?;
    Ok(Outcome::Removed)
}

fn execute(p: &Planned, remove: bool, refuse_new: bool) -> Result<Outcome> {
    match &p.content {
        Content::JsonServer { top_key, entry } => {
            if remove {
                remove_json_server(&p.path, top_key)
            } else {
                merge_json_server(&p.path, top_key, entry, refuse_new)
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
                if !p.path.exists() {
                    return Ok(Outcome::Absent);
                }
                // Only delete a file we still fully own. A user who tuned the
                // generated rule has content we did not write, and discarding it
                // silently is the destructive edit every other path in this
                // module refuses to make (strict-JSON-or-skip, malformed markers).
                let current = std::fs::read_to_string(&p.path)
                    .with_context(|| format!("reading {}", p.path.display()))?;
                if current != *text {
                    return Ok(Outcome::Skipped(
                        "edited since travsr generated it (left untouched)".into(),
                    ));
                }
                std::fs::remove_file(&p.path)
                    .with_context(|| format!("removing {}", p.path.display()))?;
                Ok(Outcome::Removed)
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

/// Entries currently listed inside a managed block, in file order. Empty when the
/// file, or a well-formed block, is absent.
fn block_entries(path: &Path, begin: &str, end: &str) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let (Some(start), Some(estart)) = (text.find(begin), text.find(end)) else {
        return Ok(Vec::new());
    };
    let body_start = start + begin.len();
    if estart < body_start {
        return Ok(Vec::new());
    }
    Ok(text[body_start..estart]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Repo-root-anchored form of a generated entry.
///
/// Git treats a pattern containing no slash as matching at **any** depth, so a
/// bare `.mcp.json` would also ignore `vendor/thing/.mcp.json`, while
/// `.cursor/mcp.json` is already anchored to the `.gitignore`'s directory. That
/// left the block's entries meaning two different things. A leading `/` anchors
/// every one of them to the repo root.
fn anchored(rel: &str) -> String {
    format!("/{}", rel.trim_start_matches('/'))
}

/// Which of `rels` git already tracks.
///
/// `.gitignore` has no effect on a tracked path, so an entry for one is inert:
/// the travsr server definition stays in the index and goes out with the next
/// commit. Reporting "generated files are local-only" in that case states the
/// opposite of what git will do.
fn tracked(repo: &Path, rels: &[String]) -> Vec<String> {
    if rels.is_empty() {
        return Vec::new();
    }
    // Bounded (#717 triage): an unbounded `Command::output()` here can hang the
    // CLI forever on Windows if git or something it spawns inherits the stdout
    // pipe and never closes it (#503 / #572). `repo` goes through as the
    // working directory rather than a `-C <string>` argument for the same
    // reason `main_worktree_root` does: a path with bytes that are not valid
    // UTF-8 is legal, and converting it to a string first would mangle it.
    let mut args: Vec<&str> = vec!["ls-files", "--error-unmatch", "--"];
    args.extend(rels.iter().map(|r| r.trim_start_matches('/')));
    // `--error-unmatch` makes git exit non-zero when any path is untracked, but
    // it still lists the tracked ones on stdout, which is what we read.
    //
    // Fails open: no git on PATH, a repo git cannot read, or git that does not
    // answer within the bound, yields "nothing is tracked", so the adapters
    // write as usual instead of refusing everything on a machine that cannot
    // answer the question. The cost is that the refusal and its warning
    // silently do not fire there.
    let Some(out) = crate::git_bounded::git_output_bounded(Some(repo), args) else {
        return Vec::new();
    };
    let listed: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| anchored(l.trim()))
        .collect();
    rels.iter()
        .filter(|r| listed.contains(&anchored(r)))
        .cloned()
        .collect()
}

/// Maintain a `.gitignore` block listing the generated repo-relative paths.
///
/// The block accumulates across runs instead of being replaced, because a run
/// does not see every tool: `--tool cursor` plans only Cursor's files, so
/// rewriting the block from that run alone would drop `.mcp.json` out of it and
/// silently make a still-present MCP server definition committable, which is the
/// RCE-on-clone vector this block exists to close.
///
/// `add` and `drop` are applied in one pass so every caller goes through the
/// same path: a remove run drops what it unwired, and a `--commit` run drops
/// what it is opting in to committing. The block is deleted once nothing is left.
fn ensure_gitignored(repo: &Path, add: &[String], drop: &[String]) -> Result<Outcome> {
    let path = repo.join(".gitignore");
    // Normalise on read so a block written before anchoring migrates in place
    // rather than gaining a second entry for the same file.
    let mut entries: Vec<String> = block_entries(&path, GI_BEGIN, GI_END)?
        .iter()
        .map(|e| anchored(e))
        .collect();
    let drop: Vec<String> = drop.iter().map(|d| anchored(d)).collect();
    entries.retain(|e| !drop.contains(e));
    for a in add.iter().map(|a| anchored(a)) {
        if !entries.contains(&a) {
            entries.push(a);
        }
    }
    if entries.is_empty() {
        return remove_block(&path, GI_BEGIN, GI_END);
    }
    upsert_block(&path, GI_BEGIN, GI_END, &entries.join("\n"))
}

/// Markdown-block paths claimed by more than one auto-detected tool, mapped to
/// the ids of every tool that claims them. Used to keep a `--tool X --remove`
/// from stripping a block another detected tool still relies on.
fn shared_md_paths(
    repo: &Path,
    home: Option<&Path>,
    cmd: &McpCommand,
) -> std::collections::HashMap<PathBuf, Vec<&'static str>> {
    let mut claims: std::collections::HashMap<PathBuf, Vec<&'static str>> = Default::default();
    for tool in Tool::ALL {
        if !matches!(tool.detect(repo, home), Detection::Auto) {
            continue;
        }
        for planned in tool.plan(repo, cmd) {
            if matches!(planned.content, Content::ManagedMd { .. }) {
                claims.entry(planned.path).or_default().push(tool.id());
            }
        }
    }
    claims.retain(|_, ids| ids.len() > 1);
    claims
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

    // The report sink never affects what is written.
    macro_rules! say {
        ($($arg:tt)*) => {
            match opts.report {
                Report::Stdout => println!($($arg)*),
                Report::Stderr => eprintln!($($arg)*),
                Report::Silent => {}
            }
        };
    }

    // Warnings survive `Silent`. `--quiet` silences what the run *did*, not what
    // it could not guarantee: the only thing routed here says a server
    // definition is committed and will auto-load for anyone who clones, and
    // `travsr init --quiet` is the unattended path where that is least likely to
    // be noticed otherwise. Always stderr, so it never lands in `--json` stdout.
    macro_rules! warn {
        ($($arg:tt)*) => { eprintln!($($arg)*) };
    }

    let mut detected = false;
    // Paths to add to the .gitignore block, and paths to drop from it. Kept
    // apart because a remove run must subtract exactly what it unwired and leave
    // every other tool's entry standing.
    let mut gitignore: Vec<String> = Vec::new();
    let mut unignore: Vec<String> = Vec::new();

    // Markdown blocks that more than one detected tool relies on. Several
    // adapters share a target file (GEMINI.md is planned by both Gemini CLI and
    // Antigravity; Zed resolves to whichever instruction file already exists, so
    // it lands on CLAUDE.md next to Claude Code) and they all use the same
    // markers. A `--tool X --remove` that stripped such a block would unwire a
    // tool it was never asked to touch, and because the file is often nothing but
    // our block, delete that file outright.
    let shared = shared_md_paths(repo_root, home.as_deref(), &cmd);

    // Config files that carry a server definition and that git already tracks.
    // Computed before anything is written, because for these `.gitignore` is
    // inert: the definition is in the index and goes out with the next commit.
    // Without `--commit` the adapters decline to introduce it rather than
    // modifying the file and warning afterwards.
    let tracked_server_files: Vec<String> = if opts.remove || opts.commit {
        Vec::new()
    } else {
        let planned: Vec<String> = Tool::ALL
            .iter()
            .filter(|t| matches!(t.detect(repo_root, home.as_deref()), Detection::Auto))
            .flat_map(|t| t.plan(repo_root, &cmd))
            .filter(|p| p.gitignore)
            .filter_map(|p| rel(repo_root, &p.path))
            .collect();
        tracked(repo_root, &planned)
    };

    // Guidance is opt-in, because it is the only part of travsr with a per-turn
    // cost: an always-on rules file is re-read on every turn of every
    // conversation, where the MCP wiring is read once at startup. Removal is not
    // filtered, or `--remove` would strand a rules file written by an earlier
    // run with `--rules`.
    // Already written by an earlier run, so keep it current.
    //
    // Filtering guidance out unconditionally froze it: `upsert_block` never ran,
    // so a `<!-- travsr:begin -->` block already in someone's CLAUDE.md kept the
    // old 2270-character body forever. The token saving this is built around
    // would have reached only people who had never run `connect`, which is
    // nobody, while `the_always_on_guidance_stays_small` passed against a
    // shipped file twice the budget.
    //
    // Presence of our block is the test, not presence of the file. CLAUDE.md
    // usually exists for reasons that have nothing to do with travsr, and
    // writing guidance into it because it happens to be there would put back the
    // opt-out this change removed.
    let already_written = |p: &Planned| match &p.content {
        Content::ManagedMd { .. } => std::fs::read_to_string(&p.path)
            .map(|t| t.contains(MD_BEGIN))
            .unwrap_or(false),
        // A file travsr owns outright: it exists only because we wrote it.
        Content::Owned { .. } => p.path.exists(),
        Content::JsonServer { .. } => false,
    };
    let wanted =
        |p: &Planned| opts.remove || opts.rules || !p.content.is_guidance() || already_written(p);

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
                let full = tool.plan(repo_root, &cmd);
                let full_len = full.len();
                let kept: Vec<Planned> = full.into_iter().filter(&wanted).collect();
                // Codex, Antigravity and Windsurf read their MCP config from a
                // global file, so a rules file is the *only* thing travsr writes
                // for them. Filtering it leaves nothing, and a tool heading with
                // no lines under it reads as a failure rather than a choice.
                // Say so whenever guidance was skipped, not only when nothing
                // at all was written. For claude-code, cursor, copilot and zed
                // the `JsonServer` entry survives, so `kept` is non-empty and a
                // default run printed a normal-looking report with the guidance
                // line simply absent: no signal that anything was withheld or
                // that a flag exists (#746 review).
                let skipped_guidance = kept.len() < full_len;
                if kept.is_empty() {
                    say!("  nothing to write; rules are opt-in, pass --rules");
                } else if skipped_guidance && !opts.remove {
                    say!("  (agent guidance not written; pass --rules to include it)");
                }
                for planned in kept {
                    let disp = rel(repo_root, &planned.path)
                        .unwrap_or_else(|| planned.path.display().to_string());

                    // A filtered remove must leave co-owned blocks alone. With no
                    // filter every claimant is in this run, so the block is ours
                    // to take.
                    if opts.remove && opts.only.is_some() {
                        if let Some(others) = shared.get(&planned.path) {
                            let remaining: Vec<&str> = others
                                .iter()
                                .filter(|id| Some(**id) != opts.only.as_deref())
                                .copied()
                                .collect();
                            if !remaining.is_empty() {
                                say!(
                                    "  skipped {disp}: shared with {} (run without --tool to remove)",
                                    remaining.join(", ")
                                );
                                continue;
                            }
                        }
                    }

                    if opts.dry_run {
                        say!(
                            "  would {} {disp}",
                            if opts.remove { "remove" } else { "write" }
                        );
                        if planned.gitignore {
                            if let Some(r) = rel(repo_root, &planned.path) {
                                if opts.remove || opts.commit {
                                    unignore.push(r);
                                } else {
                                    gitignore.push(r);
                                }
                            }
                        }
                        continue;
                    }
                    let refuse_new = planned.gitignore
                        && rel(repo_root, &planned.path)
                            .is_some_and(|r| tracked_server_files.contains(&r));
                    match execute(&planned, opts.remove, refuse_new) {
                        Ok(outcome) => {
                            match &outcome {
                                Outcome::Skipped(reason) => {
                                    say!("  skipped {disp}: {reason}")
                                }
                                other => say!("  {} {disp}", label(other)),
                            }
                            if planned.gitignore {
                                if let Some(r) = rel(repo_root, &planned.path) {
                                    if opts.remove {
                                        // Unwired (or never there): the file no
                                        // longer carries a travsr server, so its
                                        // ignore entry has nothing left to guard.
                                        if matches!(outcome, Outcome::Removed | Outcome::Absent) {
                                            unignore.push(r);
                                        }
                                    } else if opts.commit {
                                        // --commit opts in to committing these
                                        // files. Leaving an entry a previous
                                        // default run added would keep them
                                        // ignored and make the opt-in a no-op.
                                        unignore.push(r);
                                    } else if matches!(
                                        outcome,
                                        Outcome::Written | Outcome::Unchanged
                                    ) {
                                        gitignore.push(r);
                                    }
                                }
                            }
                        }
                        // Per-file failure is non-fatal; report and continue.
                        Err(e) => say!("  error {disp}: {e}"),
                    }
                }
                if !opts.remove {
                    if let Some(note) = tool.note(&cmd) {
                        say!("{note}");
                    }
                    // #829: naming the still-pending approval where a bare `ok`
                    // otherwise reads as done. Not gated on the write outcome:
                    // approval is still needed whether the file was just written
                    // or already present and committed.
                    if let Some(hint) = tool.approval_hint() {
                        say!("{hint}");
                    }
                }
            }
            Detection::Print => {
                detected = true;
                say!("{} detected (global), add manually:", tool.id());
                say!("{}", tool.snippet(repo_root, &cmd));
            }
            Detection::None => {}
        }
    }

    if !detected {
        say!(
            "tip: no AI coding tool detected. Run `travsr connect` after installing \
             Claude Code, Cursor, Copilot, Gemini CLI, Codex, Windsurf, or Zed"
        );
        return Ok(());
    }

    // What is left after the refusal above: a tracked config that already holds
    // our exact entry, so there was nothing to write and nothing to decline. The
    // definition is still committed and will still auto-load for a cloner, and
    // `.gitignore` cannot take it back, so the local-only claim has to be dropped
    // and the state named.
    let already_tracked: Vec<String> = gitignore
        .iter()
        .filter(|r| tracked_server_files.contains(r))
        .cloned()
        .collect();

    if opts.dry_run {
        for r in &gitignore {
            say!("  would ignore {}", anchored(r));
        }
        for r in &unignore {
            say!("  would un-ignore {}", anchored(r));
        }
    } else {
        match ensure_gitignored(repo_root, &gitignore, &unignore) {
            Ok(Outcome::Written) if already_tracked.is_empty() => say!(
                "  {} .gitignore (generated files are local-only)",
                label(&Outcome::Written)
            ),
            Ok(Outcome::Written) => say!("  {} .gitignore", label(&Outcome::Written)),
            Ok(Outcome::Removed) => say!("  {} .gitignore", label(&Outcome::Removed)),
            _ => {}
        }
    }

    for r in &already_tracked {
        warn!(
            "warning: {r} is tracked by git and already holds the travsr server \
             definition, so .gitignore cannot keep it local. It will auto-load for \
             anyone who clones. `git rm --cached {r}` to untrack it.",
        );
    }

    if !opts.remove && !cmd.on_path() {
        say!(
            "note: `travsr` is not on PATH, so configs use an absolute path. Add \
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
        merge_json_server(&p, "mcpServers", &entry, false).unwrap();
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
            merge_json_server(&p, "mcpServers", &entry, false).unwrap(),
            Outcome::Written
        ));
        assert!(matches!(
            merge_json_server(&p, "mcpServers", &entry, false).unwrap(),
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
            merge_json_server(&p, "context_servers", &entry, false).unwrap(),
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
        // No trailing newline, must not glue the block onto the last line.
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

    /// Each tool's config shape, checked against that tool's own documentation.
    /// These are the schemas the adapters were wrong about at least once, so the
    /// table is written out in full rather than left to per-tool assertions.
    #[test]
    fn server_entries_match_each_tool_documented_schema() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // (tool, top-level key, requires an explicit `type: "stdio"`)
        let expected = [
            // Verified live: `claude mcp list` loads this file and connects.
            (Tool::ClaudeCode, "mcpServers", false),
            // Cursor documents `type` as required for a local server.
            (Tool::Cursor, "mcpServers", true),
            // Verified against VS Code 1.109's own bundle: `servers` + `inputs`.
            (Tool::VsCodeCopilot, "servers", true),
            (Tool::GeminiCli, "mcpServers", false),
            // Zed's documented shape is flat, not {command: {path, args}}.
            (Tool::Zed, "context_servers", false),
        ];
        for (tool, top_key, needs_type) in expected {
            let plan = tool.plan(repo, &cmd());
            let (key, entry) = plan
                .iter()
                .find_map(|p| match &p.content {
                    Content::JsonServer { top_key, entry } => Some((*top_key, entry)),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{} plans no server entry", tool.id()));
            assert_eq!(key, top_key, "wrong top-level key for {}", tool.id());
            assert_eq!(
                entry.get("type").is_some(),
                needs_type,
                "wrong `type` presence for {}",
                tool.id()
            );
            assert_eq!(entry["command"], "travsr");
            assert_eq!(entry["args"][0], "mcp");
        }
    }

    /// Antigravity and Gemini CLI both live under `~/.gemini`, and a bare
    /// directory check cannot tell them apart. This is the case that shipped
    /// wrong: a machine with Antigravity and no Gemini CLI was told to edit
    /// Gemini CLI's config.
    #[test]
    fn antigravity_is_not_mistaken_for_gemini_cli() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        // Exactly the layout Antigravity leaves behind: ~/.gemini exists, but
        // there is no ~/.gemini/settings.json.
        std::fs::create_dir_all(home.join(".gemini/antigravity")).unwrap();
        std::fs::create_dir_all(home.join(".gemini/config")).unwrap();

        let empty = tempdir().unwrap();
        let repo = empty.path();
        assert!(
            matches!(Tool::GeminiCli.detect(repo, Some(home)), Detection::None),
            "an Antigravity-only ~/.gemini must not read as Gemini CLI"
        );
        assert!(matches!(
            Tool::Antigravity.detect(repo, Some(home)),
            Detection::Print
        ));

        // A real Gemini CLI install writes settings.json, and that does count.
        std::fs::write(home.join(".gemini/settings.json"), "{}").unwrap();
        assert!(matches!(
            Tool::GeminiCli.detect(repo, Some(home)),
            Detection::Print
        ));
    }

    /// Zed reads the FIRST match from its instruction-file list and stops. If a
    /// repo keeps its rules in CLAUDE.md, creating `.rules` (which outranks it)
    /// silently swaps every rule the user had for ours.
    #[test]
    fn zed_appends_to_the_file_it_already_reads() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // Nothing present: `.rules` is the right thing to create.
        assert_eq!(zed_instruction_file(repo), repo.join(".rules"));

        // CLAUDE.md present: append there instead of shadowing it.
        std::fs::write(repo.join("CLAUDE.md"), "# house rules\n").unwrap();
        assert_eq!(zed_instruction_file(repo), repo.join("CLAUDE.md"));

        // A higher-priority file wins once it exists.
        std::fs::write(repo.join("AGENTS.md"), "").unwrap();
        assert_eq!(zed_instruction_file(repo), repo.join("AGENTS.md"));
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
    fn only_mcp_server_files_are_gitignored() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        for tool in Tool::ALL {
            for p in tool.plan(repo, &cmd()) {
                // A file is git-ignored exactly when it carries the server
                // definition (the RCE-on-clone vector). Every rules file, and
                // Zed's shared settings.json, stays committable.
                let carries_server =
                    matches!(p.content, Content::JsonServer { .. }) && tool != Tool::Zed;
                let name = p.path.file_name().unwrap().to_string_lossy().into_owned();
                assert_eq!(
                    p.gitignore, carries_server,
                    "wrong gitignore policy for {name}"
                );
            }
        }
    }

    /// A `--tool X` run plans only X's files. If the ignore block were rewritten
    /// from that run alone, another tool's still-present `.mcp.json` would fall
    /// out of it and become committable. A committed server definition is the
    /// RCE-on-clone vector the block exists to close.
    #[test]
    fn gitignore_block_accumulates_across_partial_runs() {
        let dir = tempdir().unwrap();
        let repo = dir.path();

        ensure_gitignored(repo, &[".mcp.json".into()], &[]).unwrap();
        ensure_gitignored(repo, &[".cursor/mcp.json".into()], &[]).unwrap();

        let entries = block_entries(&repo.join(".gitignore"), GI_BEGIN, GI_END).unwrap();
        assert_eq!(entries, vec!["/.mcp.json", "/.cursor/mcp.json"]);

        // Re-adding an entry already present must not duplicate it.
        ensure_gitignored(repo, &[".mcp.json".into()], &[]).unwrap();
        let entries = block_entries(&repo.join(".gitignore"), GI_BEGIN, GI_END).unwrap();
        assert_eq!(entries, vec!["/.mcp.json", "/.cursor/mcp.json"]);
    }

    /// Several adapters target the same markdown file with the same markers.
    /// A `--tool X --remove` that stripped a co-owned block would unwire a tool
    /// it was never asked to touch, and since such a file is often nothing but
    /// our block, delete the file outright.
    #[test]
    fn shared_markdown_blocks_are_claimed_by_every_owner() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // Gemini CLI and Antigravity both write their rules into GEMINI.md.
        std::fs::create_dir_all(repo.join(".gemini")).unwrap();
        std::fs::write(repo.join(".gemini/settings.json"), "{}").unwrap();
        std::fs::create_dir_all(repo.join(".antigravitycli")).unwrap();

        let shared = shared_md_paths(repo, None, &cmd());
        let owners = shared
            .get(&repo.join("GEMINI.md"))
            .expect("GEMINI.md is claimed by two detected tools");
        assert!(owners.contains(&"gemini-cli"), "owners: {owners:?}");
        assert!(owners.contains(&"antigravity"), "owners: {owners:?}");

        // A file only one tool claims is not shared.
        assert!(!shared.contains_key(&repo.join(".cursor/rules/travsr.mdc")));
    }

    /// `.gitignore` cannot un-share a path git already tracks, so the entry is
    /// inert and the "local-only" claim would be false.
    #[test]
    fn tracked_paths_are_detected() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            eprintln!("skipping: git not available");
            return;
        }
        std::fs::write(repo.join(".mcp.json"), "{}").unwrap();
        std::fs::write(repo.join("untracked.json"), "{}").unwrap();
        assert!(git(&["add", ".mcp.json"]));

        let found = tracked(
            repo,
            &[".mcp.json".to_string(), "untracked.json".to_string()],
        );
        assert_eq!(found, vec![".mcp.json"]);
    }

    /// Writing a server definition into a git-tracked config puts it in the next
    /// commit, and `.gitignore` cannot take it back. Declining is what makes
    /// `--commit` the consent gate, rather than warning once the modification is
    /// already sitting in the working tree.
    #[test]
    fn a_tracked_config_is_not_given_a_server_without_consent() {
        let dir = tempdir().unwrap();
        let p = dir.path().join(".mcp.json");
        std::fs::write(&p, r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        let entry = json!({ "command": "travsr", "args": ["mcp","--stdio"] });

        // Tracked and travsr is not in it: refuse, and leave the file byte-identical.
        let before = std::fs::read_to_string(&p).unwrap();
        assert!(matches!(
            merge_json_server(&p, "mcpServers", &entry, true).unwrap(),
            Outcome::Skipped(_)
        ));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);

        // --commit is the consent gate, and it writes.
        assert!(matches!(
            merge_json_server(&p, "mcpServers", &entry, false).unwrap(),
            Outcome::Written
        ));

        // Already committed with our exact entry: nothing to write and nothing to
        // decline, so report Unchanged and let the caller warn about the state.
        assert!(matches!(
            merge_json_server(&p, "mcpServers", &entry, true).unwrap(),
            Outcome::Unchanged
        ));
    }

    /// A user who tuned a generated rule file has content travsr did not write.
    /// Deleting it on `--remove` is the destructive edit every other path in
    /// this module refuses to make.
    #[test]
    fn remove_keeps_an_edited_owned_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join(".windsurf/rules/travsr.md");
        let planned = Planned {
            path: p.clone(),
            content: Content::Owned {
                text: markdown_rules(),
            },
            gitignore: false,
        };
        execute(&planned, false, false).unwrap();
        assert!(p.exists());

        // Untouched: removal owns it and takes it.
        assert!(matches!(
            execute(&planned, true, false).unwrap(),
            Outcome::Removed
        ));
        assert!(!p.exists());

        // Edited: removal must decline and say why.
        execute(&planned, false, false).unwrap();
        std::fs::write(&p, format!("{}\n\nmy own note\n", markdown_rules())).unwrap();
        assert!(matches!(
            execute(&planned, true, false).unwrap(),
            Outcome::Skipped(_)
        ));
        assert!(p.exists(), "an edited rules file must survive --remove");
    }

    /// Replacing a file must not widen its permissions: the temp file is created
    /// under the process umask, so a `0600` CLAUDE.md would come back `0644`.
    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().unwrap();
        let p = dir.path().join("CLAUDE.md");
        write_atomic(&p, "v1").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&p, "v2").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "write_atomic widened the file mode");
    }

    /// `--tool cursor --remove` unwires Cursor only. Claude Code's `.mcp.json` is
    /// still on disk with a travsr server in it, so its ignore entry must stay.
    #[test]
    fn partial_remove_keeps_other_tools_ignored() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        let gi = repo.join(".gitignore");
        std::fs::write(&gi, "target/\n").unwrap();

        ensure_gitignored(repo, &[".mcp.json".into(), ".cursor/mcp.json".into()], &[]).unwrap();
        ensure_gitignored(repo, &[], &[".cursor/mcp.json".into()]).unwrap();

        let entries = block_entries(&gi, GI_BEGIN, GI_END).unwrap();
        assert_eq!(entries, vec!["/.mcp.json"]);
        // The user's own rules are untouched throughout.
        assert!(std::fs::read_to_string(&gi).unwrap().contains("target/"));

        // Removing the last entry takes the whole block with it.
        ensure_gitignored(repo, &[], &[".mcp.json".into()]).unwrap();
        let text = std::fs::read_to_string(&gi).unwrap();
        assert!(!text.contains(GI_BEGIN), "empty block should be removed");
        assert!(text.contains("target/"));
    }

    #[test]
    fn every_tool_writes_a_rules_file() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        for tool in Tool::ALL {
            let plan = tool.plan(repo, &cmd());
            let has_rules = plan
                .iter()
                .any(|p| matches!(p.content, Content::ManagedMd { .. } | Content::Owned { .. }));
            assert!(
                has_rules,
                "{} wires MCP but never tells the agent to use it",
                tool.id()
            );
        }
    }

    #[test]
    fn rules_only_tools_explain_the_missing_mcp_step() {
        // Derived from `plan()`, not hardcoded: a rules-only adapter keeps its
        // MCP servers in a global file we do not write, so without a note the
        // user gets rules and no server with no clue why. Listing the tools by
        // hand meant Antigravity landed as the third such adapter without ever
        // being covered, so the invariant is stated over Tool::ALL instead.
        let dir = tempdir().unwrap();
        let repo = dir.path();
        let mut rules_only = Vec::new();
        for tool in Tool::ALL {
            let writes_server = tool
                .plan(repo, &cmd())
                .iter()
                .any(|p| matches!(p.content, Content::JsonServer { .. }));
            if writes_server {
                assert!(
                    tool.note(&cmd()).is_none(),
                    "{} writes a server, so it should not claim to be rules-only",
                    tool.id()
                );
            } else {
                rules_only.push(tool.id());
                assert!(
                    tool.note(&cmd()).is_some(),
                    "{} writes no server and needs a manual-MCP note",
                    tool.id()
                );
            }
        }
        assert!(
            rules_only.len() >= 3,
            "expected Antigravity, Codex and Windsurf to be rules-only, got {rules_only:?}"
        );
    }

    /// #829: writing `.mcp.json` reports `ok`, but Claude Code will not load a
    /// project server until it is approved once. The report must name that step,
    /// or the wiring looks done while it is inert.
    #[test]
    fn claude_code_names_the_one_time_approval_step() {
        let hint = Tool::ClaudeCode
            .approval_hint()
            .expect("claude-code writes a project .mcp.json and needs an approval hint");
        assert!(
            hint.contains("approval") && hint.contains("/mcp"),
            "hint should name the approval and how to grant it: {hint}"
        );
        // The hint is Claude Code specific; a rules-only tool must not carry it.
        assert!(Tool::Codex.approval_hint().is_none());
    }

    #[test]
    fn detection_uses_markers_that_identify_one_tool() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        assert!(matches!(
            Tool::GeminiCli.detect(repo, None),
            Detection::None
        ));
        assert!(matches!(Tool::Codex.detect(repo, None), Detection::None));
        assert!(matches!(
            Tool::Antigravity.detect(repo, None),
            Detection::None
        ));

        // AGENTS.md is a cross-tool convention, not a Codex marker: Zed reads it
        // too (it is in ZED_INSTRUCTION_FILES), so on its own it identifies no
        // one. Treating it as evidence reported `codex (configured)` in any repo
        // that adopted the convention and told the user to edit
        // ~/.codex/config.toml for a tool they do not have. Same shared-file
        // error as GEMINI.md below.
        std::fs::write(repo.join("AGENTS.md"), "").unwrap();
        assert!(matches!(Tool::Codex.detect(repo, None), Detection::None));

        // `.codex/` identifies exactly one tool, so that is the marker.
        std::fs::create_dir_all(repo.join(".codex")).unwrap();
        assert!(matches!(Tool::Codex.detect(repo, None), Detection::Auto));

        // GEMINI.md is read by BOTH Gemini CLI and Antigravity, so on its own it
        // identifies neither. Gemini CLI needs its settings.json; Antigravity
        // needs its own project dir.
        std::fs::write(repo.join("GEMINI.md"), "").unwrap();
        assert!(matches!(
            Tool::GeminiCli.detect(repo, None),
            Detection::None
        ));
        assert!(matches!(
            Tool::Antigravity.detect(repo, None),
            Detection::None
        ));

        std::fs::create_dir_all(repo.join(".gemini")).unwrap();
        std::fs::write(repo.join(".gemini/settings.json"), "{}").unwrap();
        assert!(matches!(
            Tool::GeminiCli.detect(repo, None),
            Detection::Auto
        ));

        std::fs::create_dir_all(repo.join(".antigravitycli")).unwrap();
        assert!(matches!(
            Tool::Antigravity.detect(repo, None),
            Detection::Auto
        ));
    }

    #[test]
    fn guide_names_only_tools_the_mcp_server_exposes() {
        // A real pin, not a mirror: the names are read out of the generated
        // guidance and checked against the payload `travsr mcp --stdio` actually
        // serves. A rename on the server side fails here, which a hardcoded list
        // could never catch, and a rule telling the agent to call a tool that
        // does not exist is worse than no rule.
        let served: Vec<String> = travsr_mcp::stdio_tools_list()["tools"]
            .as_array()
            .expect("tools/list payload has a `tools` array")
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default().to_string())
            .collect();

        let named = guide_table_tools(&guide_body());
        // The guidance names only the few tools whose *use* needs explaining
        // beyond their own description, so this is a floor against the parser
        // silently matching nothing, not a target to grow.
        assert!(
            named.len() >= 2,
            "extraction recovered too few tools, it has probably broken: {named:?}"
        );
        for name in &named {
            assert!(
                served.contains(name),
                "guidance routes to `{name}`, which `travsr mcp --stdio` does not serve"
            );
        }
    }

    /// `docs/ai-tool-prompt.md` tells an agent that is unsure which mode it is
    /// talking to how to find out, and names `get_callers` as the discriminator.
    /// The first attempt at that advice said "`repo` is present in exactly one of
    /// them", which is false: `get_snippets` declares `repo` in the single-repo
    /// schema too, scoped to global mode by its description only. An agent
    /// applying the rule to that tool would conclude it was in global mode and
    /// start passing `repo` everywhere, which is the bug the guidance exists to
    /// prevent. Pin the discriminator that actually holds.
    #[test]
    fn get_callers_discriminates_the_two_server_modes() {
        let schema = |list: serde_json::Value, tool: &str| -> Vec<String> {
            list["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == tool)
                .unwrap_or_else(|| panic!("{tool} is not served"))["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };

        let stdio = schema(travsr_mcp::stdio_tools_list(), "get_callers");
        let global = schema(travsr_mcp::global_tools_list(), "get_callers");
        assert!(
            !stdio.contains(&"repo".to_string()),
            "get_callers gained `repo` in single-repo mode, the documented \
             discriminator no longer works: {stdio:?}"
        );
        assert!(
            global.contains(&"repo".to_string()),
            "get_callers lost `repo` in global mode: {global:?}"
        );

        // And the reason the naive "present in exactly one" rule fails.
        assert!(
            schema(travsr_mcp::stdio_tools_list(), "get_snippets").contains(&"repo".to_string()),
            "get_snippets no longer declares `repo` in single-repo mode, so the \
             warning against using it as a discriminator can be dropped"
        );
    }

    /// Tool names the guidance tells the agent to call.
    ///
    /// Read out of the generated text rather than kept as a list here, which is
    /// what makes the pin above real: a rename on the server side fails the
    /// test, where a hardcoded copy would happily agree with itself.
    ///
    /// Scans the whole body. It used to parse the second column of a routing
    /// table, and when that table was removed for costing tokens on every turn
    /// the parser silently recovered nothing, so the check passed by finding no
    /// tools to check. Anchored to the naming convention instead.
    fn guide_table_tools(body: &str) -> Vec<String> {
        let mut names = Vec::new();
        for word in body.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            let looks_like_a_tool = word.starts_with("get_")
                || word.starts_with("find_")
                || word.starts_with("search_")
                || word.starts_with("repos_");
            if looks_like_a_tool && !names.contains(&word.to_string()) {
                names.push(word.to_string());
            }
        }
        names
    }

    /// The point of the whole change: a default `connect` must leave nothing
    /// behind that an agent re-reads every turn.
    ///
    /// Exercised through `run()` rather than `plan()`. Every other test in this
    /// file calls `plan()`, which still contains the rules entry, so all of them
    /// kept passing when the filter was added and none of them would have caught
    /// it being dropped.
    #[test]
    fn a_default_connect_writes_no_always_on_guidance() {
        let d = tempfile::tempdir().expect("tempdir");
        let repo = d.path();
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        std::fs::create_dir_all(repo.join(".claude")).expect("claude dir");

        let opts = ConnectOpts {
            only: Some("claude-code".to_string()),
            dry_run: false,
            remove: false,
            commit: false,
            rules: false,
            report: Report::Silent,
        };
        run(repo, &opts).expect("connect");

        assert!(
            repo.join(".mcp.json").is_file(),
            "the MCP wiring is what makes the tools reachable and must still land"
        );
        assert!(
            !repo.join("CLAUDE.md").exists(),
            "a default connect wrote CLAUDE.md, which is re-read on every turn"
        );
    }

    /// #746 review: an existing guidance block must be refreshed, not frozen.
    ///
    /// Filtering guidance out unconditionally meant `upsert_block` never ran, so
    /// whoever ran `connect` before this change kept the old body forever and
    /// the token saving reached nobody who had already connected.
    #[test]
    fn an_existing_guidance_block_is_refreshed_without_rules() {
        let d = tempfile::tempdir().expect("tempdir");
        let repo = d.path();
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        std::fs::create_dir_all(repo.join(".claude")).expect("claude dir");

        let opts = |rules: bool| ConnectOpts {
            only: Some("claude-code".to_string()),
            dry_run: false,
            remove: false,
            commit: false,
            rules,
            report: Report::Silent,
        };

        // An earlier run wrote guidance, with a body that is now out of date.
        run(repo, &opts(true)).expect("connect --rules");
        let path = repo.join("CLAUDE.md");
        let stale = std::fs::read_to_string(&path)
            .expect("CLAUDE.md")
            .replace(GUIDE_TITLE, "Some older heading we no longer ship");
        std::fs::write(&path, &stale).expect("write stale body");

        // A default run must bring it back up to date.
        run(repo, &opts(false)).expect("connect");
        let after = std::fs::read_to_string(&path).expect("CLAUDE.md");
        assert!(
            after.contains(GUIDE_TITLE),
            "an existing block must be refreshed by a default run: {after}"
        );
    }

    /// The other half: a CLAUDE.md that exists for its own reasons must not
    /// acquire guidance just by being there, or the opt-in is an opt-out.
    #[test]
    fn a_users_own_markdown_does_not_acquire_guidance() {
        let d = tempfile::tempdir().expect("tempdir");
        let repo = d.path();
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        std::fs::create_dir_all(repo.join(".claude")).expect("claude dir");
        std::fs::write(repo.join("CLAUDE.md"), "# My own notes\n").expect("write");

        run(
            repo,
            &ConnectOpts {
                only: Some("claude-code".to_string()),
                dry_run: false,
                remove: false,
                commit: false,
                rules: false,
                report: Report::Silent,
            },
        )
        .expect("connect");

        let after = std::fs::read_to_string(repo.join("CLAUDE.md")).expect("CLAUDE.md");
        assert!(
            !after.contains(MD_BEGIN),
            "a file we never wrote to must not gain guidance by default: {after}"
        );
    }

    /// And the opt-in still works, or the flag is a lie.
    #[test]
    fn rules_writes_the_guidance_when_asked() {
        let d = tempfile::tempdir().expect("tempdir");
        let repo = d.path();
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        std::fs::create_dir_all(repo.join(".claude")).expect("claude dir");

        let opts = ConnectOpts {
            only: Some("claude-code".to_string()),
            dry_run: false,
            remove: false,
            commit: false,
            rules: true,
            report: Report::Silent,
        };
        run(repo, &opts).expect("connect");

        let md = std::fs::read_to_string(repo.join("CLAUDE.md")).expect("CLAUDE.md");
        assert!(
            md.contains(GUIDE_TITLE),
            "--rules did not write the guidance: {md}"
        );
    }

    /// `--remove` must not be filtered, or a rules file written by an earlier
    /// `--rules` run is stranded: the flag that created it is not the flag that
    /// deletes it, and the user has no reason to think one is needed.
    #[test]
    fn remove_cleans_up_guidance_written_earlier() {
        let d = tempfile::tempdir().expect("tempdir");
        let repo = d.path();
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        std::fs::create_dir_all(repo.join(".claude")).expect("claude dir");

        let with_rules = |rules: bool, remove: bool| ConnectOpts {
            only: Some("claude-code".to_string()),
            dry_run: false,
            remove,
            commit: false,
            rules,
            report: Report::Silent,
        };
        run(repo, &with_rules(true, false)).expect("connect --rules");
        assert!(repo.join("CLAUDE.md").is_file(), "setup did not write it");

        run(repo, &with_rules(false, true)).expect("connect --remove");
        let left = std::fs::read_to_string(repo.join("CLAUDE.md")).unwrap_or_default();
        assert!(
            !left.contains(GUIDE_TITLE),
            "--remove left the guidance behind: {left}"
        );
    }

    /// The guidance is loaded on every turn of every conversation, so its size
    /// is a recurring cost paid by every user of every agent, not a one-off.
    /// Nothing else in the codebase makes that cost visible, which is how it
    /// reached 2270 characters: each addition looked small on its own.
    ///
    /// The budget is the point of this test. Adding a line means removing one,
    /// or making a deliberate case for raising the limit.
    #[test]
    fn the_always_on_guidance_stays_small() {
        let n = guide_body().chars().count();
        assert!(
            n <= 1200,
            "agent guidance is {n} chars, over the 1200 budget. It is re-sent on \
             every turn, so this is paid repeatedly. Detail that a reader wants \
             once belongs in the FAQ, reachable with `travsr ask \"travsr: ...\"`"
        );
    }

    /// The pointer has to be a command that works, or it is worse than nothing:
    /// an agent that follows it gets an error and learns to distrust the rest.
    #[test]
    fn the_guidance_points_at_a_working_command() {
        let body = guide_body();
        let marker = "travsr ask \"travsr:";
        assert!(
            body.contains(marker),
            "guidance lost its pointer to the FAQ"
        );
        assert!(
            crate::faq::strip_namespace("travsr: how does MCP work").is_some(),
            "the namespace the guidance tells agents to use is not recognised"
        );
        assert!(
            crate::faq::match_namespaced("how does MCP work").is_some(),
            "the example question the pointer implies matches nothing"
        );
    }

    /// `plan()` wires `travsr mcp --stdio` with no `--global`, which is
    /// single-repo mode. There, exactly one tool (`get_snippets`) even declares
    /// `repo`, and its own description scopes it to "global / multi-repo mode
    /// only"; every other schema is closed, so a `repo` argument is not part of
    /// the contract. Guidance that tells the agent to pass one on every call
    /// puts a rejected argument on every question it asks.
    #[test]
    fn guide_does_not_promise_a_repo_argument() {
        let body = guide_body();
        for bad in ["(repo", ", repo)", ", repo,"] {
            assert!(
                !body.contains(bad),
                "guidance passes `repo`, which `travsr mcp --stdio` does not take: {bad}"
            );
        }
        // repos_list is a global-registry tool; in a repo-scoped session it
        // answers a question the agent never needs to ask.
        assert!(!body.contains("repos_list"));
    }

    /// The command written into every tool config must stay single-repo. Adding
    /// `--global` would silently widen a project-scoped config into one that
    /// serves every repo on the machine, and would invalidate the guidance above.
    #[test]
    fn wired_command_is_single_repo_stdio() {
        assert_eq!(
            McpCommand::resolve().args,
            vec!["mcp".to_string(), "--stdio".to_string()]
        );
    }

    #[test]
    fn cursor_mdc_has_always_apply_frontmatter() {
        assert!(cursor_mdc().starts_with("---\n"));
        assert!(cursor_mdc().contains("alwaysApply: true"));
    }
}
