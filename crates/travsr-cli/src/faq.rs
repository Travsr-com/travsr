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

/// One question and its answer. Answers are wrapped at print time, so they are
/// written here as single paragraphs.
pub(crate) struct Entry {
    pub question: &'static str,
    pub answer: &'static str,
    /// A command that demonstrates or acts on the answer. Empty when there is
    /// nothing to run, rather than inventing one.
    pub command: &'static str,
}

const ENTRIES: &[Entry] = &[
    Entry {
        question: "what is travsr?",
        answer: "A code intelligence daemon that builds a graph of your codebase and keeps \
                 it current with git. Every function, class and call is a node or an edge, so \
                 questions about structure have exact answers instead of guesses.",
        command: "",
    },
    Entry {
        question: "how is it different from search or vector RAG?",
        answer: "Vector search chunks code into passages and retrieves by similarity, which \
                 approximates relationships that are already exact. Travsr computes them: a \
                 parser produces the structure and a resolver decides what each call refers to. \
                 Ask what calls a function and the answer is derived, not inferred.",
        command: "",
    },
    Entry {
        question: "how does it work?",
        answer: "Two passes. Phase A parses every tracked file with tree-sitter into nodes and \
                 definition edges. Phase B resolves calls to specific definitions, natively for \
                 Rust, TypeScript and Python and via a sandboxed analyzer for other languages. \
                 Retrieval then seeds from your query, walks the graph with personalized \
                 PageRank, reranks, and packs the result into your token budget.",
        command: "",
    },
    Entry {
        question: "how do I install it?",
        answer: "The installer verifies the release signature before installing. npm and \
                 building from source both work too.",
        command: "curl -fsSL https://travsr.com/install.sh | sh",
    },
    Entry {
        question: "how do I start using it on a repo?",
        answer: "Run this from the repo root. It indexes the tracked files and installs a git \
                 hook so the graph stays current as you commit.",
        command: "travsr init --semantic",
    },
    Entry {
        question: "which languages does it support?",
        answer: "Sixteen for structure. Call resolution is native for Rust, TypeScript, \
                 JavaScript and Python, and needs an installed analyzer for the rest. This \
                 command shows which are active for your repo.",
        command: "travsr lang list",
    },
    Entry {
        question: "does my code leave my machine?",
        answer: "No. Indexing, storage and queries are all local. The graph lives in .travsr/ \
                 inside your repo, and embedding models run as a local sidecar. Network access \
                 is only for downloading travsr itself and optional models.",
        command: "",
    },
    Entry {
        question: "do I need the daemon running?",
        answer: "No. Queries work without it, served from the database directly. The daemon \
                 keeps semantic analysis current as files change and answers faster from a warm \
                 store, so it is worth starting for day-to-day use.",
        command: "travsr daemon start",
    },
    Entry {
        question: "how do I connect it to an AI agent?",
        answer: "Travsr speaks MCP, so any agent that supports it can query the graph. This \
                 detects installed tools and writes their config; add --print first to see what \
                 would change.",
        command: "travsr connect",
    },
    Entry {
        question: "what can I ask about my code?",
        answer: "Structural questions have exact answers: what calls this, what does it depend \
                 on, where is it used. Anchor a question to a symbol name for the best results.",
        command: "travsr ask --examples",
    },
    Entry {
        question: "why did it say it found nothing?",
        answer: "Travsr abstains rather than returning a low-confidence guess, because an \
                 answer you cannot trust is worse than none. Conceptual questions that name no \
                 symbol are the common case, and are a known gap being worked on. The \
                 abstention suggests what to try instead.",
        command: "",
    },
    Entry {
        question: "where is the data, and how do I remove it?",
        answer: "Per-repo state is .travsr/ in the repo. Shared binaries and models are in \
                 ~/.travsr. Deleting either is safe: the graph is derived from your source and \
                 rebuilds with travsr init.",
        command: "",
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

/// Print one entry, wrapped, with its command when it has one.
pub(crate) fn print_entry(e: &Entry, pal: Palette) {
    for line in wrap(e.answer, 76) {
        println!("{line}");
    }
    if !e.command.is_empty() {
        println!("  {} {}", pal.dim("$"), pal.ident(e.command));
    }
}

/// Print the FAQ.
pub fn run() {
    let pal = Palette::for_stream(std::io::stdout().is_terminal());

    println!("{}", pal.bold("Travsr FAQ"));
    println!();

    for e in ENTRIES {
        println!("{}", pal.orange(e.question));
        for line in wrap(e.answer, 76) {
            println!("  {line}");
        }
        if !e.command.is_empty() {
            println!("  {} {}", pal.dim("$"), pal.ident(e.command));
        }
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
            assert!(!e.answer.is_empty(), "`{}` has no answer", e.question);
        }
    }

    /// A command that does not exist sends someone to a dead end while looking
    /// authoritative, which is what #727 was. Read the subcommand list from clap
    /// rather than keeping a copy here.
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
            let joined = wrap(e.answer, 76).join(" ");
            let before: Vec<&str> = e.answer.split_whitespace().collect();
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
            for line in wrap(e.answer, 76) {
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
