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
    pub question: String,
    /// One sentence. The answer, if the reader stops here.
    pub lead: String,
    /// The fuller answer: the reasoning, caveat or consequence behind the
    /// points. Points say what is true and are quick to scan; on their own they
    /// read as a list of assertions with nothing joining them, which is the
    /// feedback this field exists to answer. Prose carries the why.
    pub detail: String,
    /// Supporting points, each short enough to sit on one line.
    pub points: Vec<String>,
    /// An ordered walkthrough, as (what this does, command) pairs. Used where
    /// the answer is a sequence rather than a fact: a list of commands printed
    /// as a block loses the order, and "install" is only useful as the whole
    /// path from nothing to a working query.
    pub steps: Vec<(String, String)>,
    /// Commands that act on the answer. Empty when there is nothing to run,
    /// rather than inventing one. A slice because some answers genuinely have
    /// several: install has three real routes, and naming one while mentioning
    /// the others in prose makes the reader go looking for them.
    pub commands: Vec<String>,
}

/// The catalogue, as written by a human in `faq.txt`.
///
/// A data file rather than Rust source. Adding a question used to mean editing a
/// table of struct literals with escaped quotes in it, which is enough friction
/// that answers do not get added. The file is embedded at compile time, so this
/// stays exactly as cheap as the constants it replaced: no I/O, nothing to ship
/// alongside the binary, and nothing to keep in sync.
const CATALOGUE: &str = include_str!("faq.txt");

/// Parsed catalogue, built once on first use.
fn entries() -> &'static [Entry] {
    static ENTRIES: std::sync::OnceLock<Vec<Entry>> = std::sync::OnceLock::new();
    ENTRIES.get_or_init(|| parse(CATALOGUE))
}

/// Read the catalogue format.
///
/// Line-led, so each kind of content is recognisable on sight and a malformed
/// line cannot silently change the meaning of the one after it. Blank lines and
/// `#` comments are ignored; a line that matches no marker is detail prose,
/// because that is the part someone writes most freely.
fn parse(src: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut detail: Vec<String> = Vec::new();

    // Detail accumulates across lines, so it is only attached when the entry is
    // known to be finished: at the next question, or at the end of the file.
    fn flush(out: &mut [Entry], detail: &mut Vec<String>) {
        if let Some(e) = out.last_mut() {
            e.detail = std::mem::take(detail).join(" ");
        } else {
            detail.clear();
        }
    }

    for line in src.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') && !t.starts_with("## ") {
            continue;
        }
        if let Some(q) = t.strip_prefix("## ") {
            flush(&mut out, &mut detail);
            out.push(Entry {
                question: q.trim().to_string(),
                lead: String::new(),
                detail: String::new(),
                steps: Vec::new(),
                points: Vec::new(),
                commands: Vec::new(),
            });
            continue;
        }
        let Some(e) = out.last_mut() else {
            // Content before the first `##` has no entry to belong to. Dropping
            // it is right: the alternative is inventing one with no question.
            continue;
        };
        if let Some(lead) = t.strip_prefix("> ") {
            e.lead = lead.trim().to_string();
        } else if let Some(point) = t.strip_prefix("- ") {
            e.points.push(point.trim_end().to_string());
        } else if let Some(cmd) = t.strip_prefix("$ ") {
            e.commands.push(cmd.trim().to_string());
        } else if let Some(step) = numbered(t) {
            // `what it does :: command`, with the command allowed to be empty
            // for a step like "restart the agent" that has nothing to run.
            let (what, cmd) = step.split_once("::").unwrap_or((step, ""));
            e.steps
                .push((what.trim().to_string(), cmd.trim().to_string()));
        } else {
            detail.push(t.to_string());
        }
    }
    flush(&mut out, &mut detail);
    out
}

/// The text after a `N. ` marker, if the line starts with one.
fn numbered(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(|c: char| c.is_ascii_digit());
    if rest.len() == line.len() {
        return None;
    }
    rest.strip_prefix(". ")
}

/// The prefix that says "this is a question about travsr, not about my code".
pub(crate) const NAMESPACE: &str = "travsr:";

/// The question inside an explicitly namespaced query, if it is one.
pub(crate) fn strip_namespace(query: &str) -> Option<&str> {
    let q = query.trim();
    // Accept the spelling with a space before the colon too; someone typing a
    // sentence naturally writes one, and rejecting it would be a puzzle rather
    // than a rule.
    let rest = q
        .strip_prefix(NAMESPACE)
        .or_else(|| q.strip_prefix("travsr :"))?;
    Some(rest.trim())
}

/// Best entry for a question that is already known to be about travsr.
///
/// Deliberately looser than [`match_question`]. That one has to protect a code
/// search from being hijacked, so it demands a question shape and coverage in
/// both directions. Here the reader has said which kind of question this is by
/// typing the prefix, so `travsr: logs` should work even though bare `logs`
/// is a symbol search.
pub(crate) fn match_namespaced(question: &str) -> Option<&'static Entry> {
    let asked = distinctive_words(question);
    if asked.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &'static Entry)> = None;
    for e in entries() {
        let want = distinctive_words(&e.question);
        let hits = want.iter().filter(|w| asked.contains(*w)).count();
        if hits == 0 {
            continue;
        }
        if best.map_or(true, |(n, _)| hits > n) {
            best = Some((hits, e));
        }
    }
    best.map(|(_, e)| e)
}

/// Print the catalogue's questions, for a namespaced query that matched nothing.
pub(crate) fn print_questions(pal: Palette) {
    println!("{}", pal.bold("Questions about travsr"));
    for q in questions() {
        println!("  {q}");
    }
    println!();
    println!(
        "{}",
        pal.dim("Ask one with: travsr ask \"travsr: <question>\"")
    );
}

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
    for e in entries() {
        let want = distinctive_words(&e.question);
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
    for line in wrap(&e.lead, 74) {
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
    for p in &e.points {
        // Only wrap when it does not fit. `wrap` splits on whitespace, so running
        // it over a short point would collapse the runs of spaces some points use
        // to align a label against its description ("Phase A   parses ...").
        if p.chars().count() <= 70 {
            println!("    {} {p}", pal.dim("·"));
            continue;
        }
        let mut lines = wrap(p.as_str(), 70).into_iter();
        if let Some(first) = lines.next() {
            println!("    {} {first}", pal.dim("·"));
        }
        for rest in lines {
            println!("      {rest}");
        }
    }
    if !e.detail.is_empty() {
        println!();
        for line in wrap(&e.detail, 74) {
            println!("  {line}");
        }
    }
    if !e.commands.is_empty() {
        println!();
        for c in &e.commands {
            println!("    {} {}", pal.dim("$"), pal.ident(c.as_str()));
        }
    }
}

/// Every catalogue question, for `ask --examples` to list.
///
/// These are answered by `ask` directly, so they are shown without a command
/// beside them: the question *is* the command.
pub(crate) fn questions() -> impl Iterator<Item = &'static str> {
    entries().iter().map(|e| e.question.as_str())
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
    use super::{entries, wrap};

    #[test]
    fn every_entry_is_a_question_with_an_answer() {
        assert!(!entries().is_empty());
        for e in entries() {
            assert!(
                e.question.ends_with('?'),
                "`{}` is not a question",
                e.question.as_str()
            );
            assert!(
                e.question.chars().next().is_some_and(|c| c.is_lowercase()),
                "`{}` should read as spoken text",
                e.question.as_str()
            );
            assert!(!e.lead.is_empty(), "`{}` has no lead", e.question.as_str());
            // A lead that runs long is a paragraph again, which is the format
            // this structure replaced. One sentence, readable at a glance.
            assert!(
                e.lead.chars().count() <= 90,
                "`{}` has a {}-char lead; that is a paragraph, not a lead",
                e.question.as_str(),
                e.lead.chars().count()
            );
        }
    }

    /// The catalogue is a data file now, so a parse that silently loses content
    /// is the new failure mode: a mistyped marker turns a point into prose, or a
    /// block into nothing at all, and every other test would still pass on what
    /// survived. Pin the count and the shape of what came out.
    #[test]
    fn the_catalogue_file_parses_completely() {
        let questions = super::CATALOGUE
            .lines()
            .filter(|l| l.starts_with("## "))
            .count();
        assert!(questions > 0, "the catalogue file has no questions in it");
        assert_eq!(
            entries().len(),
            questions,
            "faq.txt has {questions} questions but {} parsed",
            entries().len()
        );
        for e in entries() {
            assert!(
                !e.lead.is_empty(),
                "`{}` parsed with no lead; check its `>` marker",
                e.question
            );
            assert!(
                !e.points.is_empty(),
                "`{}` parsed with no points; check its `-` markers",
                e.question
            );
        }
    }

    /// Points state facts; on their own they read as a list of assertions with
    /// nothing joining them. Every answer owes the reader the reasoning behind
    /// them, so the prose is required rather than optional, and long enough to
    /// be an explanation rather than one more restated point.
    #[test]
    fn every_entry_explains_itself() {
        for e in entries() {
            let n = e.detail.chars().count();
            assert!(
                n >= 140,
                "`{}` has a {n}-char detail; that is another point, not an explanation",
                e.question.as_str()
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
        for e in entries() {
            corpus.push_str(&e.question);
            corpus.push(' ');
            corpus.push_str(&e.lead);
            corpus.push(' ');
            corpus.push_str(&e.detail);
            corpus.push(' ');
            for p in &e.points {
                corpus.push_str(p);
                corpus.push(' ');
            }
            for (w, c) in &e.steps {
                corpus.push_str(w);
                corpus.push(' ');
                corpus.push_str(c);
                corpus.push(' ');
            }
            for c in &e.commands {
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
        for e in entries() {
            for p in &e.points {
                let n = p.chars().count();
                assert!(
                    n <= 70,
                    "`{}` has a {n}-char point; it would wrap and lose alignment: {p}",
                    e.question.as_str()
                );
            }
        }
    }

    /// Every catalogue question must be reachable by asking it. The point of
    /// matching the questions themselves is that a new entry works from `ask`
    /// the moment it is written, with no second list to update.
    #[test]
    fn every_catalogue_question_matches_itself() {
        for e in entries() {
            let got = super::match_question(&e.question)
                .unwrap_or_else(|| panic!("`{}` does not match itself", e.question.as_str()));
            assert_eq!(got.question, e.question.as_str());
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

        for e in entries() {
            for (what, c) in &e.steps {
                assert!(
                    !what.is_empty(),
                    "`{}` has a step with no description",
                    e.question.as_str()
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

        for e in entries() {
            for c in &e.commands {
                check(c.as_str(), &e.question);
            }
            for (_, c) in &e.steps {
                check(c.as_str(), &e.question);
            }
        }
    }

    #[test]
    fn wrapping_preserves_every_word() {
        for e in entries() {
            for text in [e.lead.as_str(), e.detail.as_str()] {
                let joined = wrap(text, 76).join(" ");
                let before: Vec<&str> = text.split_whitespace().collect();
                let after: Vec<&str> = joined.split_whitespace().collect();
                assert_eq!(before, after, "wrapping altered `{}`", e.question.as_str());
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
        for e in entries() {
            for line in wrap(&e.lead, 76).into_iter().chain(wrap(&e.detail, 76)) {
                let n = line.chars().count();
                // Only a single over-long word may exceed it.
                assert!(
                    n <= 76 || !line.contains(' '),
                    "`{}` produced a {n}-column line: {line}",
                    e.question.as_str()
                );
            }
        }
    }
}
