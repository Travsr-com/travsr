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
    /// Supporting points, each short enough to sit on one line.
    pub points: &'static [&'static str],
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
        points: &[
            "every function, class and call is a node or an edge",
            "structural questions get exact answers, not guesses",
            "the graph updates as you commit",
        ],
        commands: &[],
    },
    Entry {
        question: "how is it different from search or vector RAG?",
        lead: "It computes relationships instead of approximating them.",
        points: &[
            "vector search chunks code and retrieves by similarity",
            "travsr parses the code and resolves each call to its definition",
            "\"what calls this\" is derived, not inferred",
        ],
        commands: &[],
    },
    Entry {
        question: "how does it work?",
        lead: "Two indexing passes, then a retrieval pipeline.",
        points: &[
            "Phase A   tree-sitter parses every tracked file into nodes",
            "Phase B   resolves calls to the definitions they point at",
            "retrieval seeds from your query, walks the graph, reranks",
            "the result is packed to fit your token budget",
        ],
        commands: &[],
    },
    Entry {
        question: "how do I install it?",
        lead: "Three routes, all installing the same binary.",
        points: &[
            "the installer verifies the release signature before installing",
            "add -s -- --system to install system-wide",
        ],
        commands: &[
            "curl -fsSL https://travsr.com/install.sh | sh",
            "npm install -g @travsr.com/travsr",
            "cargo build --release -p travsr-cli",
        ],
    },
    Entry {
        question: "how do I start using it on a repo?",
        lead: "Run this from the repo root.",
        points: &[
            "indexes the tracked files",
            "installs a git hook so the graph stays current",
        ],
        commands: &["travsr init --semantic"],
    },
    Entry {
        question: "which languages does it support?",
        lead: "Sixteen for structure; four resolve calls without setup.",
        points: &[
            "native   rust, typescript, javascript, python",
            "others need an analyzer installed",
        ],
        commands: &["travsr lang list"],
    },
    Entry {
        question: "does my code leave my machine?",
        lead: "No. Indexing, storage and queries are all local.",
        points: &[
            "the graph lives in .travsr/ inside your repo",
            "embedding models run as a local sidecar",
            "network is only for downloading travsr and optional models",
        ],
        commands: &[],
    },
    Entry {
        question: "do I need the daemon running?",
        lead: "No, but it helps.",
        points: &[
            "queries work without it, served from the database",
            "the daemon keeps semantic analysis current as files change",
            "and answers faster from a warm store",
        ],
        commands: &["travsr daemon start"],
    },
    Entry {
        question: "how do I connect it to an AI agent?",
        lead: "Travsr speaks MCP, so any agent that supports it can query the graph.",
        points: &[
            "detects installed tools and writes their config",
            "add --print first to see what would change",
        ],
        commands: &["travsr connect"],
    },
    Entry {
        question: "what can I ask about my code?",
        lead: "Structural questions, anchored to a symbol name.",
        points: &[
            "what calls it, what it depends on, where it is used",
            "naming a symbol gives the best results",
        ],
        commands: &["travsr ask --examples"],
    },
    Entry {
        question: "why did it say it found nothing?",
        lead: "It abstains rather than returning a low-confidence guess.",
        points: &[
            "an answer you cannot trust is worse than none",
            "conceptual questions naming no symbol are a known gap",
            "the abstention suggests what to try instead",
        ],
        commands: &[],
    },
    Entry {
        question: "where is the data, and how do I remove it?",
        lead: "Two places, both safe to delete.",
        points: &[
            ".travsr/    per-repo graph, inside the repo",
            "~/.travsr   shared binaries and models",
            "the graph is derived from your source and rebuilds",
        ],
        commands: &["travsr init"],
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

    #[test]
    fn every_command_is_runnable() {
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
            for c in e.commands {
                // Only travsr subcommands are checkable here; the installer
                // pipeline and the cargo build are not travsr invocations.
                if !c.starts_with("travsr ") {
                    continue;
                }
                let sub = c.split_whitespace().nth(1).unwrap_or("");
                assert!(
                    known.iter().any(|k| k == sub),
                    "`{c}` names unknown subcommand {sub:?}"
                );
            }
        }
    }

    #[test]
    fn wrapping_preserves_every_word() {
        for e in ENTRIES {
            let joined = wrap(e.lead, 76).join(" ");
            let before: Vec<&str> = e.lead.split_whitespace().collect();
            let after: Vec<&str> = joined.split_whitespace().collect();
            assert_eq!(before, after, "wrapping altered `{}`", e.question);
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
            for line in wrap(e.lead, 76) {
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
