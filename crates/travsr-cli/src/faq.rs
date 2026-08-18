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

use std::io::IsTerminal as _;

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
    /// A command that demonstrates or acts on the answer. Empty when there is
    /// nothing to run, rather than inventing one.
    pub command: &'static str,
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
        command: "",
    },
    Entry {
        question: "how is it different from search or vector RAG?",
        lead: "It computes relationships instead of approximating them.",
        points: &[
            "vector search chunks code and retrieves by similarity",
            "travsr parses the code and resolves each call to its definition",
            "\"what calls this\" is derived, not inferred",
        ],
        command: "",
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
        command: "",
    },
    Entry {
        question: "how do I install it?",
        lead: "One line. The installer verifies the release signature first.",
        points: &["npm and building from source also work"],
        command: "curl -fsSL https://travsr.com/install.sh | sh",
    },
    Entry {
        question: "how do I start using it on a repo?",
        lead: "Run this from the repo root.",
        points: &[
            "indexes the tracked files",
            "installs a git hook so the graph stays current",
        ],
        command: "travsr init --semantic",
    },
    Entry {
        question: "which languages does it support?",
        lead: "Sixteen for structure; four resolve calls without setup.",
        points: &[
            "native   rust, typescript, javascript, python",
            "others need an analyzer installed",
        ],
        command: "travsr lang list",
    },
    Entry {
        question: "does my code leave my machine?",
        lead: "No. Indexing, storage and queries are all local.",
        points: &[
            "the graph lives in .travsr/ inside your repo",
            "embedding models run as a local sidecar",
            "network is only for downloading travsr and optional models",
        ],
        command: "",
    },
    Entry {
        question: "do I need the daemon running?",
        lead: "No, but it helps.",
        points: &[
            "queries work without it, served from the database",
            "the daemon keeps semantic analysis current as files change",
            "and answers faster from a warm store",
        ],
        command: "travsr daemon start",
    },
    Entry {
        question: "how do I connect it to an AI agent?",
        lead: "Travsr speaks MCP, so any agent that supports it can query the graph.",
        points: &[
            "detects installed tools and writes their config",
            "add --print first to see what would change",
        ],
        command: "travsr connect",
    },
    Entry {
        question: "what can I ask about my code?",
        lead: "Structural questions, anchored to a symbol name.",
        points: &[
            "what calls it, what it depends on, where it is used",
            "naming a symbol gives the best results",
        ],
        command: "travsr ask --examples",
    },
    Entry {
        question: "why did it say it found nothing?",
        lead: "It abstains rather than returning a low-confidence guess.",
        points: &[
            "an answer you cannot trust is worse than none",
            "conceptual questions naming no symbol are a known gap",
            "the abstention suggests what to try instead",
        ],
        command: "",
    },
    Entry {
        question: "where is the data, and how do I remove it?",
        lead: "Two places, both safe to delete.",
        points: &[
            ".travsr/    per-repo graph, inside the repo",
            "~/.travsr   shared binaries and models",
            "the graph is derived from your source and rebuilds",
        ],
        command: "travsr init",
    },
];

/// The FAQ entry whose question is exactly `question`.
///
/// Exact rather than fuzzy: the caller already decided which entry applies, by
/// matching the phrase the user typed. An earlier version re-derived it by
/// substring, which silently failed whenever the user's wording differed from
/// the catalogue's ("how do i install travsr" does not contain "how do I install
/// it"), and fell back to naming another command instead of answering.
pub(crate) fn entry(question: &str) -> Option<&'static Entry> {
    ENTRIES.iter().find(|e| e.question == question)
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
    if !e.command.is_empty() {
        println!();
        println!("    {} {}", pal.dim("$"), pal.ident(e.command));
    }
}

/// Print the FAQ.
pub fn run() {
    let pal = Palette::for_stream(std::io::stdout().is_terminal());

    println!("{}", pal.bold("Travsr FAQ"));
    println!();

    for e in ENTRIES {
        println!("{}", pal.orange(e.question));
        print_entry(e, pal);
        println!();
    }

    println!(
        "{}",
        pal.dim("`travsr ask --examples` lists questions about your indexed code.")
    );
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
            if e.command.is_empty() || !e.command.starts_with("travsr ") {
                continue; // the installer one-liner is a shell pipeline, not a subcommand
            }
            let sub = e.command.split_whitespace().nth(1).unwrap_or("");
            assert!(
                known.iter().any(|k| k == sub),
                "`{}` names unknown subcommand {sub:?}",
                e.command
            );
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
