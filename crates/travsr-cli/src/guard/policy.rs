//! What the guard decides, and why (#916).
//!
//! Split from `mod.rs` so the decision is a pure-ish function of
//! (payload, mode, repo) and can be exercised without a subprocess, a stdin
//! pipe, or a deadline. `mod.rs` owns the process-shaped concerns (reading
//! stdin, the 200 ms deadline, the panic hook), and this owns the judgement.
//!
//! The rule the whole module is built around: **a redirect is only honest when
//! the graph can actually answer the question.** Every branch that cannot
//! establish that answers `allow`, because a guard that blocks a read the graph
//! cannot replace is an outage with a helpful error message.

use std::path::{Path, PathBuf};

use travsr_store::SqliteStore;

use super::payload::{HookInput, HookOutput};
use super::shell::{self, SearchTool};
use super::GuardMode;

/// What the agent is about to do, once the guard has recognised it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// A content search (`Grep`, or `Bash` grep/rg/ag/ack). `term` is the
    /// literal pattern when one could be recovered.
    Content { term: Option<String>, via: String },
    /// File discovery (`Glob`, or `Bash` find / `ls -R`).
    Files { hint: Option<String>, via: String },
    /// A whole-file `Read`.
    Read { path: String },
    /// A Travsr MCP call going past. Never decided on, only recorded.
    TravsrQuery { term: Option<String> },
}

/// Recognise a hook payload, or `None` when it is outside the match set.
///
/// `None` is the common case and it is the safe one: an unmatched call is
/// passed through with no decision at all, so the host's own permission
/// settings still apply to it.
pub fn classify(input: &HookInput) -> Option<Request> {
    // The hook is only ever installed for PreToolUse. A payload claiming a
    // different event means the settings file was hand-edited into a shape the
    // guard was not written for, and it has no business deciding there.
    if input
        .hook_event_name
        .as_deref()
        .is_some_and(|e| e != "PreToolUse")
    {
        return None;
    }
    let tool = input.tool_name.as_deref()?;
    let args = input.tool_input.as_ref();

    if let Some(term) = travsr_tool_term(tool, input) {
        return Some(Request::TravsrQuery { term });
    }

    match tool {
        "Grep" => Some(Request::Content {
            term: args
                .and_then(|a| a.pattern.clone())
                .filter(|p| !p.is_empty()),
            via: "Grep".into(),
        }),
        "Glob" => Some(Request::Files {
            hint: args.and_then(|a| a.pattern.clone().or_else(|| a.glob.clone())),
            via: "Glob".into(),
        }),
        "Read" => {
            let path = args
                .and_then(|a| a.file_path.clone())
                .filter(|p| !p.is_empty())?;
            // A ranged read is a follow-up, not a fishing expedition: the agent
            // already knows which lines it wants, very often because Travsr
            // just told it. Redirecting that would undo the graph's own work.
            if args.is_some_and(|a| a.offset.is_some() || a.limit.is_some()) {
                return None;
            }
            Some(Request::Read { path })
        }
        "Bash" => {
            let command = args.and_then(|a| a.command.as_deref())?;
            let found = shell::classify(command)?;
            match found.kind {
                SearchTool::Content => Some(Request::Content {
                    term: found.term,
                    via: found.program,
                }),
                SearchTool::Files => Some(Request::Files {
                    hint: found.term,
                    via: found.program,
                }),
            }
        }
        _ => None,
    }
}

/// The search term of a Travsr MCP call, when `tool` is one.
///
/// Matched on the trailing segment rather than a fixed prefix, because an MCP
/// tool name is `mcp__<server>__<tool>` and the server is named by whoever
/// wrote the client's config: `travsr` from `travsr connect`, but not
/// necessarily from a hand-written one.
fn travsr_tool_term(tool: &str, input: &HookInput) -> Option<Option<String>> {
    const GRAPH_TOOLS: [&str; 7] = [
        "get_callers",
        "find_references",
        "get_context",
        "search_symbol",
        "get_graph_json",
        "get_dependencies",
        "get_blast_radius",
    ];
    let leaf = tool.rsplit("__").next().unwrap_or(tool);
    if !tool.starts_with("mcp__") || !GRAPH_TOOLS.contains(&leaf) {
        return None;
    }
    let args = input.tool_input.as_ref();
    Some(
        args.and_then(|a| {
            a.symbol
                .clone()
                .or_else(|| a.query.clone())
                .or_else(|| a.name.clone())
                .or_else(|| a.file.clone())
        })
        .filter(|t| !t.is_empty()),
    )
}

/// Everything the guard learned about the repository, or the reason it could
/// not. Built once per invocation.
struct Index {
    root: PathBuf,
    store: SqliteStore,
}

/// Open the repo's graph for reading, or say why it could not be: no
/// `.travsr/graph.db`, a database that will not open (locked or corrupt),
/// an empty index, or an index that does not describe the
/// current `HEAD`. Every `Err` here is a fail-open condition.
///
/// The read-only open is tried first and a writable one is the fallback, the
/// same order `daemon_client::open_read_store` uses and for the same reason:
/// SQLite cannot open a WAL database read-only unless the `-shm` file already
/// exists, and after the last writer closes it does not. The usual state of an
/// idle repo (indexed once, daemon since exited) is therefore one where the
/// read-only open fails outright. Preferring it is still right, because it is
/// the open that cannot migrate or checkpoint the user's index as a side effect
/// of deciding whether to allow a `grep`; the fallback only widens that to what
/// every other read path in the CLI already does.
fn open_index(repo_root: &Path) -> Result<Index, String> {
    let db = repo_root.join(".travsr").join("graph.db");
    if !db.exists() {
        return Err(format!("no index at {}", db.display()));
    }
    let store = match SqliteStore::open_read_only(&db) {
        Ok(s) => s,
        Err(read_only) => SqliteStore::open(&db)
            .map_err(|e| format!("index will not open (read-only: {read_only}; writable: {e})"))?,
    };
    // Phase A has not landed: there is nothing to redirect to yet.
    if store.node_count().unwrap_or(0) == 0 {
        return Err("index is empty; Phase A has not landed".into());
    }
    let stored = store
        .get_meta("last_commit")
        .ok()
        .flatten()
        .unwrap_or_default();
    let head = head_sha(repo_root).unwrap_or_default();
    // Both must be known *and* agree. An unknown HEAD (no git, an unborn
    // branch, a ref this reader cannot resolve) is "staleness cannot be
    // determined safely", which is a fail-open condition in its own right, so
    // the empty cases below have to allow rather than pass.
    if stored.is_empty() {
        return Err("index records no commit".into());
    }
    if head.is_empty() {
        return Err("HEAD could not be resolved, so staleness is unknown".into());
    }
    if travsr_mcp::head_index_mismatch_note(&head, &stored).is_some() {
        return Err(format!("index is at {stored}, HEAD is at {head}"));
    }
    Ok(Index {
        root: repo_root.to_path_buf(),
        store,
    })
}

/// The commit `HEAD` points at, read from `.git` directly.
///
/// No subprocess. `git rev-parse` is normally a few milliseconds, but the guard
/// runs on every matched tool call inside a 200 ms budget, and spawning a
/// process is the one operation on this path whose worst case is unbounded
/// (a credential helper, a filesystem hiccup, an antivirus scanning the
/// executable). Reading two small files has no such tail.
///
/// Returns the full 40-character sha; the stored `last_commit` is a `--short`
/// abbreviation, and [`travsr_mcp::head_index_mismatch_note`] compares
/// abbreviations as prefixes, so the widths do not have to match.
fn head_sha(repo_root: &Path) -> Option<String> {
    let dot_git = repo_root.join(".git");
    // A linked worktree's `.git` is a file holding `gitdir: <path>`.
    let git_dir = if dot_git.is_file() {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let raw = text.strip_prefix("gitdir:")?.trim();
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            repo_root.join(p)
        }
    } else if dot_git.is_dir() {
        dot_git
    } else {
        return None;
    };

    // Shared refs live in the main checkout's git dir; a worktree names it in
    // `commondir`. Without this a linked worktree resolves no branch ref at all.
    let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|c| {
            let p = PathBuf::from(c.trim());
            if p.is_absolute() {
                p
            } else {
                git_dir.join(p)
            }
        })
        .unwrap_or_else(|| git_dir.clone());

    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if is_hex_sha(head) {
        return Some(head.to_string());
    }
    let refname = head.strip_prefix("ref:")?.trim();

    // A loose ref, in this worktree's git dir first (HEAD, ORIG_HEAD and the
    // per-worktree refs live there), then in the shared one.
    for base in [&git_dir, &common_dir] {
        // `refname` is a slash-separated git ref, which is a relative path in
        // both git dirs on every platform.
        if let Ok(text) = std::fs::read_to_string(base.join(refname)) {
            let sha = text.trim();
            if is_hex_sha(sha) {
                return Some(sha.to_string());
            }
        }
    }
    // Packed refs: `<sha> <refname>` per line, with `#` comments and `^`
    // peeled-tag lines to skip.
    let packed = std::fs::read_to_string(common_dir.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        let (sha, name) = line.split_once(' ')?;
        (name.trim() == refname && is_hex_sha(sha)).then(|| sha.to_string())
    })
}

fn is_hex_sha(s: &str) -> bool {
    s.len() >= 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// A symbol name the graph could plausibly hold: a bare identifier, long enough
/// that a lookup means something, with no regex metacharacter in it.
///
/// A regex is not a symbol. `grep -rn 'fn .*charge'` is a text search whose
/// intent the graph cannot reconstruct, and looking the whole pattern up would
/// simply miss, which reads the same as "the graph does not know this symbol"
/// while meaning something quite different.
fn as_symbol(term: &str) -> Option<&str> {
    let t = term.trim();
    // Three characters: below that a name is too generic for an exact lookup to
    // say anything, and `grep -rn id .` is not a structural question.
    if t.len() < 3 || t.len() > 128 {
        return None;
    }
    let mut chars = t.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    // Dotted and `::`-qualified names are identifiers too: `PaymentService.charge`
    // is exactly the shape `get_callers` wants.
    t.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == ':')
        .then_some(t)
}

/// File extensions the graph never carries, so a `Read` of one is never
/// graph-answerable. Checked before the database is opened: the cheapest way to
/// answer a question is not to ask it.
fn excluded_extension(path: &str) -> bool {
    const DOCS: [&str; 6] = ["md", "mdx", "rst", "txt", "adoc", "org"];
    const DATA: [&str; 12] = [
        "json",
        "toml",
        "yaml",
        "yml",
        "ini",
        "cfg",
        "conf",
        "xml",
        "properties",
        "env",
        "csv",
        "tsv",
    ];
    const BINARY: [&str; 22] = [
        "png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "zip", "gz", "tar", "bz2", "xz", "exe",
        "dll", "so", "dylib", "bin", "wasm", "class", "jar", "db", "sqlite",
    ];
    let lower = path.to_ascii_lowercase();
    // Lock files are named, not extended: `Cargo.lock` has an extension,
    // `go.sum` and `yarn.lock` are a mixed bag, so match the whole file name.
    const LOCKS: [&str; 9] = [
        "cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "poetry.lock",
        "gemfile.lock",
        "composer.lock",
        "go.sum",
        "flake.lock",
    ];
    let name = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
    if LOCKS.contains(&name) || name.ends_with(".lock") {
        return true;
    }
    let Some(ext) = name.rsplit_once('.').map(|(_, e)| e) else {
        // No extension at all: LICENSE, Makefile, Dockerfile. Not code the
        // graph indexes by path, so not something to redirect.
        return true;
    };
    DOCS.contains(&ext) || DATA.contains(&ext) || BINARY.contains(&ext)
}

/// Whether a repo-relative path lies under a directory the index never walks.
fn vendored(rel: &str) -> bool {
    const EXTRA: [&str; 6] = [
        "vendor",
        "third_party",
        "thirdparty",
        ".venv",
        "site-packages",
        "bower_components",
    ];
    rel.split(['/', '\\'])
        .any(|c| travsr_mcp::SKIP_DIRS.contains(&c) || EXTRA.contains(&c))
}

/// Resolve a tool's `file_path` to the repo-relative, forward-slash form the
/// index keys files by. `None` when the path escapes the repository.
fn repo_relative(repo_root: &Path, cwd: Option<&str>, raw: &str) -> Option<String> {
    let given = PathBuf::from(raw);
    let absolute = if given.is_absolute() {
        given
    } else {
        // Relative to the agent's cwd, which is not necessarily the repo root.
        let base = cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| repo_root.to_path_buf());
        base.join(given)
    };
    // `canonicalize` would touch the filesystem (and fail outright on a path
    // that does not exist yet). Lexical normalisation is enough here: the
    // result is a lookup key, never something opened.
    //
    // Both sides go through it, because `strip_prefix` compares components and
    // a root carrying a `.` (which is what a `cwd` of "." resolves to) would
    // otherwise fail to match a file path that has had its own `.` folded away.
    let rel = lexical(&absolute);
    let rel = rel.strip_prefix(lexical(repo_root)).ok()?;
    let slashed = rel.to_string_lossy().replace('\\', "/");
    (!slashed.is_empty()).then_some(slashed)
}

/// Fold `.` and `..` away without touching the filesystem.
///
/// Rebuilt by pushing components rather than joining strings: on Windows an
/// absolute path's leading components are a `Prefix` (`C:`) and a `RootDir`
/// (`\`), and joining those with a separator produces `C:\\\...`, which is not
/// the path anyone meant.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether the index holds this file. Tries the forward-slash form the indexer
/// writes and the platform-native form, so a Windows index built by an older
/// build is not read as "not indexed".
fn file_indexed(store: &SqliteStore, rel: &str) -> bool {
    if store.get_file_hash(rel).ok().flatten().is_some() {
        return true;
    }
    let native = rel.replace('/', std::path::MAIN_SEPARATOR_STR);
    native != rel && store.get_file_hash(&native).ok().flatten().is_some()
}

/// The redirect the guard offers: the exact call, and the sentence around it.
struct Redirect {
    /// The term the strict-mode release valve is keyed on.
    key: String,
    text: String,
}

/// What the graph can do for this request, if anything.
fn redirect_for(index: &Index, cwd: Option<&str>, request: &Request) -> Option<Redirect> {
    match request {
        Request::Content { term, via } => {
            let symbol = as_symbol(term.as_deref()?)?;
            // The lookup that decides the whole thing: an exact-name hit means
            // the graph holds this symbol and can enumerate its use sites
            // better than a text search can. A miss means it cannot, and the
            // text search is the correct tool.
            let known = index
                .store
                .search_nodes_by_name_exact(symbol)
                .map(|n| !n.is_empty())
                .unwrap_or(false);
            if !known {
                return None;
            }
            Some(Redirect {
                key: symbol.to_string(),
                text: format!(
                    "Travsr has indexed this repository and holds `{symbol}`. It can answer \
                     this structurally: call find_references(symbol=\"{symbol}\") for every \
                     use site, or get_callers(symbol=\"{symbol}\") for the callers, instead \
                     of {via}. If the graph comes back empty, run this search again and it \
                     will be allowed."
                ),
            })
        }
        Request::Read { path } => {
            if excluded_extension(path) {
                return None;
            }
            let rel = repo_relative(&index.root, cwd, path)?;
            if vendored(&rel) || !file_indexed(&index.store, &rel) {
                return None;
            }
            let stem = rel
                .rsplit('/')
                .next()
                .and_then(|n| n.rsplit_once('.').map(|(s, _)| s))
                .unwrap_or(&rel)
                .to_string();
            Some(Redirect {
                key: rel.clone(),
                text: format!(
                    "Travsr has `{rel}` in the graph. Call \
                     get_context(query=\"{stem}\", include_snippets=true) for the symbols in \
                     it with their source inline, or get_dependencies(file=\"{rel}\") for \
                     what it imports, instead of reading the whole file. Read it directly \
                     when you need something the graph does not carry; run this again and \
                     it will be allowed."
                ),
            })
        }
        // File discovery is deliberately never graph-answerable. The graph
        // indexes code files only, so it cannot enumerate the untracked,
        // ignored, generated and non-code files a glob legitimately finds, and
        // answering "here is the subset I know about" to a question about the
        // whole tree would be wrong in a way the agent could not detect.
        // Advisory mode still nudges these (see `advisory_note`); strict mode
        // leaves them alone.
        Request::Files { .. } => None,
        Request::TravsrQuery { .. } => None,
    }
}

/// The advisory nudge for a matched call the graph could not answer exactly.
///
/// Short on purpose: it rides into the model's context on every matched tool
/// call, so it is one sentence naming one call, not a restatement of the rules
/// file. Returns `None` when there is nothing true to say.
fn advisory_note(request: &Request) -> Option<String> {
    match request {
        Request::Content { term, via } => Some(match term {
            Some(t) => format!(
                "Travsr indexes this repository: find_pattern(pattern=\"{t}\") runs the same \
                 search already scoped to the indexed files, and `scope` narrows it further. \
                 Continuing with {via}."
            ),
            None => format!(
                "Travsr indexes this repository: find_pattern is the same search scoped to \
                 the indexed files. Continuing with {via}."
            ),
        }),
        Request::Files { hint, via } => Some(match hint {
            Some(h) => format!(
                "Travsr indexes this repository: get_repo_map() lists the indexed files and \
                 get_context(query=\"{h}\") finds where something lives without walking the \
                 tree. Continuing with {via}."
            ),
            None => format!(
                "Travsr indexes this repository: get_repo_map() lists the indexed files \
                 without walking the tree. Continuing with {via}."
            ),
        }),
        // Nothing true to say. This arm is only reached once `redirect_for`
        // has established the graph does not carry the file: it is a doc, a
        // lockfile, a config, vendored, untracked, or outside the repository
        // altogether. Nudging toward `get_context` there would send the agent
        // at a call that cannot answer, which is worse than staying quiet. An
        // indexed file takes the `Some` arm and gets the real redirect.
        Request::Read { .. } => None,
        Request::TravsrQuery { .. } => None,
    }
}

/// A decision and the one-line account of how it was reached.
///
/// The account exists because "the guard is installed and nothing is being
/// blocked" has a dozen causes that all look identical from outside: the mode
/// is off, the index is stale, the symbol is unknown, the command was not
/// recognised. `travsr guard --explain` prints it on stderr, never stdout,
/// which belongs to the decision, so the question is answerable without
/// reading this file.
#[derive(Debug)]
pub struct Decision {
    pub output: HookOutput,
    pub why: String,
}

impl Decision {
    fn new(output: HookOutput, why: impl Into<String>) -> Self {
        Self {
            output,
            why: why.into(),
        }
    }

    /// The three outcomes `guard::run` reaches without ever calling [`decide`].
    pub fn unreadable_payload() -> Self {
        Self::new(HookOutput::Neutral, "the payload could not be read")
    }

    pub fn timed_out() -> Self {
        Self::new(HookOutput::Neutral, "the decision missed its deadline")
    }

    pub fn internal_error() -> Self {
        Self::new(HookOutput::Neutral, "the guard errored internally")
    }

    /// Whether this decision blocks the tool call. The property every
    /// fail-open test asserts the negation of.
    #[cfg(test)]
    fn blocks(&self) -> bool {
        self.output.blocks()
    }
}

/// The guard's decision for one payload.
///
/// `repo_root` is `None` when the payload's `cwd` is not inside a git
/// repository, which is itself a fail-open condition.
pub fn decide(input: &HookInput, mode: GuardMode, repo_root: Option<&Path>) -> Decision {
    let pass = |why: &str| Decision::new(HookOutput::Neutral, why);

    if mode == GuardMode::Off {
        return pass("guard.mode is off");
    }
    let Some(request) = classify(input) else {
        return pass("not a tool call the guard inspects");
    };
    let Some(root) = repo_root else {
        return pass("not inside a git repository");
    };

    // A Travsr call going past is never decided on. It is recorded, so a later
    // search for the same term is released.
    if let Request::TravsrQuery { term } = &request {
        if let (Some(term), Some(session)) = (term, input.session_id.as_deref()) {
            super::session::record_query(root, session, term);
            return pass(&format!("observed a travsr query for `{term}`"));
        }
        return pass("observed a travsr query");
    }

    // Every fail-open condition collapses here: no index, an index that will
    // not open, an empty one, or one that does not describe the checkout.
    let index = match open_index(root) {
        Ok(i) => i,
        Err(why) => return pass(&why),
    };

    match (mode, redirect_for(&index, input.cwd.as_deref(), &request)) {
        // Strict, and the graph genuinely holds the answer. One redirect per
        // (session, term): the valve releases every repeat, so this can never
        // become a loop the agent cannot leave.
        (GuardMode::Strict, Some(r)) => {
            if super::session::release(root, input.session_id.as_deref(), &r.key) {
                // Released: the agent keeps the redirect but the call is not
                // decided on, so the user's permission settings still apply.
                Decision::new(
                    HookOutput::context(r.text),
                    format!("`{}` already redirected in this session", r.key),
                )
            } else {
                Decision::new(
                    HookOutput::deny(r.text),
                    format!("the graph answers `{}`", r.key),
                )
            }
        }
        // Advisory names the same call and decides nothing.
        (GuardMode::Advisory, Some(r)) => Decision::new(
            HookOutput::context(r.text),
            format!("the graph answers `{}`, advisory does not block", r.key),
        ),
        // The graph cannot answer this one. Strict must not block it; advisory
        // still has something worth saying, because the index exists and is
        // current; `open_index` already established both.
        (GuardMode::Strict, None) => pass("the graph cannot answer this"),
        (GuardMode::Advisory, None) => match advisory_note(&request) {
            Some(note) => Decision::new(
                HookOutput::context(note),
                "the graph cannot answer this exactly; nudged",
            ),
            None => pass("the graph cannot answer this"),
        },
        (GuardMode::Off, _) => pass("guard.mode is off"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(json: &str) -> HookInput {
        serde_json::from_str(json).expect("fixture must parse")
    }

    #[test]
    fn the_direct_tools_are_recognised() {
        assert!(matches!(
            classify(&input(
                r#"{"tool_name":"Grep","tool_input":{"pattern":"charge"}}"#
            )),
            Some(Request::Content { .. })
        ));
        assert!(matches!(
            classify(&input(
                r#"{"tool_name":"Glob","tool_input":{"pattern":"**/*.rs"}}"#
            )),
            Some(Request::Files { .. })
        ));
        assert!(matches!(
            classify(&input(
                r#"{"tool_name":"Read","tool_input":{"file_path":"src/a.rs"}}"#
            )),
            Some(Request::Read { .. })
        ));
    }

    #[test]
    fn the_bash_search_tools_are_recognised() {
        for cmd in [
            "grep -rn x .",
            "rg x",
            "find . -name x",
            "ag x",
            "ack x",
            "ls -R",
        ] {
            let payload = format!(
                r#"{{"tool_name":"Bash","tool_input":{{"command":{}}}}}"#,
                serde_json::to_string(cmd).unwrap()
            );
            assert!(
                classify(&input(&payload)).is_some(),
                "{cmd} must be in the match set"
            );
        }
    }

    #[test]
    fn an_unrelated_tool_or_command_is_not_recognised() {
        assert!(classify(&input(
            r#"{"tool_name":"Write","tool_input":{"file_path":"a"}}"#
        ))
        .is_none());
        assert!(classify(&input(r#"{"tool_name":"Edit"}"#)).is_none());
        assert!(classify(&input(
            r#"{"tool_name":"Bash","tool_input":{"command":"cargo test"}}"#
        ))
        .is_none());
        assert!(classify(&input("{}")).is_none(), "no tool name at all");
    }

    #[test]
    fn a_ranged_read_is_left_alone() {
        assert!(classify(&input(
            r#"{"tool_name":"Read","tool_input":{"file_path":"src/a.rs","offset":40,"limit":20}}"#
        ))
        .is_none());
    }

    #[test]
    fn a_non_pretooluse_event_is_never_decided_on() {
        assert!(classify(&input(
            r#"{"hook_event_name":"PostToolUse","tool_name":"Grep","tool_input":{"pattern":"x"}}"#
        ))
        .is_none());
    }

    #[test]
    fn a_travsr_call_is_recognised_as_an_observation() {
        let r = classify(&input(
            r#"{"tool_name":"mcp__travsr__get_callers","tool_input":{"symbol":"charge"}}"#,
        ));
        assert_eq!(
            r,
            Some(Request::TravsrQuery {
                term: Some("charge".into())
            })
        );
    }

    /// The server name is chosen by whoever wrote the MCP config, so matching
    /// `mcp__travsr__` literally would miss a hand-wired server under any
    /// other name and silently disable the release valve for it.
    #[test]
    fn a_travsr_call_under_another_server_name_is_still_recognised() {
        assert!(matches!(
            classify(&input(
                r#"{"tool_name":"mcp__code-graph__find_references","tool_input":{"symbol":"x"}}"#
            )),
            Some(Request::TravsrQuery { .. })
        ));
        // But an unrelated MCP tool is not.
        assert!(classify(&input(
            r#"{"tool_name":"mcp__github__create_issue","tool_input":{"query":"x"}}"#
        ))
        .is_none());
    }

    #[test]
    fn only_identifier_shaped_terms_are_treated_as_symbols() {
        assert_eq!(as_symbol("charge"), Some("charge"));
        assert_eq!(
            as_symbol("PaymentService.charge"),
            Some("PaymentService.charge")
        );
        assert_eq!(as_symbol("travsr_store::open"), Some("travsr_store::open"));
        // Too short to mean anything as an exact lookup.
        assert_eq!(as_symbol("id"), None);
        // Regexes are text searches, not structural questions.
        assert_eq!(as_symbol("fn .*charge"), None);
        assert_eq!(as_symbol("^impl"), None);
        assert_eq!(as_symbol("charge|refund"), None);
        assert_eq!(as_symbol("TODO: fix"), None);
        assert_eq!(as_symbol("3"), None);
    }

    #[test]
    fn non_code_files_are_excluded_before_the_database_is_touched() {
        for p in [
            "README.md",
            "docs/guide.rst",
            "Cargo.lock",
            "package-lock.json",
            "go.sum",
            "config.toml",
            "settings.json",
            ".github/workflows/ci.yml",
            "logo.png",
            "LICENSE",
            "Makefile",
        ] {
            assert!(excluded_extension(p), "{p} must never be redirected");
        }
        for p in ["src/main.rs", "app/page.tsx", "lib/pay.py", "Main.java"] {
            assert!(!excluded_extension(p), "{p} is code");
        }
    }

    #[test]
    fn vendored_and_generated_trees_are_excluded() {
        assert!(vendored("node_modules/left-pad/index.js"));
        assert!(vendored("target/debug/build/x.rs"));
        assert!(vendored("vendor/github.com/x/y.go"));
        assert!(vendored(".venv/lib/site-packages/x.py"));
        assert!(vendored(".git/hooks/pre-commit"));
        assert!(
            !vendored("src/vendor_client.rs"),
            "a prefix is not a segment"
        );
        assert!(!vendored("crates/travsr-cli/src/main.rs"));
    }

    #[test]
    fn a_path_outside_the_repository_has_no_relative_form() {
        let root = Path::new(if cfg!(windows) { r"C:\repo" } else { "/repo" });
        let outside = if cfg!(windows) {
            r"C:\elsewhere\secrets.rs"
        } else {
            "/elsewhere/secrets.rs"
        };
        assert_eq!(repo_relative(root, None, outside), None);
    }

    #[test]
    fn a_relative_path_resolves_against_the_agents_cwd() {
        let root = Path::new(if cfg!(windows) { r"C:\repo" } else { "/repo" });
        let cwd = if cfg!(windows) {
            r"C:\repo\crates"
        } else {
            "/repo/crates"
        };
        assert_eq!(
            repo_relative(root, Some(cwd), "cli/src/main.rs").as_deref(),
            Some("crates/cli/src/main.rs"),
            "a relative path is the agent's, not the repo root's"
        );
        // And `..` is resolved lexically rather than by touching the disk.
        assert_eq!(
            repo_relative(root, Some(cwd), "../src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
    }

    /// A root that still carries a `.`, which is what a `cwd` of "." resolves
    /// to, must not stop every file path under it from being made relative.
    #[test]
    fn a_root_carrying_a_dot_component_still_matches() {
        let root = PathBuf::from(if cfg!(windows) {
            r"C:\repo\."
        } else {
            "/repo/."
        });
        let file = if cfg!(windows) {
            r"C:\repo\src\pay.rs"
        } else {
            "/repo/src/pay.rs"
        };
        assert_eq!(
            repo_relative(&root, None, file).as_deref(),
            Some("src/pay.rs")
        );
    }

    /// Joining components as strings produces `C:\\\repo` on Windows, which is
    /// not the path anyone meant. Pushing them does not.
    #[test]
    fn an_absolute_path_survives_normalisation() {
        let p = PathBuf::from(if cfg!(windows) {
            r"C:\a\.\b\..\c"
        } else {
            "/a/./b/../c"
        });
        assert_eq!(
            lexical(&p),
            PathBuf::from(if cfg!(windows) { r"C:\a\c" } else { "/a/c" })
        );
    }

    #[test]
    fn off_decides_nothing_at_all() {
        let i = input(r#"{"tool_name":"Grep","tool_input":{"pattern":"charge"}}"#);
        assert_eq!(decide(&i, GuardMode::Off, None).output, HookOutput::Neutral);
    }

    #[test]
    fn a_payload_outside_a_repository_is_never_blocked() {
        let i = input(r#"{"tool_name":"Grep","tool_input":{"pattern":"charge"}}"#);
        for mode in [GuardMode::Advisory, GuardMode::Strict] {
            assert!(!decide(&i, mode, None).blocks());
        }
    }

    #[test]
    fn a_missing_graph_database_is_never_blocked() {
        let d = tempfile::tempdir().unwrap();
        let i = input(r#"{"tool_name":"Grep","tool_input":{"pattern":"charge"}}"#);
        for mode in [GuardMode::Advisory, GuardMode::Strict] {
            assert!(!decide(&i, mode, Some(d.path())).blocks());
        }
    }

    #[test]
    fn an_unopenable_graph_database_is_never_blocked() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".travsr")).unwrap();
        // Not a SQLite file at all.
        std::fs::write(d.path().join(".travsr/graph.db"), b"garbage").unwrap();
        assert!(open_index(d.path()).is_err());
        let i = input(r#"{"tool_name":"Grep","tool_input":{"pattern":"charge"}}"#);
        assert!(!decide(&i, GuardMode::Strict, Some(d.path())).blocks());
    }

    #[test]
    fn head_is_resolved_from_a_loose_ref_without_spawning_git() {
        let d = tempfile::tempdir().unwrap();
        let git = d.path().join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let sha = "a".repeat(40);
        std::fs::write(git.join("refs/heads/main"), format!("{sha}\n")).unwrap();
        assert_eq!(head_sha(d.path()).as_deref(), Some(sha.as_str()));
    }

    #[test]
    fn head_is_resolved_from_packed_refs() {
        let d = tempfile::tempdir().unwrap();
        let git = d.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let sha = "b".repeat(40);
        std::fs::write(
            git.join("packed-refs"),
            format!("# pack-refs with: peeled\n{sha} refs/heads/main\n"),
        )
        .unwrap();
        assert_eq!(head_sha(d.path()).as_deref(), Some(sha.as_str()));
    }

    #[test]
    fn a_detached_head_is_read_directly() {
        let d = tempfile::tempdir().unwrap();
        let git = d.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        let sha = "c".repeat(40);
        std::fs::write(git.join("HEAD"), format!("{sha}\n")).unwrap();
        assert_eq!(head_sha(d.path()).as_deref(), Some(sha.as_str()));
    }

    #[test]
    fn an_unborn_branch_resolves_to_nothing() {
        let d = tempfile::tempdir().unwrap();
        let git = d.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(head_sha(d.path()), None, "no ref file, no packed-refs");
    }

    #[test]
    fn a_worktree_gitdir_file_is_followed_to_the_shared_refs() {
        let d = tempfile::tempdir().unwrap();
        let main_git = d.path().join("main/.git");
        let wt_git = main_git.join("worktrees/wt");
        std::fs::create_dir_all(main_git.join("refs/heads")).unwrap();
        std::fs::create_dir_all(&wt_git).unwrap();
        let sha = "d".repeat(40);
        std::fs::write(main_git.join("refs/heads/main"), format!("{sha}\n")).unwrap();
        std::fs::write(wt_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();

        let wt = d.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        assert_eq!(head_sha(&wt).as_deref(), Some(sha.as_str()));
    }

    #[test]
    fn glob_and_find_are_never_answerable_from_the_graph() {
        // Asserted at the redirect layer rather than end to end, because the
        // property is unconditional: whatever the index holds, a question about
        // the whole tree is not one the code graph can answer.
        let d = tempfile::tempdir().unwrap();
        let store = SqliteStore::open_in_memory().unwrap();
        let index = Index {
            root: d.path().to_path_buf(),
            store,
        };
        for r in [
            Request::Files {
                hint: Some("*.rs".into()),
                via: "Glob".into(),
            },
            Request::Files {
                hint: None,
                via: "find".into(),
            },
        ] {
            assert!(redirect_for(&index, None, &r).is_none());
        }
    }

    #[test]
    fn the_advisory_note_names_a_real_travsr_tool() {
        let served: Vec<String> = travsr_mcp::stdio_tools_list()["tools"]
            .as_array()
            .expect("tools/list has a tools array")
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default().to_string())
            .collect();
        let notes = [
            advisory_note(&Request::Content {
                term: Some("charge".into()),
                via: "Grep".into(),
            }),
            advisory_note(&Request::Files {
                hint: None,
                via: "Glob".into(),
            }),
        ];
        let mut checked = 0;
        for note in notes.into_iter().flatten() {
            let named: Vec<&String> = served.iter().filter(|t| note.contains(*t)).collect();
            assert!(
                !named.is_empty(),
                "a nudge that names no served tool teaches nothing: {note}"
            );
            checked += 1;
        }
        assert_eq!(checked, 2, "both nudges must exist to be worth checking");
    }

    /// A read this arm is reached for is one `redirect_for` has already placed
    /// outside the graph: a doc, a lockfile, a config, something vendored or
    /// out of the repository. `get_context` cannot answer for any of them, so
    /// the honest nudge is no nudge.
    #[test]
    fn an_unindexed_read_gets_no_nudge() {
        assert_eq!(
            advisory_note(&Request::Read {
                path: "README.md".into(),
            }),
            None
        );
    }
}
