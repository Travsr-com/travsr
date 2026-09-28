//! Recognising a read-only search command inside a `Bash` tool call (#916).
//!
//! The guard has to answer one question about a shell command: *is this whole
//! command line nothing but a single invocation of `grep`, `rg`, `find`, `ag`,
//! `ack`, or `ls -R`?* Anything else (a pipeline, a `&&` chain, a command
//! substitution, a redirect) is left alone.
//!
//! "Whole command line" is not fussiness. The guard's `allow` is the host's
//! *auto-approve*, not merely "do not block", so answering yes to
//! `grep foo && rm -rf build` would spend the user's permission prompt on the
//! `rm`. Requiring a single simple command makes that unreachable, and it
//! happens to be the same rule that keeps `find . | xargs sed -i` out of the
//! match set, which nobody would want nudged toward a graph query either.
//!
//! Substring matching is not used anywhere here. `programgrep` and
//! `my-rg-wrapper` are different programs that merely spell a tool's name
//! inside their own; they are matched against the resolved argv[0] basename,
//! whole, so neither is a hit.

/// A search tool the guard knows how to redirect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTool {
    /// Content search: `grep`, `rg`, `ag`, `ack`. Carries a pattern.
    Content,
    /// File discovery: `find`, `ls -R`. Carries a name fragment at best.
    Files,
}

/// A `Bash` command line the guard recognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCommand {
    /// Which program, by basename, e.g. `rg`.
    pub program: String,
    pub kind: SearchTool,
    /// The search term: the pattern for a content search, the `-name` argument
    /// for `find`. `None` when the command has no recoverable literal term.
    pub term: Option<String>,
}

/// Programs whose whole job is content search.
const CONTENT_TOOLS: [&str; 4] = ["grep", "rg", "ag", "ack"];

/// Split a command line into words the way a POSIX shell would, but refuse
/// rather than guess whenever a construct could introduce a second command.
///
/// Returns `None` for: any of `| & ; ( ) < > ` $ \n`, an unterminated quote, or
/// a backslash escape outside quotes. Refusing on `$` costs the guard a few
/// legitimate `grep "$pat" .` calls, which then take the normal permission
/// flow, which is the right way to be wrong, since the alternative is reasoning about
/// a value this process cannot see.
fn simple_words(command: &str) -> Option<Vec<String>> {
    // Cheap pre-filter for the operators, before any per-character work. `!`
    // is here for history expansion, `~` deliberately is not: tilde expansion
    // produces a path, never a command.
    const FORBIDDEN: [char; 12] = ['|', '&', ';', '(', ')', '<', '>', '`', '$', '\n', '\r', '!'];
    if command.chars().any(|c| FORBIDDEN.contains(&c)) {
        return None;
    }

    let mut words = Vec::new();
    let mut current = String::new();
    let mut has_word = false;
    let mut quote: Option<char> = None;

    for c in command.chars() {
        match (quote, c) {
            // Inside quotes everything is literal until the matching quote.
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => current.push(c),
            // A backslash outside quotes escapes the next character, which
            // includes escaping a newline into a line continuation. Refuse
            // rather than re-implement that; a search command rarely needs it.
            (None, '\\') => return None,
            (None, '\'') | (None, '"') => {
                quote = Some(c);
                // An empty quoted word is still a word: `grep "" .` passes an
                // empty pattern, and dropping it would shift every later
                // argument left by one.
                has_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if has_word {
                    words.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            (None, c) => {
                current.push(c);
                has_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if has_word {
        words.push(current);
    }
    (!words.is_empty()).then_some(words)
}

/// The program name from an argv[0], with any directory and a Windows `.exe`
/// suffix removed. `/usr/bin/grep` and `C:\tools\rg.exe` are the same programs
/// as `grep` and `rg`; `my-rg-wrapper` is not.
fn basename(arg0: &str) -> String {
    let cut = arg0
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(arg0)
        .to_ascii_lowercase();
    cut.strip_suffix(".exe").unwrap_or(&cut).to_string()
}

/// Whether `word` is an option rather than an operand.
fn is_flag(word: &str) -> bool {
    word.starts_with('-') && word != "-"
}

/// Classify a `Bash` command line, or `None` when it is not a single read-only
/// search invocation.
pub fn classify(command: &str) -> Option<SearchCommand> {
    let words = simple_words(command)?;
    let mut words = words.into_iter();
    let program = basename(&words.next()?);
    let args: Vec<String> = words.collect();

    if CONTENT_TOOLS.contains(&program.as_str()) {
        return Some(SearchCommand {
            term: content_term(&program, &args),
            program,
            kind: SearchTool::Content,
        });
    }
    if program == "find" {
        return Some(SearchCommand {
            program,
            kind: SearchTool::Files,
            term: find_name_term(&args),
        });
    }
    // Only recursive `ls` is in the match set: a plain `ls src/` is a directory
    // listing, not a search, and nudging it toward the graph would be noise.
    // `-R` may ride in a cluster (`ls -lR`), which is why this is a character
    // scan of each short flag rather than an equality test.
    if program == "ls" && args.iter().any(|a| short_flag_has(a, 'R')) {
        return Some(SearchCommand {
            program,
            kind: SearchTool::Files,
            term: None,
        });
    }
    None
}

/// Whether `word` is a short-flag cluster containing `want` (`-lR` has `R`).
/// Long options (`--recursive`) are excluded deliberately: `--Recursive` is not
/// a thing, and scanning their letters would match `-R` inside `--reverse`.
fn short_flag_has(word: &str, want: char) -> bool {
    match word.strip_prefix('-') {
        Some(rest) if !rest.starts_with('-') && !rest.is_empty() => rest.contains(want),
        _ => false,
    }
}

/// The pattern argument of a content search.
///
/// The first non-flag operand, with two corrections. `grep -e PAT` and
/// `rg --regexp PAT` put the pattern behind a flag that takes a value, so the
/// separated forms are read directly; and an option that takes a value must not
/// have its value mistaken for the pattern, which is what the skip list is for.
/// `None` when the pattern cannot be located, which the caller treats as "not
/// graph-answerable" rather than guessing.
fn content_term(program: &str, args: &[String]) -> Option<String> {
    // Options that consume the following word. Conservative and shared across
    // the four tools: a flag listed here that a given tool does not take costs
    // at most a missed redirect, while one omitted would read a file name or a
    // number as the pattern.
    const TAKES_VALUE: [&str; 16] = [
        "-f",
        "-m",
        "-A",
        "-B",
        "-C",
        "-d",
        "-D",
        "--file",
        "--max-count",
        "--after-context",
        "--before-context",
        "--context",
        "--include",
        "--exclude",
        "--exclude-dir",
        "-g",
    ];
    const PATTERN_FLAGS: [&str; 4] = ["-e", "--regexp", "--pattern", "-p"];

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        // `--` ends the options; the next word is the pattern.
        if a == "--" {
            return args.get(i + 1).cloned().filter(|s| !s.is_empty());
        }
        if let Some((flag, value)) = a.split_once('=') {
            if PATTERN_FLAGS.contains(&flag) {
                return (!value.is_empty()).then(|| value.to_string());
            }
            if is_flag(a) {
                i += 1;
                continue;
            }
        }
        if PATTERN_FLAGS.contains(&a.as_str()) {
            return args.get(i + 1).cloned().filter(|s| !s.is_empty());
        }
        if TAKES_VALUE.contains(&a.as_str()) {
            i += 2;
            continue;
        }
        if is_flag(a) {
            i += 1;
            continue;
        }
        // `ack` and `ag` take the pattern first, same as `grep` and `rg`.
        let _ = program;
        return (!a.is_empty()).then(|| a.clone());
    }
    None
}

/// The literal argument of `find`'s `-name` / `-iname` / `-path`, stripped of
/// its glob characters. `find . -name "PaymentService.java"` is asking where a
/// file is, which is a question the graph has an answer to; `find . -type f` is
/// not, and yields `None`.
fn find_name_term(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if matches!(
            args[i].as_str(),
            "-name" | "-iname" | "-path" | "-ipath" | "-wholename"
        ) {
            let raw = args.get(i + 1)?;
            let trimmed = raw.trim_matches(|c| c == '*' || c == '?');
            return (!trimmed.is_empty()).then(|| trimmed.to_string());
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(cmd: &str) -> Option<SearchTool> {
        classify(cmd).map(|c| c.kind)
    }

    fn term(cmd: &str) -> Option<String> {
        classify(cmd).and_then(|c| c.term)
    }

    #[test]
    fn the_whole_match_set_is_recognised() {
        assert_eq!(kind("grep -rn needle src/"), Some(SearchTool::Content));
        assert_eq!(kind("rg needle"), Some(SearchTool::Content));
        assert_eq!(kind("ag needle"), Some(SearchTool::Content));
        assert_eq!(kind("ack needle"), Some(SearchTool::Content));
        assert_eq!(kind("find . -name '*.rs'"), Some(SearchTool::Files));
        assert_eq!(kind("ls -R"), Some(SearchTool::Files));
        assert_eq!(kind("ls -lR src"), Some(SearchTool::Files));
    }

    #[test]
    fn unrelated_commands_are_not_matched() {
        for cmd in [
            "cargo test",
            "ls src/",
            "ls -l",
            "git log --oneline",
            "npm run build",
            "cat README.md",
            "sed -n 1,20p file.rs",
        ] {
            assert_eq!(classify(cmd), None, "{cmd} must not match");
        }
    }

    /// The false positives the issue names by name. Both spell a tool's name
    /// inside their own; neither is that tool.
    #[test]
    fn a_program_whose_name_merely_contains_a_tool_name_is_not_matched() {
        assert_eq!(classify("programgrep foo"), None);
        assert_eq!(classify("my-rg-wrapper needle"), None);
        assert_eq!(classify("grepper x"), None);
        assert_eq!(classify("findutils-helper"), None);
        assert_eq!(classify("ripgrep needle"), None, "not the `rg` binary");
    }

    /// The property that makes an explicit `allow` safe to emit: a command with
    /// anything else in it is never recognised, so it is never auto-approved.
    #[test]
    fn a_compound_command_is_never_a_match() {
        for cmd in [
            "grep -rn foo . && rm -rf build",
            "grep foo . ; curl evil.example",
            "grep foo . | xargs rm",
            "rg needle > out.txt",
            "rg needle 2>/dev/null",
            "find . -name '*.rs' -delete | sh",
            "grep `whoami` .",
            "grep $(cat /etc/passwd) .",
            "grep \"$PATTERN\" .",
            "(grep foo .)",
            "grep foo . & ",
        ] {
            assert_eq!(classify(cmd), None, "{cmd} must not be auto-approved");
        }
    }

    #[test]
    fn an_unterminated_quote_is_refused() {
        assert_eq!(classify("grep 'unterminated"), None);
        assert_eq!(classify("rg \"half"), None);
    }

    #[test]
    fn an_absolute_path_resolves_to_its_basename() {
        assert_eq!(kind("/usr/bin/grep foo ."), Some(SearchTool::Content));
        assert_eq!(kind("C:\\tools\\rg.exe foo"), None, "backslash is refused");
        assert_eq!(kind("C:/tools/rg.exe foo"), Some(SearchTool::Content));
    }

    #[test]
    fn the_pattern_is_recovered_past_flags() {
        assert_eq!(term("grep -rn needle src/"), Some("needle".into()));
        assert_eq!(term("rg --hidden needle"), Some("needle".into()));
        assert_eq!(term("grep -e needle src/"), Some("needle".into()));
        assert_eq!(term("rg --regexp=needle src/"), Some("needle".into()));
        assert_eq!(term("grep -- needle src/"), Some("needle".into()));
        assert_eq!(
            term("grep 'quoted needle' src/"),
            Some("quoted needle".into())
        );
    }

    /// A value-taking flag's operand is not the pattern. `-A 3` used to make
    /// `3` the search term, which then looked up a symbol named "3".
    #[test]
    fn a_flags_value_is_not_mistaken_for_the_pattern() {
        assert_eq!(term("grep -A 3 needle src/"), Some("needle".into()));
        assert_eq!(term("grep -m 5 needle"), Some("needle".into()));
        assert_eq!(term("rg -g '*.rs' needle"), Some("needle".into()));
        assert_eq!(
            term("grep --include=*.rs needle src"),
            Some("needle".into())
        );
    }

    #[test]
    fn find_yields_its_name_argument_without_globs() {
        assert_eq!(
            term("find . -name 'PaymentService.java'"),
            Some("PaymentService.java".into())
        );
        assert_eq!(term("find . -name '*.rs'"), Some(".rs".into()));
        assert_eq!(term("find . -type f"), None);
        assert_eq!(term("ls -R"), None);
    }

    #[test]
    fn an_empty_or_blank_command_is_not_a_match() {
        assert_eq!(classify(""), None);
        assert_eq!(classify("   "), None);
    }

    #[test]
    fn an_empty_quoted_pattern_does_not_shift_the_operands() {
        // `grep "" src/` searches for the empty string in src/. The empty word
        // must survive tokenisation or `src/` would be read as the pattern.
        assert_eq!(simple_words("grep \"\" src/").unwrap().len(), 3);
        assert_eq!(term("grep \"\" src/"), None, "an empty pattern is no term");
    }

    #[test]
    fn short_flag_clusters_are_read_per_character() {
        assert!(short_flag_has("-lR", 'R'));
        assert!(short_flag_has("-R", 'R'));
        assert!(!short_flag_has("--reverse", 'R'));
        assert!(!short_flag_has("-l", 'R'));
        assert!(!short_flag_has("plain", 'R'));
    }
}
