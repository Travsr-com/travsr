//! `travsr faq` — questions about travsr itself.
//!
//! Distinct from `travsr ask --examples`, which lists questions about the
//! *indexed code*. This answers questions about the tool: what it is, how it
//! works, how to install it, where the data goes.
//!
//! Answered offline from constants rather than from the graph. A user asking
//! "what is travsr" has often not indexed anything yet, and routing these
//! through retrieval is what produced the failure this exists to fix:
//! `travsr ask "what is this repo written in?"` matched the words against symbol
//! names and returned a hundred rows of `var:REPO` and bench files. `ask` is
//! graph-grounded, so it answers questions about code and cannot answer
//! questions about the product.

use crate::progress::Palette;

/// One question, answered as a short lead plus scannable points.
///
/// Prose was the first shape and it read badly in a terminal: five wrapped lines
/// of paragraph that have to be read start to finish before the shape of the
/// answer is visible. A lead line plus points can be scanned, which is what
/// someone does with terminal output.
pub(crate) struct Entry {
    pub question: &'static str,
    /// One sentence. The answer, if the reader stops here.
    pub lead: &'static str,
    /// The fuller answer: the reasoning, caveat or consequence behind the
    /// points. Points say what is true and are quick to scan; on their own they
    /// read as a list of assertions with nothing joining them, which is the
    /// feedback this field exists to answer. Prose carries the why.
    pub detail: &'static str,
    /// Supporting points, each short enough to sit on one line.
    pub points: &'static [&'static str],
    /// An ordered walkthrough, as (what this does, command) pairs. Used where
    /// the answer is a sequence rather than a fact: a list of commands printed
    /// as a block loses the order, and "install" is only useful as the whole
    /// path from nothing to a working query.
    pub steps: &'static [(&'static str, &'static str)],
    /// Commands that act on the answer. Empty when there is nothing to run,
    /// rather than inventing one. A slice because some answers genuinely have
    /// several: install has three real routes, and naming one while mentioning
    /// the others in prose makes the reader go looking for them.
    pub commands: &'static [&'static str],
}

const ENTRIES: &[Entry] = &[
    Entry {
        question: "what is travsr?",
        lead: "A code graph that lives next to git.",
        detail: "Most tools treat a repo as text and search it. Travsr parses it, resolves each reference to the definition it actually points at, and keeps the result as a graph. A question like \"what calls this\" then becomes a traversal with an exact answer, which is how it hands an agent a small, complete slice of the repo instead of a large pile of similar-looking chunks.",
        steps: &[],
        points: &[
            "every function, class, call and import is a node or an edge",
            "structural questions get computed answers, not guesses",
            "the graph rebuilds as you commit, so it matches your checkout",
            "agents query it over MCP, so it is not just a CLI",
        ],
        commands: &[
            "travsr status",
        ],
    },
    Entry {
        question: "how is it different from search or vector RAG?",
        lead: "It computes relationships instead of approximating them.",
        detail: "A vector index answers \"what looks like this query\". That works for prose and struggles on code, where the function you need may share no words with your question, and two near-identical helpers may have nothing to do with each other. Travsr answers \"what is connected to this\": a caller three files away is found because an edge says so, not because the wording happened to match.",
        steps: &[],
        points: &[
            "grep matches text and cannot tell a call from a comment",
            "vector search chunks code and retrieves by similarity",
            "similar code is not the same thing as related code",
            "travsr resolves each call to the definition it points at",
        ],
        commands: &[],
    },
    Entry {
        question: "how does it work?",
        lead: "Two indexing passes, then a retrieval pipeline.",
        detail: "Phase A is fast and always runs, so the index is usable seconds after init. Phase B is the expensive part and runs in the background via the daemon unless you pass --semantic. Queries work throughout: before Phase B lands you get structure, and after it you get call edges too.",
        steps: &[],
        points: &[
            "Phase A     tree-sitter parses every tracked file into nodes",
            "Phase B     resolves calls to the definitions they point at",
            "retrieval   seeds from your query, walks the graph, reranks",
            "packing     trims the result to the token budget you have",
        ],
        commands: &[
            "travsr status",
        ],
    },
    Entry {
        question: "how do I install it?",
        lead: "Install, then index a repo. Five steps from nothing to a query.",
        detail: "The installer checks the release signature before it installs anything, and drops the binary somewhere on your PATH for the current user. Nothing else is needed to start: models for semantic search are optional and download later, only if you ask for them.",
        steps: &[
            ("install the binary", "curl -fsSL https://travsr.com/install.sh | sh"),
            ("check it is on your PATH", "travsr --version"),
            ("index a repo, from its root", "travsr init --semantic"),
            ("confirm the graph is ready", "travsr status"),
            ("ask it something", "travsr ask \"<a symbol in your code>\""),
        ],
        points: &[
            "sh -s -- --system   installs for all users, not just you",
            "npm     npm install -g @travsr.com/travsr",
            "cargo   cargo build --release -p travsr-cli   (from a clone)",
            "no daemon, database or account to set up first",
        ],
        commands: &[],
    },
    Entry {
        question: "how do I start using it on a repo?",
        lead: "Run this once from the repo root.",
        detail: "init is incremental and safe to repeat: a second run picks up only what changed, which is exactly what the git hook does on your behalf after each commit. Reach for --force only after changing a flag that affects analysis, since per-file change detection cannot see that by itself.",
        steps: &[],
        points: &[
            "parses every tracked file, skipping whatever git ignores",
            "installs a hook so the graph follows your commits",
            "registers the repo so other tools can find it",
            "--semantic waits for call edges instead of backgrounding them",
        ],
        commands: &[
            "travsr init --semantic",
        ],
    },
    Entry {
        question: "which languages does it support?",
        lead: "Sixteen parse out of the box; four resolve calls with no setup.",
        detail: "Structural parsing covers all sixteen: definitions, files, imports and the shape of the code. Resolving a call across files needs a language-specific analyzer, which is why some languages read as partial until you install one. Enabling is per repo even when the analyzer is installed globally, so indexing a new checkout never silently runs something you did not turn on there.",
        steps: &[],
        points: &[
            "always on   rust, typescript, javascript, python",
            "one command adds full analysis for go, ruby, php, swift and more",
            "the rest parse for structure and can be enabled per repo",
            "travsr lang install <language>   turns one on, inside the repo",
        ],
        commands: &[
            "travsr lang list",
        ],
    },
    Entry {
        question: "does my code leave my machine?",
        lead: "No. Indexing, storage and queries are all local.",
        detail: "The cloud tier is opt-in and separate: you deploy it and point at it deliberately. Nothing on the local path calls home, so travsr works on a machine with no network at all once the binary and any models you want are present.",
        steps: &[],
        points: &[
            ".travsr/    the graph, inside your repo",
            "~/.travsr   shared binaries, models and the repo registry",
            "embedding and reranking models run as local sidecars",
            "the network is used to download travsr and optional models",
        ],
        commands: &[],
    },
    Entry {
        question: "do I need the daemon running?",
        lead: "No. Queries work without it; it keeps the graph fresher.",
        detail: "The git hook already keeps the graph honest at commit boundaries. The daemon narrows the window further, which is what you want when you are asking about code you are still in the middle of writing.",
        steps: &[],
        points: &[
            "without it, queries are served from the database as it stands",
            "with it, edits are picked up as you save, not only on commit",
            "it runs the background Phase B work after init",
            "and hosts MCP, so an agent connects without a cold start",
        ],
        commands: &[
            "travsr daemon start",
        ],
    },
    Entry {
        question: "how do I connect it to an AI agent?",
        lead: "Travsr speaks MCP, so any agent that supports it can query the graph.",
        detail: "Run it from the repo you want the agent to see. Start with --print, which changes nothing and shows you exactly what it would write, then run it for real.",
        steps: &[],
        points: &[
            "detects the coding tools you already have installed",
            "claude-code is configured for you, in .mcp.json and CLAUDE.md",
            "cursor and zed get their config printed for you to paste",
            "travsr connect --print   shows what would change first",
        ],
        commands: &[
            "travsr connect",
        ],
    },
    Entry {
        question: "what is the graph, exactly?",
        lead: "Nodes are the things you write; edges are how they relate.",
        detail: "Identity is a Kythe VName rather than a file offset, so a node survives reformatting, renaming and re-indexing, and two repos can share one identity space. That is what makes cross-repo questions possible at all, and why an answer stays pointing at the right thing while you edit around it.",
        steps: &[],
        points: &[
            "nodes       functions, classes, methods, files, imports",
            "ref/call    this calls that, the edge most answers lean on",
            "defines     this file or class contains that symbol",
            "depends     this file imports that module",
        ],
        commands: &[
            "travsr graph <symbol> --direction both",
        ],
    },
    Entry {
        question: "how does MCP work?",
        lead: "MCP is the only way into travsr from outside.",
        detail: "The envelope is the part worth knowing about: repo content arrives labelled as data, so a comment in your source that happens to read like an instruction is not treated as one. There is no REST or GraphQL surface either, which keeps the thing you have to trust down to a single local pipe.",
        steps: &[],
        points: &[
            "your agent launches `travsr mcp --stdio` as a child process",
            "the two speak JSON-RPC over that process's stdin and stdout",
            "travsr advertises its tools; the agent calls them by name",
            "every response is wrapped in a <travsr-data> envelope",
        ],
        commands: &[
            "travsr mcp --stdio",
        ],
    },
    Entry {
        question: "how do I connect MCP to my agent?",
        lead: "One command detects your tools and configures what it can.",
        detail: "connect records the absolute path of the binary it was run from, so an agent started days later launches the same travsr you tested with. Run it again after moving or upgrading the binary, and remember the agent only reads its config at startup.",
        steps: &[
            ("see what would change, without writing anything", "travsr connect --print"),
            ("write the config for every tool it can", "travsr connect"),
            ("restart the agent so it re-reads its config", ""),
        ],
        points: &[
            "recognised today   claude-code, cursor, zed",
            "claude-code is written for you; the others print config to paste",
            "one tool only      travsr connect --tool cursor",
            "undo it later      travsr connect --remove",
        ],
        commands: &[],
    },
    Entry {
        question: "what tools does the MCP server expose?",
        lead: "Twenty-six, grouped by the kind of question they answer.",
        detail: "Your agent lists them itself once connected, so there is nothing to memorise. The grouping matters more than the names: structure tools traverse edges and are exact, search tools rank and can miss, and context tools exist to hand the model real source text with its surroundings attached.",
        steps: &[],
        points: &[
            "structure   get_callers, get_dependencies, get_blast_radius",
            "search      search_symbol, find_references, find_pattern",
            "context     get_context, get_snippets, get_execution_path",
            "overview    get_repo_map, get_graph_stats, get_lang_status",
        ],
        commands: &[],
    },
    Entry {
        question: "how does the VS Code extension work?",
        lead: "It drives the same binary and draws the graph in the editor.",
        detail: "The extension is a client, not a second implementation: it runs the same travsr the CLI does, so what it draws and what `travsr graph` prints cannot disagree. If travsr is not installed yet it offers to download it for you.",
        steps: &[],
        points: &[
            "a Travsr Graph view, plus a Repo Files tree",
            "callers, dependencies and blast radius for the current symbol",
            "LSP diagnostics overlaid onto the graph nodes",
            "Check Blast Radius Before Edit, and Copy Graph Context for Chat",
        ],
        commands: &[],
    },
    Entry {
        question: "what can I ask about my code?",
        lead: "Structural questions, anchored to a symbol name.",
        detail: "ask is graph-grounded, so it answers from edges rather than from wording. Give it a name it can anchor on and it traverses; give it a purely conceptual question with no symbol in it and it may abstain rather than guess.",
        steps: &[],
        points: &[
            "what calls it, what it depends on, where it is used",
            "what breaks if I change it, and what path leads to it",
            "naming a real symbol gives by far the best results",
            "references   every use site, as path:line",
            "graph        callers and dependencies as a tree",
            "pattern      grep, but only inside the relevant files",
        ],
        commands: &[
            "travsr ask --examples",
        ],
    },
    Entry {
        question: "why did it say it found nothing?",
        lead: "It abstains rather than return a low-confidence guess.",
        detail: "A confident wrong answer costs more than an empty one, especially when an agent is about to act on it. When it abstains it prints what to try instead, and explain, given the query and a symbol you expected it to return, shows which terms matched and which threshold it fell short of.",
        steps: &[],
        points: &[
            "conceptual questions naming no symbol are a known gap",
            "a misspelled symbol will not fuzzy-match its way to an answer",
            "the abstention suggests a narrower command to run",
            "semantic search widens it, if you indexed with --semantic",
        ],
        commands: &[
            "travsr explain \"<your query>\" <symbol>",
        ],
    },
    Entry {
        question: "where is the data, and how do I remove it?",
        lead: "Two directories, both safe to delete.",
        detail: "There is no uninstall step to run first and nothing outside these two paths. Remove them and travsr is gone; run init again and the graph comes back from your source, because everything in it was derived from your source to begin with.",
        steps: &[],
        points: &[
            ".travsr/    the per-repo graph, inside the repo",
            "~/.travsr   shared binaries, models and the repo registry",
            "deleting either loses nothing that cannot be rebuilt",
            "the .travsr directory only affects the checkout it sits in",
        ],
        commands: &[
            "rm -rf .travsr",
            "travsr init --semantic",
        ],
    },
    Entry {
        question: "how do I see the logs?",
        lead: "The daemon writes a log per repo, and one command reads it.",
        detail: "It reads the file rather than asking the daemon, so it still works after a crash, which is when you need it. One caveat worth knowing before you need it: the log is written at info, so --level debug cannot show you debug lines that were never recorded. Start the daemon with --verbose when you want them.",
        steps: &[],
        points: &[
            "last 50 lines by default; --lines 0 prints all retained history",
            "-f follows new lines, across the daily rotation",
            "--level warn, --since 10m and --repo narrow it down",
            "--json prints the stored lines verbatim, for jq or a collector",
        ],
        commands: &[
            "travsr daemon logs",
            "travsr daemon logs -f --level warn",
        ],
    },
    Entry {
        question: "how do I tell if something is broken?",
        lead: "Three commands, in the order worth trying them.",
        detail: "status answers the usual question, which is whether the graph is current. fsck is for the rarer one, where the database itself is inconsistent, and it only reports until you pass --fix. explain is for when a query returned nothing you expected: it shows the terms that matched and the threshold that stopped it.",
        steps: &[
            ("check the index first", "travsr status"),
            ("then what the daemon did", "travsr daemon logs --level warn"),
            ("then the graph itself", "travsr fsck"),
        ],
        points: &[
            "status   is the index current, and did Phase B finish",
            "logs     what the daemon has been doing, including after a crash",
            "fsck     ghost nodes and orphan edges; reports until --fix",
            "explain  why one query ranked or skipped a symbol",
        ],
        commands: &[],
    },
    Entry {
        question: "how do I use it across several repos?",
        lead: "Index each one, then serve them together.",
        detail: "Node identity is a VName rather than a path, so the same symbol keeps one identity across repositories and a query does not stop at the repo boundary. Every repo you init registers itself, which is what --global reads.",
        steps: &[],
        points: &[
            "travsr init   inside each repo registers it globally",
            "repos         lists everything registered",
            "mcp --global  serves all of them from one server",
            "the registry lives in ~/.travsr/registry.json",
        ],
        commands: &[
            "travsr repos",
            "travsr mcp --stdio --global",
        ],
    },
    Entry {
        question: "how do I make search find more?",
        lead: "Semantic search, a reranker, and synonyms for your own vocabulary.",
        detail: "These are the reasons a query can come back thin. Embeddings let a question match code that shares no words with it, the reranker reorders what came back by relevance, and synonyms teach it that your team says auth where the code says authenticate. All three are local, and all three are optional.",
        steps: &[],
        points: &[
            "embed status   is semantic search on for this repo",
            "embed init     downloads a local model and turns it on",
            "rerank install downloads the cross-encoder that reorders results",
            "synonym add    teaches it your own terms, up to 200 pairs",
        ],
        commands: &[
            "travsr embed status",
            "travsr synonym list",
        ],
    },
    Entry {
        question: "can I run it for a team?",
        lead: "Yes, over the SSE server, but it is a deliberate step.",
        detail: "serve binds to loopback by default and speaks plaintext HTTP with bearer tokens, so putting it on a network is something you opt into rather than something that happens by accident. Bind beyond this machine only with a TLS terminator in front of it.",
        steps: &[],
        points: &[
            "stdio is the local transport; SSE is the shared one",
            "defaults to 127.0.0.1:3000",
            "--host 0.0.0.0 needs TLS terminating in front of it",
            "--tenants-dir is required; it keeps tenants' data separate",
        ],
        commands: &[
            "travsr serve --tenants-dir <dir>",
        ],
    },
    Entry {
        question: "can I use it in CI?",
        lead: "Yes. Index synchronously, and read the output as JSON.",
        detail: "The thing to get right in CI is ordering: init backgrounds the expensive pass by default, so a job that queries call edges immediately after it can race the work. --semantic makes init wait, which is what it is for.",
        steps: &[],
        points: &[
            "init --semantic finishes call edges before it returns",
            "--json on init, status and ask gives machine-readable output",
            "travsr index emits a graph JSON without touching a repo's .travsr",
            "no daemon is needed; queries read the database directly",
        ],
        commands: &[
            "travsr init --semantic --json",
            "travsr index <dir> --output graph.json",
        ],
    },
    Entry {
        question: "how do I update travsr?",
        lead: "Re-run whichever way you installed it.",
        detail: "There is no self-update command, deliberately: a tool that rewrites its own binary is a tool you have to trust more than this one asks you to. Re-running the installer replaces the binary in place and leaves your indexes alone, since the graph is per repo and rebuilds from source anyway.",
        steps: &[],
        points: &[
            "curl -fsSL https://travsr.com/install.sh | sh   replaces it in place",
            "npm install -g @travsr.com/travsr@latest",
            "travsr --version   confirms what you are now running",
            "re-run travsr connect afterwards if the path changed",
        ],
        commands: &[
            "travsr --version",
        ],
    },
];

/// The FAQ entry a free-form question is asking, if any.
///
/// Matched by word overlap against the catalogue's own questions rather than a
/// separate list of phrases. The phrase list was the earlier design and it did
/// not converge: every round of feedback found a wording it did not contain, and
/// each fix added exactly that wording. Matching the questions themselves means a
/// new FAQ entry is reachable from `ask` the moment it is written, with nothing
/// to keep in sync.
///
/// Deliberately strict. Hijacking a real code search is a worse failure than
/// missing a meta question: the user gets a confident answer to a question they
/// did not ask, where a miss just runs the search they wanted. So a match needs
/// most of the question's distinctive words, not a few.
pub(crate) fn match_question(query: &str) -> Option<&'static Entry> {
    // A bare word or two is a symbol lookup, not a question. `ask` documents
    // itself as accepting a bare symbol name, so `travsr ask "install"` is a
    // search for something called `install`, and answering "here is how to
    // install travsr" would replace a real search with an unrelated answer.
    //
    // Gated on the shape of what was typed rather than on content words: "what is
    // travsr" reduces to the single word "travsr" once filler is dropped, so
    // counting content words would reject the catalogue's own questions.
    let raw_words = query.split_whitespace().count();
    if raw_words < 3 && !query.trim_end().ends_with('?') {
        return None;
    }

    let asked = distinctive_words(query);
    if asked.is_empty() {
        return None;
    }

    let mut best: Option<(usize, &'static Entry)> = None;
    for e in ENTRIES {
        let want = distinctive_words(e.question);
        if want.is_empty() {
            continue;
        }
        let hits = want.iter().filter(|w| asked.contains(*w)).count();
        if hits != want.len() {
            continue;
        }
        // Coverage has to run both ways. Requiring only the catalogue's words
        // meant "how does it work?" reduced to the single word "work", so
        // "how does the parser work" matched it and the reader's search was
        // replaced by an answer about travsr. Requiring most of what *they*
        // typed to be accounted for keeps an extra subject like "parser" from
        // being ignored.
        // "travsr" is the implicit subject of every catalogue question, so it
        // carries no signal about *which* one is being asked. Counting it against
        // coverage rejected "how does travsr work", where naming the subject is
        // the most natural phrasing. It still counts for matching, so "what is
        // travsr?" is reachable.
        let subject = |w: &String| w == "travsr";
        let judged: Vec<&String> = asked.iter().filter(|w| !subject(w)).collect();
        if !judged.is_empty() {
            let covered = judged.iter().filter(|w| want.contains(**w)).count();
            if covered * 5 < judged.len() * 3 {
                continue;
            }
        }
        if best.map_or(true, |(n, _)| hits > n) {
            best = Some((hits, e));
        }
    }
    best.map(|(_, e)| e)
}

/// Content words of a question, lowercased, with filler removed.
///
/// The filler list is the words that carry no signal in a question ("how", "do",
/// "I", "the"). What remains is what the question is actually about, which is
/// what both sides are compared on.
fn distinctive_words(text: &str) -> Vec<String> {
    const FILLER: &[&str] = &[
        "a", "am", "an", "and", "are", "as", "at", "be", "by", "can", "did", "do", "does", "for",
        "from", "how", "i", "if", "in", "is", "it", "its", "me", "my", "of", "on", "or", "should",
        "that", "the", "then", "there", "this", "to", "use", "using", "want", "was", "what",
        "when", "where", "which", "who", "why", "will", "with", "you", "your",
    ];
    // Underscore is not a separator here. Splitting it turned `install_hook`
    // into "install", which matched the install FAQ and hijacked a real symbol
    // search. A symbol is one token.
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .filter(|w| !FILLER.contains(&w.as_str()))
        .collect()
}

/// Print one entry: lead, points, then the command.
pub(crate) fn print_entry(e: &Entry, pal: Palette) {
    for line in wrap(e.lead, 74) {
        println!("  {line}");
    }
    // The lead is a sentence and the points are a list; run together they read
    // as one block and the lead stops being the summary it is meant to be.
    // Steps open with their own blank line, so adding one here would double it.
    if e.steps.is_empty() && !e.points.is_empty() {
        println!();
    }
    for (i, (what, cmd)) in e.steps.iter().enumerate() {
        println!();
        println!("    {} {what}", pal.dim(&format!("{}.", i + 1)));
        // A step can be an instruction with nothing to run ("restart the agent"),
        // and printing an empty command line for it would look like a bug.
        if !cmd.is_empty() {
            println!("       {}", pal.ident(cmd));
        }
    }
    if !e.steps.is_empty() && !e.points.is_empty() {
        println!();
    }
    for p in e.points {
        // Only wrap when it does not fit. `wrap` splits on whitespace, so running
        // it over a short point would collapse the runs of spaces some points use
        // to align a label against its description ("Phase A   parses ...").
        if p.chars().count() <= 70 {
            println!("    {} {p}", pal.dim("·"));
            continue;
        }
        let mut lines = wrap(p, 70).into_iter();
        if let Some(first) = lines.next() {
            println!("    {} {first}", pal.dim("·"));
        }
        for rest in lines {
            println!("      {rest}");
        }
    }
    if !e.detail.is_empty() {
        println!();
        for line in wrap(e.detail, 74) {
            println!("  {line}");
        }
    }
    if !e.commands.is_empty() {
        println!();
        for c in e.commands {
            println!("    {} {}", pal.dim("$"), pal.ident(c));
        }
    }
}

/// Every catalogue question, for `ask --examples` to list.
///
/// These are answered by `ask` directly, so they are shown without a command
/// beside them: the question *is* the command.
pub(crate) fn questions() -> impl Iterator<Item = &'static str> {
    ENTRIES.iter().map(|e| e.question)
}

/// Wrap on whitespace at `width` columns.
///
/// Hand-rolled to keep this dependency-free, and because the answers are short
/// prose with no markup: a word longer than the width is emitted on its own line
/// rather than split, since breaking a command or a path would make it wrong.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{wrap, ENTRIES};

    #[test]
    fn every_entry_is_a_question_with_an_answer() {
        assert!(!ENTRIES.is_empty());
        for e in ENTRIES {
            assert!(
                e.question.ends_with('?'),
                "`{}` is not a question",
                e.question
            );
            assert!(
                e.question.chars().next().is_some_and(|c| c.is_lowercase()),
                "`{}` should read as spoken text",
                e.question
            );
            assert!(!e.lead.is_empty(), "`{}` has no lead", e.question);
            // A lead that runs long is a paragraph again, which is the format
            // this structure replaced. One sentence, readable at a glance.
            assert!(
                e.lead.chars().count() <= 90,
                "`{}` has a {}-char lead; that is a paragraph, not a lead",
                e.question,
                e.lead.chars().count()
            );
        }
    }

    /// Points state facts; on their own they read as a list of assertions with
    /// nothing joining them. Every answer owes the reader the reasoning behind
    /// them, so the prose is required rather than optional, and long enough to
    /// be an explanation rather than one more restated point.
    #[test]
    fn every_entry_explains_itself() {
        for e in ENTRIES {
            let n = e.detail.chars().count();
            assert!(
                n >= 140,
                "`{}` has a {n}-char detail; that is another point, not an explanation",
                e.question
            );
        }
    }

    /// Every command a reader can type must be named by some answer.
    ///
    /// The catalogue is the first place someone looks, so a command that appears
    /// in no answer is one they will only find by reading `--help` line by line.
    /// `daemon logs` was exactly that until this test was written: a full
    /// logging surface, with filters and follow, mentioned nowhere.
    ///
    /// This checks that the name appears, not that it is well explained, which
    /// no test can check. Its job is to fail when a *new* command is added and
    /// the catalogue is not updated to mention it.
    #[test]
    fn every_command_is_mentioned_somewhere() {
        use clap::CommandFactory as _;
        let cli = crate::Cli::command();

        let mut corpus = String::new();
        for e in ENTRIES {
            corpus.push_str(e.question);
            corpus.push(' ');
            corpus.push_str(e.lead);
            corpus.push(' ');
            corpus.push_str(e.detail);
            corpus.push(' ');
            for p in e.points {
                corpus.push_str(p);
                corpus.push(' ');
            }
            for (w, c) in e.steps {
                corpus.push_str(w);
                corpus.push(' ');
                corpus.push_str(c);
                corpus.push(' ');
            }
            for c in e.commands {
                corpus.push_str(c);
                corpus.push(' ');
            }
        }

        for sub in cli.get_subcommands() {
            // `help` is clap's own, and `hook-run` is hidden because the git hook
            // runs it rather than a person. Neither is a reader's to type.
            if sub.get_name() == "help" || sub.is_hide_set() {
                continue;
            }
            let name = sub.get_name();
            let mentioned = corpus
                .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                .any(|w| w == name);
            assert!(
                mentioned,
                "`travsr {name}` is a command a reader can type, and no answer mentions it"
            );
        }
    }

    /// A command that does not exist sends someone to a dead end while looking
    /// authoritative, which is what #727 was. Read the subcommand list from clap
    /// rather than keeping a copy here.
    /// Points are printed unwrapped when short, to keep column alignment, so an
    /// over-long one would run off a narrow terminal. Kept within the width the
    /// renderer treats as "short".
    #[test]
    fn points_fit_on_one_line() {
        for e in ENTRIES {
            for p in e.points {
                let n = p.chars().count();
                assert!(
                    n <= 70,
                    "`{}` has a {n}-char point; it would wrap and lose alignment: {p}",
                    e.question
                );
            }
        }
    }

    /// Every catalogue question must be reachable by asking it. The point of
    /// matching the questions themselves is that a new entry works from `ask`
    /// the moment it is written, with no second list to update.
    #[test]
    fn every_catalogue_question_matches_itself() {
        for e in ENTRIES {
            let got = super::match_question(e.question)
                .unwrap_or_else(|| panic!("`{}` does not match itself", e.question));
            assert_eq!(got.question, e.question);
        }
    }

    /// The failure that matters. Hijacking a real search is worse than missing a
    /// meta question: the reader gets a confident answer to something they did
    /// not ask, where a miss simply runs the search they wanted.
    ///
    /// Each of these was an actual hijack before the matcher required coverage in
    /// both directions and stopped splitting on underscore. `install_hook` became
    /// "install" and matched the install entry; "how does the parser work"
    /// reduced to "work" and matched "how does it work?".
    #[test]
    fn code_searches_are_never_hijacked() {
        for q in [
            "what calls install_hook",
            "how does the parser work",
            "install_creates_only_sh",
            "NodeId",
            "repo_languages",
            "language_distribution",
            "where is NodeId used",
            "daemon_client",
            "run",
            "what calls data",
            "how does the daemon work",
            "install",
        ] {
            assert!(
                super::match_question(q).is_none(),
                "`{q}` is a code search and must reach retrieval"
            );
        }
    }

    /// A single-word catalogue question is the shape that caused the hijacks: it
    /// reduces to one token that any sentence containing that token matches.
    /// Coverage both ways is what makes it safe, so this pins that a question
    /// carrying only one distinctive word still cannot swallow a longer query.
    #[test]
    fn a_longer_query_does_not_match_a_one_word_question() {
        // "how does it work?" reduces to ["work"].
        assert!(super::match_question("how does it work").is_some());
        assert!(super::match_question("how does the scheduler work").is_none());
        assert!(super::match_question("does the retry work after a crash").is_none());
    }

    /// Steps are a walkthrough someone follows in order, so each one has to be a
    /// real command and the sequence has to be complete enough to end somewhere
    /// useful.
    #[test]
    fn steps_are_ordered_real_commands() {
        use clap::CommandFactory as _;
        let cmd = crate::Cli::command();
        let known: Vec<String> = cmd
            .get_subcommands()
            .flat_map(|c| {
                std::iter::once(c.get_name().to_string())
                    .chain(c.get_all_aliases().map(str::to_string))
            })
            .collect();

        for e in ENTRIES {
            for (what, c) in e.steps {
                assert!(
                    !what.is_empty(),
                    "`{}` has a step with no description",
                    e.question
                );
                // A step may legitimately have no command.
                // The install line is a shell pipeline, not a travsr invocation.
                if !c.starts_with("travsr ") {
                    continue;
                }
                let sub = c.split_whitespace().nth(1).unwrap_or("");
                // `travsr --version` is a flag on the root command, not a
                // subcommand, and is a legitimate step.
                if sub.starts_with("--") {
                    continue;
                }
                assert!(
                    known.iter().any(|k| k == sub),
                    "`{c}` names unknown subcommand {sub:?}"
                );
            }
        }
    }

    /// Every printed command must parse as a real invocation, arguments and all.
    ///
    /// This used to check only the subcommand name, which is not enough: a name
    /// can exist while the line is still unrunnable. `travsr explain "<query>"`
    /// passed that check and failed for a reader, because `explain` takes a
    /// query *and* a symbol. Handing someone a command that exits 2 is the same
    /// dead end as naming a subcommand that does not exist (#727), so the whole
    /// line goes through clap.
    #[test]
    fn every_command_is_runnable() {
        use clap::CommandFactory as _;

        let check = |c: &str, where_: &str| {
            // Only travsr invocations are parseable here; the installer
            // pipeline, the cargo build and `rm` are not.
            if !c.starts_with("travsr ") {
                return;
            }
            // Split the way a shell would: a quoted placeholder such as
            // `"<a symbol in your code>"` is one argument, and splitting it on
            // whitespace would report a false failure.
            let mut argv: Vec<String> = Vec::new();
            let mut cur = String::new();
            let mut quoted = false;
            for ch in c.chars() {
                match ch {
                    '"' => quoted = !quoted,
                    c if c.is_whitespace() && !quoted => {
                        if !cur.is_empty() {
                            argv.push(std::mem::take(&mut cur));
                        }
                    }
                    c => cur.push(c),
                }
            }
            if !cur.is_empty() {
                argv.push(cur);
            }
            if let Err(e) = crate::Cli::command().try_get_matches_from(argv) {
                // `--version` and `--help` "fail" by printing and exiting, which
                // is exactly what a reader running them wants.
                use clap::error::ErrorKind::{DisplayHelp, DisplayVersion};
                assert!(
                    matches!(e.kind(), DisplayHelp | DisplayVersion),
                    "`{c}` in {where_} does not parse: {}",
                    e.render().to_string().lines().next().unwrap_or_default()
                );
            }
        };

        for e in ENTRIES {
            for c in e.commands {
                check(c, e.question);
            }
            for (_, c) in e.steps {
                check(c, e.question);
            }
        }
    }

    #[test]
    fn wrapping_preserves_every_word() {
        for e in ENTRIES {
            for text in [e.lead, e.detail] {
                let joined = wrap(text, 76).join(" ");
                let before: Vec<&str> = text.split_whitespace().collect();
                let after: Vec<&str> = joined.split_whitespace().collect();
                assert_eq!(before, after, "wrapping altered `{}`", e.question);
            }
        }
        // A word longer than the width must survive rather than being split.
        assert_eq!(
            wrap("short verylongunbreakabletoken end", 10).join(" "),
            "short verylongunbreakabletoken end"
        );
    }

    #[test]
    fn no_line_exceeds_the_wrap_width() {
        for e in ENTRIES {
            for line in wrap(e.lead, 76).into_iter().chain(wrap(e.detail, 76)) {
                let n = line.chars().count();
                // Only a single over-long word may exceed it.
                assert!(
                    n <= 76 || !line.contains(' '),
                    "`{}` produced a {n}-column line: {line}",
                    e.question
                );
            }
        }
    }
}
