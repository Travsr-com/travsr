//! Live progress UI for `travsr init` (issue #293).
//!
//! A large repo can take many minutes to index; with no output the command is
//! indistinguishable from a hang. This renders progress to **stderr** (stdout
//! stays clean for the final summary), adapting to context:
//!
//! - **TTY**: a single self-updating line — a pulsing graph-node spinner, an
//!   eighth-precision bar, `done/total`, percent, and elapsed time.
//!   Brand orange while working; the summary that follows is plain text.
//! - **Non-TTY** (pipe/CI): occasional newline-terminated lines, no control
//!   chars or color.
//! - **`--json`**: one JSON object per (throttled) event on stderr.
//! - **`--quiet`**: nothing.
//!
//! Color follows the Travsr design foundation (orange `#fb923c` hot/in-progress,
//! green `#86df86` fresh) and is gated on a TTY plus `NO_COLOR`/`CLICOLOR_FORCE`.
//! Status is always icon + text, never color alone, so it degrades cleanly.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use travsr_daemon::InitProgress;

/// Pulsing graph-node spinner frames (on-brand: "nodes pulse").
const NODE: [char; 4] = ['◐', '◓', '◑', '◒'];
/// Sub-cell bar fragments for 1/8..7/8 of a cell (index 0 unused).
const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
/// Progress-bar width in cells (kept modest so the line fits ~74 cols).
const BAR_W: usize = 20;
/// Minimum gap between TTY repaints (caps refresh at ~10/s).
const TTY_TICK: Duration = Duration::from_millis(100);
/// Cadence for non-TTY / JSON lines so logs stay readable.
const LINE_TICK: Duration = Duration::from_secs(2);

/// Brand color helper. When disabled, every method returns the text unchanged,
/// so the UI degrades to plain glyphs (icon + text carry the meaning).
#[derive(Clone, Copy)]
pub struct Palette {
    color: bool,
}

impl Palette {
    /// Enable color when the target stream is a TTY and not suppressed, or when
    /// `CLICOLOR_FORCE` is set. `NO_COLOR` always wins (https://no-color.org).
    pub fn for_stream(is_tty: bool) -> Self {
        let color = if std::env::var_os("NO_COLOR").is_some() {
            false
        } else if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
            true
        } else {
            is_tty && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
        };
        Self { color }
    }

    /// Whether color/ANSI output is enabled for this stream, per the canonical
    /// gate (`NO_COLOR` / `CLICOLOR_FORCE` / `TERM=dumb`). Lets other surfaces
    /// (e.g. the `--help` logo) reuse the same decision instead of re-deriving it.
    pub fn enabled(self) -> bool {
        self.color
    }

    fn paint(self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// Orange `#fb923c` — hot / in-progress (`--color-stale`/`--color-edge-hot`).
    pub fn orange(self, s: &str) -> String {
        self.paint("38;2;251;146;60", s)
    }
    /// Fresh green `#86df86` — done / fresh node (`--color-fresh`).
    pub fn green(self, s: &str) -> String {
        self.paint("38;2;134;223;134", s)
    }
    /// Empty bar track — charcoal `#4d4d4d` (`--color-border`).
    fn track(self, s: &str) -> String {
        self.paint("38;2;77;77;77", s)
    }
    /// Muted secondary text (elapsed/hints).
    pub fn dim(self, s: &str) -> String {
        self.paint("2", s)
    }
    /// Bold — the wordmark.
    pub fn bold(self, s: &str) -> String {
        self.paint("1", s)
    }
    /// Cyan — an identifier the reader is expected to swap for their own.
    ///
    /// Basic ANSI (36) rather than the truecolor the brand hues use, deliberately:
    /// this one has to stay legible against a light terminal background as well as
    /// a dark one, and the 16-colour codes are remapped by the user's own theme.
    /// A fixed hex that reads well on charcoal can be near-invisible on white.
    pub fn ident(self, s: &str) -> String {
        self.paint("36", s)
    }
}

/// Brand banner shown at the top of `travsr --help`: the graph-node motif (one
/// node fanning to its callers/dependents) plus the `travsr` wordmark, in brand
/// orange on a TTY (plain when piped, respects `NO_COLOR`).
///
/// This is a terminal-appropriate evocation of the brand, not the official logo
/// asset — that lives in `design/logo/` and must not be hand-recreated.
pub fn banner() -> String {
    let p = Palette::for_stream(std::io::stdout().is_terminal());
    let n = p.orange("●"); // center node — alive
    let s = p.track("◍"); // satellite nodes
    let e = p.track("─");
    let tl = p.track("╭");
    let bl = p.track("╰");
    format!(
        "\n   {tl}{e}{s}\n   {n}{e}{s}   {}\n   {bl}{e}{s}",
        p.bold("travsr")
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Tty,
    Plain,
    Json,
    Quiet,
}

/// Renders [`InitProgress`] events. Construct once, call [`update`] per event,
/// [`finish`] to clear the live line, then print the summary via
/// [`print_summary`].
///
/// [`update`]: ProgressReporter::update
/// [`finish`]: ProgressReporter::finish
pub struct ProgressReporter {
    mode: Mode,
    palette: Palette,
    start: Instant,
    last_paint: Instant,
    spin: usize,
    last_width: usize,
}

impl ProgressReporter {
    /// Pick a mode from the flags and whether stderr is a terminal.
    /// `--quiet` wins over `--json`.
    pub fn new(quiet: bool, json: bool) -> Self {
        let is_tty = std::io::stderr().is_terminal();
        let mode = if quiet {
            Mode::Quiet
        } else if json {
            Mode::Json
        } else if is_tty {
            Mode::Tty
        } else {
            Mode::Plain
        };
        let now = Instant::now();
        Self {
            mode,
            palette: Palette::for_stream(is_tty),
            start: now,
            // Offset so the first non-TTY / JSON event paints immediately.
            last_paint: now - LINE_TICK,
            spin: 0,
            last_width: 0,
        }
    }

    /// Wall-clock time since construction (used for the final summary).
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Handle one progress event (throttled internally).
    pub fn update(&mut self, p: InitProgress) {
        match self.mode {
            Mode::Quiet => {}
            Mode::Tty => self.render_tty(p),
            Mode::Plain => self.render_line(p, false),
            Mode::Json => self.render_line(p, true),
        }
    }

    /// Clear the in-place TTY line so the caller's stdout summary prints cleanly.
    /// No-op in the other modes.
    pub fn finish(&mut self) {
        if self.mode == Mode::Tty && self.last_width > 0 {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r{}\r", " ".repeat(self.last_width));
            let _ = err.flush();
            self.last_width = 0;
        }
    }

    fn render_tty(&mut self, p: InitProgress) {
        let now = Instant::now();
        if now.duration_since(self.last_paint) < TTY_TICK {
            return;
        }
        self.last_paint = now;
        self.spin = (self.spin + 1) % NODE.len();
        let spinner = self.palette.orange(&NODE[self.spin].to_string());
        let line = self.compose(&spinner, p);

        let mut err = std::io::stderr().lock();
        let width = visible_width(&line);
        let pad = self.last_width.saturating_sub(width);
        let _ = write!(err, "\r{}{}", line, " ".repeat(pad));
        let _ = err.flush();
        self.last_width = width;
    }

    fn render_line(&mut self, p: InitProgress, json: bool) {
        let now = Instant::now();
        if now.duration_since(self.last_paint) < LINE_TICK {
            return;
        }
        self.last_paint = now;
        let line = if json {
            self.describe_json(p)
        } else {
            // Plain, color-free, no spinner — safe for CI logs.
            format!("travsr: {}", self.describe_plain(p))
        };
        let _ = writeln!(std::io::stderr(), "{line}");
    }

    /// Styled one-liner for the TTY (spinner already rendered by the caller).
    fn compose(&self, spinner: &str, p: InitProgress) -> String {
        let pal = self.palette;
        let elapsed = fmt_dur(self.start.elapsed());
        match p {
            InitProgress::Scanning { scanned } => {
                format!(
                    "  {spinner} scanning  {} files   {}",
                    commas(scanned),
                    pal.dim(&elapsed)
                )
            }
            InitProgress::Indexing { done, total, .. } => {
                let pct = (done * 100).checked_div(total).unwrap_or(0);
                format!(
                    "  {spinner} reading files  {}  {}/{}  {pct}%   {}",
                    bar(pal, pct),
                    commas(done),
                    commas(total),
                    pal.dim(&elapsed)
                )
            }
            InitProgress::Saving => {
                format!("  {spinner} making it searchable   {}", pal.dim(&elapsed))
            }
            InitProgress::Finalizing => {
                format!("  {spinner} tracing calls   {}", pal.dim(&elapsed))
            }
            InitProgress::SemanticRunning { langs, budget_secs } => {
                format!(
                    "  {spinner} tracing calls  {}   {}",
                    semantic_langs_cell(&langs),
                    pal.dim(&semantic_tail(&langs, budget_secs, &elapsed))
                )
            }
            InitProgress::PhaseBDeferred => {
                // Transient line — do not assert *when* semantic edges build (that
                // depends on whether a daemon is running, which init decides after
                // this pass). print_summary states it accurately; here just report
                // that the structural pass is done.
                format!("  {} files read   {}", pal.green("●"), pal.dim(&elapsed))
            }
        }
    }

    /// Plain (uncolored, spinnerless) description for non-TTY lines.
    fn describe_plain(&self, p: InitProgress) -> String {
        let elapsed = fmt_dur(self.start.elapsed());
        match p {
            InitProgress::Scanning { scanned } => {
                format!("scanning {} files  {elapsed}", commas(scanned))
            }
            InitProgress::Indexing { done, total, .. } => {
                let pct = (done * 100).checked_div(total).unwrap_or(0);
                format!(
                    "reading files {}/{} ({pct}%)  {elapsed}",
                    commas(done),
                    commas(total)
                )
            }
            InitProgress::Saving => format!("making it searchable  {elapsed}"),
            InitProgress::Finalizing => format!("tracing calls  {elapsed}"),
            InitProgress::SemanticRunning { langs, budget_secs } => {
                format!(
                    "tracing calls: {}  {}",
                    semantic_langs_cell(&langs),
                    semantic_tail(&langs, budget_secs, &elapsed)
                )
            }
            InitProgress::PhaseBDeferred => {
                format!("files read  {elapsed}")
            }
        }
    }

    fn describe_json(&self, p: InitProgress) -> String {
        let secs = self.start.elapsed().as_secs();
        match p {
            InitProgress::Scanning { scanned } => {
                format!(r#"{{"phase":"scanning","scanned":{scanned},"elapsed_s":{secs}}}"#)
            }
            InitProgress::Indexing { done, total, .. } => {
                format!(
                    r#"{{"phase":"indexing","done":{done},"total":{total},"elapsed_s":{secs}}}"#
                )
            }
            InitProgress::Saving => {
                format!(r#"{{"phase":"saving","elapsed_s":{secs}}}"#)
            }
            InitProgress::Finalizing => {
                format!(r#"{{"phase":"finalizing","elapsed_s":{secs}}}"#)
            }
            InitProgress::SemanticRunning { langs, budget_secs } => {
                let items: Vec<String> = langs
                    .iter()
                    .map(|(lang, s, sidecar)| {
                        format!(
                            r#"{{"lang":{},"elapsed_s":{s},"bounded":{sidecar}}}"#,
                            crate::lang::json_str(lang)
                        )
                    })
                    .collect();
                // `budget_s` is null when nothing running is bounded, rather than
                // quoting a ceiling that applies to none of these languages.
                let budget = if langs.iter().any(|(_, _, sidecar)| *sidecar) {
                    budget_secs.to_string()
                } else {
                    "null".to_string()
                };
                format!(
                    r#"{{"phase":"semantic","running":[{}],"budget_s":{budget},"elapsed_s":{secs}}}"#,
                    items.join(",")
                )
            }
            InitProgress::PhaseBDeferred => {
                format!(r#"{{"phase":"phase_b_deferred","elapsed_s":{secs}}}"#)
            }
        }
    }
}

/// The per-language cell of the semantic heartbeat: `kotlin 34s · scala 12s`.
/// Language names come from the fan-out, so the user sees WHICH analyzer is
/// slow, not just that something is (#755 item 3).
fn semantic_langs_cell(langs: &[(String, u64, bool)]) -> String {
    langs
        .iter()
        .map(|(lang, s, _)| format!("{lang} {}", fmt_dur(Duration::from_secs(*s))))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The dim tail of the heartbeat line: the ceiling when there is one, a JVM
/// warm-up note when it applies, and total elapsed. Naming the ceiling is the
/// documented-budget half of #755 item 3: "kotlin 90s" alone still reads as a
/// hang unless the line also says how long the run is allowed to take.
///
/// The ceiling is quoted **only for languages that actually have one**. It comes
/// from the sidecar transport's watchdogs, and the builtin languages (rust,
/// typescript, javascript, python, dart) are called in-process by
/// `phase_b_native_*` with no per-language timeout at all. Quoting it for them
/// would be worse than saying nothing: a native TypeScript pass that runs seven
/// minutes would print a ceiling it had already blown past, and a reader who
/// sees a stated limit already exceeded concludes the run is wedged and kills
/// it, which is the behaviour this heartbeat exists to prevent.
fn semantic_tail(langs: &[(String, u64, bool)], budget_secs: u64, elapsed: &str) -> String {
    let jvm = langs.iter().any(|(lang, _, _)| {
        travsr_plugin_host::phase_b::catalog::lookup(lang)
            .and_then(|e| e.runtime_driver)
            .is_some_and(|d| d == "java")
    });
    let hint = if jvm {
        ", JVM startup is slow on first run"
    } else {
        ""
    };
    if !langs.iter().any(|(_, _, sidecar)| *sidecar) {
        // Nothing running is bounded. Say only what is true: which analyzers are
        // going and for how long.
        return if hint.is_empty() {
            format!("   {elapsed}")
        } else {
            format!("({})   {elapsed}", hint.trim_start_matches(", "))
        };
    }
    // Scoped wording rather than "per language": a mixed run has both kinds on
    // the line at once, and only the external ones are bounded.
    format!(
        "(language tools stop at {} each{hint})   {elapsed}",
        fmt_dur(Duration::from_secs(budget_secs))
    )
}

/// Everything the `init` summary says, gathered as `init` runs, so the whole
/// summary is one pure function with a golden test (plan S9).
pub struct InitSummary {
    pub repo: String,
    /// Detected languages, by the name the user knows them by.
    pub found: Vec<&'static str>,
    /// Downloads stopped at the first network failure.
    pub offline: bool,
    /// `installed`, `failed` or `skipped`, as in `--json`'s `search_ranking`.
    pub ranking: &'static str,
    pub files_read: u64,
    /// Nothing changed since the last run.
    pub no_op: bool,
    /// `running`, `started` or `not_started`, as in `--json`'s `keeping_fresh`.
    pub keeping_fresh: &'static str,
    pub connected: crate::connect::Connected,
    pub travsrignore_created: bool,
    pub gitignore_updated: bool,
    pub ghosts_pruned: u64,
    pub ghost_prune_aborted: bool,
    pub languages: Vec<(String, travsr_plugin_host::phase_b::status::Readiness)>,
    /// This repo has not turned on meaning-based search (decision 3: optional).
    pub embed_optional: bool,
    /// The repo has no commit yet, so freshness has no baseline (DEBT-013).
    pub no_commit: bool,
    pub quiet: bool,
}

/// The final line. Stable, because agents key on it (G6).
pub const READY: &str = "Ready. Ask your AI about this code.";
pub const READY_NO_CHANGE: &str = "Ready. Nothing changed since the last run.";

/// The final line for a run, shared by the text summary and `--json`'s `next`.
pub fn ready_line(no_op: bool) -> &'static str {
    if no_op {
        READY_NO_CHANGE
    } else {
        READY
    }
}

/// The `init` summary in plain words (plan 3.0, 3.2): one line per stage, the
/// `Ready` line, then each language that is not ready with its one fix.
pub fn render_summary(s: &InitSummary) -> Vec<String> {
    let mut out = Vec::new();
    let full = !s.no_op && !s.quiet;
    if s.offline {
        out.push(
            "  ! No network, so some language tools were not downloaded. Run `travsr init` \
             again when online."
                .to_string(),
        );
    }
    // On every run, as UX-023 requires: a tripped limit deletes nothing, so the
    // run also reads as "Nothing changed".
    if s.ghost_prune_aborted {
        out.push(
            "  ! Kept the entries for missing files: an unusual number vanished at once. \
             Run `travsr fsck --fix --force` if that was intended."
                .to_string(),
        );
    }
    if full {
        out.insert(0, format!("travsr  Setting up {}", s.repo));
        let mut stage = |text: String| out.push(format!("  \u{2713} {text}"));
        if !s.found.is_empty() {
            stage(format!("Found {}", s.found.join(", ")));
        }
        // Not while a language still waits on tools init gets itself (a failed
        // download). What only the user can supply has its own line below.
        let tools_pending = s
            .languages
            .iter()
            .any(|(_, r)| *r == travsr_plugin_host::phase_b::status::Readiness::SettingUp);
        if !s.offline && !tools_pending {
            stage("Got language tools".to_string());
        }
        if s.ranking == "installed" {
            stage("Got search ranking".to_string());
        }
        stage(format!(
            "Read {} file{}",
            commas(s.files_read),
            if s.files_read == 1 { "" } else { "s" }
        ));
        // Not when every Phase B language failed: the per-language lines below
        // `Ready.` then say calls could not be traced, and an unconditional
        // "Traced calls" would contradict them (PR #940 review). Partial
        // success (some language did trace) still earns the line. `tag()`
        // groups Failed and PartMissing as "failed", as status.rs does.
        let all_failed =
            !s.languages.is_empty() && s.languages.iter().all(|(_, r)| r.tag() == "failed");
        if !all_failed {
            stage("Traced calls".to_string());
        }
        if s.keeping_fresh != "not_started" {
            stage("Keeping it fresh on every commit".to_string());
        }
        if !s.connected.tools.is_empty() {
            let names: Vec<&str> = s
                .connected
                .tools
                .iter()
                .map(|id| crate::connect::display_name(id))
                .collect();
            stage(format!("Connected to {}", names.join(", ")));
        }
        if s.ranking == "failed" {
            out.push(
                "  ! Could not get search ranking; results are ordered by text match until \
                 `travsr init` runs again online."
                    .to_string(),
            );
        }
        if s.connected.needs_approval {
            out.push(
                "  Claude Code asks once whether to trust this project's tools: accept it \
                 (or run /mcp in Claude Code)."
                    .to_string(),
            );
        }
        if s.ghosts_pruned > 0 && !s.ghost_prune_aborted {
            out.push(format!(
                "  Removed {} entr{} for files no longer on disk.",
                commas(s.ghosts_pruned),
                if s.ghosts_pruned == 1 { "y" } else { "ies" }
            ));
        }
        if s.travsrignore_created {
            out.push(
                "  Created .travsrignore: edit it to leave generated or vendored folders out."
                    .to_string(),
            );
        }
        let mut changed: Vec<&str> = s.connected.user_files.iter().map(String::as_str).collect();
        if s.gitignore_updated && !changed.contains(&".gitignore") {
            changed.push(".gitignore");
        }
        if !changed.is_empty() {
            out.push(format!(
                "  Updated {} so travsr's files stay on this machine.",
                changed.join(", ")
            ));
        }
        if s.embed_optional {
            out.push("  Optional: `travsr embed init` adds meaning-based search.".to_string());
        }
        if s.no_commit {
            out.push(
                "  Make a first commit so `travsr status` can tell you how fresh the index is."
                    .to_string(),
            );
        }
    }
    if !s.quiet {
        for problem in &s.connected.problems {
            out.push(format!("  ! {problem}"));
        }
        // A no-change run can still write a file the user owns; RFC-026 keeps
        // such writes visible.
        if s.no_op && !s.connected.user_files.is_empty() {
            out.push(format!("  Updated {}.", s.connected.user_files.join(", ")));
        }
    }
    out.push(ready_line(s.no_op).to_string());
    for (lang, r) in &s.languages {
        if let Some(line) = crate::status::readiness_line(lang, r) {
            out.push(line);
        }
    }
    // Not on a no-change run: travsr cannot see a tool's own settings, so a
    // step already taken would otherwise be asked for on every run.
    if !s.no_op {
        for id in &s.connected.one_step {
            out.push(format!(
                "  {} needs one step from you: run `travsr connect --tool {id}` to see it.",
                crate::connect::display_name(id)
            ));
        }
    }
    out
}

/// The name a user knows a catalog language by.
pub fn language_name(lang: &str) -> &'static str {
    match lang {
        "typescript" => "TypeScript",
        "javascript" => "JavaScript",
        "python" => "Python",
        "rust" => "Rust",
        "go" => "Go",
        "java" => "Java",
        "kotlin" => "Kotlin",
        "scala" => "Scala",
        "csharp" => "C#",
        "cpp" => "C++",
        "c" => "C",
        "ruby" => "Ruby",
        "php" => "PHP",
        "swift" => "Swift",
        "objectivec" => "Objective-C",
        "dart" => "Dart",
        _ => "another language",
    }
}

/// Render the colored progress bar: orange filled (with an eighth-precision
/// leading edge) over a dim track.
fn bar(pal: Palette, pct: u64) -> String {
    let eighths = (pct.min(100) as usize * BAR_W * 8) / 100;
    let full = (eighths / 8).min(BAR_W);
    let rem = if full < BAR_W { eighths % 8 } else { 0 };
    let partial = usize::from(rem > 0);
    let empty = BAR_W - full - partial;

    let mut filled = "█".repeat(full);
    if partial == 1 {
        filled.push(PARTIAL[rem]);
    }
    format!("{}{}", pal.orange(&filled), pal.track(&"░".repeat(empty)))
}

/// Static orange progress bar of a given cell `width`, eighth-precision, for
/// snapshot displays like `travsr embed status`. Same look as the live init bar
/// (orange fill over a dim track) but bracket-free and width-configurable.
pub fn bar_of_width(pal: Palette, done: u64, total: u64, width: usize) -> String {
    let pct = ((done.min(total) as usize) * 100)
        .checked_div(total as usize)
        .unwrap_or(0)
        .min(100);
    let eighths = (pct * width * 8) / 100;
    let full = (eighths / 8).min(width);
    let rem = if full < width { eighths % 8 } else { 0 };
    let partial = usize::from(rem > 0);
    let empty = width - full - partial;
    let mut filled = "█".repeat(full);
    if partial == 1 {
        filled.push(PARTIAL[rem]);
    }
    format!("{}{}", pal.orange(&filled), pal.track(&"░".repeat(empty)))
}

/// Reusable live progress line matching the `travsr init` look — a pulsing
/// graph-node spinner, the orange eighth-precision bar, `done/total`, percent,
/// and elapsed time. Renders in place on **stderr** for a TTY (stdout stays
/// clean for the final summary); emits throttled newline lines otherwise.
///
/// Used by `travsr embed reindex`/`embed init` so a multi-minute embed shows the
/// same progress UI as indexing, instead of going silent.
pub struct LiveBar {
    pal: Palette,
    label: String,
    start: Instant,
    last_paint: Instant,
    frame: usize,
    is_tty: bool,
    last_width: usize,
}

impl LiveBar {
    /// Create a bar rendering to stderr. `label` is a short verb, e.g. "embedding".
    pub fn new(label: impl Into<String>) -> Self {
        let is_tty = std::io::stderr().is_terminal();
        Self {
            pal: Palette::for_stream(is_tty),
            label: label.into(),
            start: Instant::now(),
            // Force an immediate first paint.
            last_paint: Instant::now() - TTY_TICK - TTY_TICK,
            frame: 0,
            is_tty,
            last_width: 0,
        }
    }

    /// Update progress. Throttled to ~10 fps on a TTY, ~every 2 s otherwise.
    pub fn tick(&mut self, done: u64, total: u64) {
        let now = Instant::now();
        let gap = if self.is_tty { TTY_TICK } else { LINE_TICK };
        if now.duration_since(self.last_paint) < gap {
            return;
        }
        self.last_paint = now;
        self.render(done, total, false);
    }

    /// Final paint: green node, 100 %, trailing newline. Call once when done.
    pub fn finish(&mut self, done: u64, total: u64) {
        self.render(done, total.max(done), true);
    }

    fn render(&mut self, done: u64, total: u64, done_state: bool) {
        let pct = (done.min(total) * 100)
            .checked_div(total)
            .unwrap_or(if done_state { 100 } else { 0 })
            .min(100);
        let elapsed = fmt_dur(self.start.elapsed());
        if self.is_tty {
            let node = if done_state {
                self.pal.green("\u{25cf}")
            } else {
                let f = self.pal.orange(&NODE[self.frame % NODE.len()].to_string());
                self.frame += 1;
                f
            };
            let line = format!(
                "  {} {} {} {}/{}  {:>3}%  {}",
                node,
                self.label,
                bar(self.pal, if done_state { 100 } else { pct }),
                commas(done),
                commas(total),
                pct,
                elapsed,
            );
            let pad = self.last_width.saturating_sub(visible_width(&line));
            eprint!("\r{}{}", line, " ".repeat(pad));
            self.last_width = visible_width(&line);
            if done_state {
                eprintln!();
            }
            let _ = std::io::stderr().flush();
        } else if done_state {
            eprintln!(
                "  {} complete, {} embedded in {}",
                self.label,
                commas(done),
                elapsed
            );
        } else {
            eprintln!(
                "  {} {}/{} ({}%) {}",
                self.label,
                commas(done),
                commas(total),
                pct,
                elapsed
            );
        }
    }
}

/// Display width ignoring ANSI SGR escapes, so in-place redraws pad correctly.
fn visible_width(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip the CSI sequence up to and including its final byte ('m').
            for d in chars.by_ref() {
                if d == 'm' {
                    break;
                }
            }
        } else {
            n += 1;
        }
    }
    n
}

/// Group an integer with thousands separators, e.g. `17203` -> `17,203`.
fn commas(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i != 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Compact human duration: `45s`, `2m30s`, `1h02m`.
pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// #724 Finding 4: scip-java generates a `javac` wrapper that expands empty
/// arrays under `set -u`; that is an "unbound variable" error in bash 3.2 (the
/// default `/bin/bash` on macOS) but legal in bash 4.4+. When the `bash`
/// resolved on PATH is too old, scip-java exits 1 and Java Phase B silently
/// produces no call edges, surfacing only as the generic zero-node warning.
/// Returns an actionable hint when running on macOS with a `bash` older than
/// 4.4, else `None`.
pub(crate) fn macos_java_bash_hint() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    // scip-java's shim resolves `bash` via `/usr/bin/env bash`; probing plain
    // `bash` here resolves the same first-on-PATH interpreter.
    let output = std::process::Command::new("bash")
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let (major, minor) = parse_bash_version(&text)?;
    if major > 4 || (major == 4 && minor >= 4) {
        return None;
    }
    Some(format!(
        "note: tracing Java calls needs bash 4.4 or newer, and this Mac's `bash` is {major}.{minor}. Install a newer bash (`brew install bash`) and put it ahead of /bin/bash on PATH, or Java calls will not be traced."
    ))
}

/// Parse the `bash --version` banner into `(major, minor)`, e.g.
/// "GNU bash, version 3.2.57(1)-release (...)" → `(3, 2)`.
fn parse_bash_version(text: &str) -> Option<(u32, u32)> {
    let ver = text.split("version ").nth(1)?;
    let mut parts = ver.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    Some((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> InitSummary {
        use travsr_plugin_host::phase_b::status::Readiness;
        InitSummary {
            repo: "maya-app".into(),
            found: vec!["TypeScript", "Python", "Java"],
            offline: false,
            ranking: "installed",
            files_read: 5,
            no_op: false,
            keeping_fresh: "started",
            connected: crate::connect::Connected {
                tools: vec!["claude-code", "cursor"],
                needs_approval: true,
                user_files: vec![".gitignore".into()],
                one_step: vec!["codex"],
                problems: vec![],
            },
            travsrignore_created: true,
            gitignore_updated: true,
            ghosts_pruned: 0,
            ghost_prune_aborted: false,
            languages: vec![
                ("typescript".into(), Readiness::Ready),
                ("python".into(), Readiness::Ready),
                (
                    "java".into(),
                    Readiness::NeedsToolchain {
                        needs: "JDK, Maven or Gradle".into(),
                    },
                ),
            ],
            embed_optional: true,
            no_commit: false,
            quiet: false,
        }
    }

    /// A tripped mass-delete limit prunes nothing, so the run is a no-change
    /// one; the warning must show anyway, and under --quiet too (UX-023).
    #[test]
    fn a_kept_missing_file_warning_shows_on_a_no_change_quiet_run() {
        let mut s = summary();
        s.no_op = true;
        s.quiet = true;
        s.ghost_prune_aborted = true;
        let lines = render_summary(&s);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("travsr fsck --fix --force")),
            "{lines:#?}"
        );
    }

    /// A download that failed leaves a language setting up: no "Got language
    /// tools" over it.
    #[test]
    fn no_got_language_tools_while_one_still_waits_on_them() {
        use travsr_plugin_host::phase_b::status::Readiness;
        let mut s = summary();
        s.languages.push(("rust".into(), Readiness::SettingUp));
        let lines = render_summary(&s);
        assert!(
            !lines.iter().any(|l| l.contains("Got language tools")),
            "{lines:#?}"
        );
    }

    /// PR #940 review (blocking): "Traced calls" must not claim success when
    /// every Phase B language failed, since the per-language lines below `Ready.`
    /// then say calls could not be traced. Partial success still earns the line.
    #[test]
    fn traced_calls_is_suppressed_only_when_every_language_failed() {
        use travsr_plugin_host::phase_b::status::Readiness;

        // One language, Failed: no "Traced calls", and the honest failure line
        // is the only word on tracing.
        let mut s = summary();
        s.languages = vec![("python".into(), Readiness::Failed)];
        let lines = render_summary(&s);
        assert!(
            !lines.iter().any(|l| l.contains("Traced calls")),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("could not trace calls")),
            "{lines:#?}"
        );

        // PartMissing (the reviewer's own repro) groups as failed too.
        let mut s = summary();
        s.languages = vec![("python".into(), Readiness::PartMissing)];
        assert!(
            !render_summary(&s)
                .iter()
                .any(|l| l.contains("Traced calls")),
            "part-missing must suppress it too"
        );

        // Partial success: one language traced, one failed — the line stays.
        let mut s = summary();
        s.languages = vec![
            ("rust".into(), Readiness::Ready),
            ("python".into(), Readiness::Failed),
        ];
        let lines = render_summary(&s);
        assert!(
            lines.iter().any(|l| l.contains("Traced calls")),
            "partial success must keep it:\n{lines:#?}"
        );
    }

    /// PR #940 review: a commented `.mcp.json` left Claude Code unconnected
    /// with only "Connected to Cursor" to show for it, and a no-change run
    /// wrote user files without a word. Both now say so, even on that run.
    #[test]
    fn a_no_change_run_names_skipped_and_changed_files() {
        let mut s = summary();
        s.no_op = true;
        s.connected.problems = vec![
            "Left .mcp.json alone for Claude Code: existing file is not strict JSON \
             (left untouched)."
                .into(),
        ];
        let lines = render_summary(&s);
        assert_eq!(
            &lines[..3],
            &[
                "  ! Left .mcp.json alone for Claude Code: existing file is not strict JSON \
                 (left untouched).",
                "  Updated .gitignore.",
                "Ready. Nothing changed since the last run.",
            ]
        );
        s.quiet = true;
        assert!(!render_summary(&s).iter().any(|l| l.contains(".mcp.json")));
    }

    /// Plan S9 golden output: the first run, stage by stage, then `Ready`, then
    /// each language that is not ready with its one fix. Every line plain
    /// (plan 3.0): no counts of internal things, no PATH, no placeholders.
    #[test]
    fn init_summary_golden() {
        let lines = render_summary(&summary());
        assert_eq!(
            lines,
            vec![
                "travsr  Setting up maya-app",
                "  \u{2713} Found TypeScript, Python, Java",
                "  \u{2713} Got language tools",
                "  \u{2713} Got search ranking",
                "  \u{2713} Read 5 files",
                "  \u{2713} Traced calls",
                "  \u{2713} Keeping it fresh on every commit",
                "  \u{2713} Connected to Claude Code, Cursor",
                "  Claude Code asks once whether to trust this project's tools: accept it \
                 (or run /mcp in Claude Code).",
                "  Created .travsrignore: edit it to leave generated or vendored folders out.",
                "  Updated .gitignore so travsr's files stay on this machine.",
                "  Optional: `travsr embed init` adds meaning-based search.",
                "Ready. Ask your AI about this code.",
                "  java        needs JDK, Maven or Gradle. Install JDK, Maven or Gradle, then \
                 run `travsr init`.",
                "  Codex needs one step from you: run `travsr connect --tool codex` to see it.",
            ]
        );
        for line in &lines {
            assert_eq!(
                travsr_plugin_host::phase_b::status::jargon_in(line),
                None,
                "{line}"
            );
            assert!(!line.contains("PATH"), "{line}");
        }
    }

    /// A re-run with nothing to do says so in one line, and still lists what the
    /// user must do; the offline case says what to do when back online.
    #[test]
    fn init_summary_no_change_and_offline() {
        let mut s = summary();
        s.no_op = true;
        // Nothing written either: a write is named (see the test above).
        s.connected.user_files.clear();
        let lines = render_summary(&s);
        assert_eq!(lines[0], "Ready. Nothing changed since the last run.");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[1].contains("java"), "{lines:?}");

        let mut s = summary();
        s.offline = true;
        s.ranking = "skipped";
        let lines = render_summary(&s);
        assert!(lines.iter().any(|l| l.contains("when online")), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains("Got language tools")),
            "{lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("Ready.")).count(),
            1,
            "{lines:?}"
        );

        // G6: in a repo with no commit yet, `Ready.` is still the last line at
        // the left margin; the first-commit advice sits above it.
        let mut s = summary();
        s.no_commit = true;
        let lines = render_summary(&s);
        let last_unindented = lines.iter().rfind(|l| !l.starts_with(' ')).unwrap();
        assert!(last_unindented.starts_with("Ready."), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("first commit")),
            "{lines:?}"
        );
    }

    #[test]
    fn commas_groups_thousands() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(7), "7");
        assert_eq!(commas(123), "123");
        assert_eq!(commas(1234), "1,234");
        assert_eq!(commas(17203), "17,203");
        assert_eq!(commas(1234567), "1,234,567");
    }

    #[test]
    fn fmt_dur_scales() {
        assert_eq!(fmt_dur(Duration::from_secs(0)), "0s");
        assert_eq!(fmt_dur(Duration::from_secs(45)), "45s");
        assert_eq!(fmt_dur(Duration::from_secs(150)), "2m30s");
        assert_eq!(fmt_dur(Duration::from_secs(3720)), "1h02m");
    }

    #[test]
    fn bar_width_is_constant_and_clamped() {
        // No color so we can measure visible cells directly.
        let pal = Palette { color: false };
        for pct in [0, 1, 43, 99, 100, 250] {
            assert_eq!(
                bar(pal, pct).chars().count(),
                BAR_W,
                "bar must always be BAR_W cells wide (pct={pct})"
            );
        }
        assert!(bar(pal, 100).chars().all(|c| c == '█'));
        assert!(bar(pal, 0).chars().all(|c| c == '░'));
    }

    #[test]
    fn visible_width_ignores_ansi() {
        let pal = Palette { color: true };
        let painted = pal.orange("hello");
        assert!(painted.len() > 5, "ANSI codes add bytes");
        assert_eq!(visible_width(&painted), 5, "but width counts only glyphs");
        assert_eq!(visible_width(&bar(pal, 50)), BAR_W);
    }

    #[test]
    fn no_color_palette_is_passthrough() {
        let pal = Palette { color: false };
        assert_eq!(pal.orange("x"), "x");
        assert_eq!(pal.green("●"), "●");
    }

    #[test]
    fn parses_bash_version_banner() {
        // #724 Finding 4: the macOS stock bash and a brew bash must parse, and
        // the 4.4 boundary must land on the right side.
        assert_eq!(
            parse_bash_version("GNU bash, version 3.2.57(1)-release (arm64-apple-darwin24)"),
            Some((3, 2))
        );
        assert_eq!(
            parse_bash_version("GNU bash, version 5.2.37(1)-release (aarch64-apple-darwin24.4.0)"),
            Some((5, 2))
        );
        assert_eq!(
            parse_bash_version("GNU bash, version 4.4.23(1)-release"),
            Some((4, 4))
        );
        assert_eq!(parse_bash_version("not a version banner"), None);
    }

    #[test]
    fn indexing_frame_drops_eta_keeps_elapsed() {
        // D1: the projected ETA is gone from the live bar, but measured elapsed
        // (a fact, not a projection) stays. describe_plain ignores the mode, so
        // constructing in any mode is fine.
        let r = ProgressReporter::new(true, false);
        let line = r.describe_plain(InitProgress::Indexing {
            done: 283,
            total: 566,
            workers: 4,
        });
        assert!(line.contains("283/566"), "counts must remain: {line}");
        assert!(line.contains("(50%)"), "percent must remain: {line}");
        assert!(
            !line.to_ascii_lowercase().contains("eta"),
            "projected ETA must be gone: {line}"
        );
        assert!(
            line.trim_end().ends_with('s'),
            "measured elapsed must remain: {line}"
        );
    }

    /// Saving what was read took 74 s on yugabyte-db (15 s flush, 59 s search
    /// rebuild) with the line frozen at "reading files ... 99%". The heartbeat
    /// names the step and keeps its clock moving.
    #[test]
    fn saving_is_named_in_plain_words() {
        let r = ProgressReporter::new(true, false);
        let line = r.describe_plain(InitProgress::Saving);
        assert!(line.starts_with("making it searchable"), "{line}");
        assert!(line.trim_end().ends_with('s'), "elapsed must show: {line}");
        assert_eq!(travsr_plugin_host::phase_b::status::jargon_in(&line), None);
        assert!(r
            .describe_json(InitProgress::Saving)
            .contains(r#""phase":"saving""#));
    }
}

/// #755 item 3: the semantic heartbeat line — the signal that stops a
/// multi-minute JVM cold start from reading as a hang.
#[cfg(test)]
mod issue_755_heartbeat_tests {
    use super::*;

    fn kotlin_scala() -> Vec<(String, u64, bool)> {
        // Both are sidecar languages, so both are bounded by the transport.
        vec![
            ("kotlin".to_string(), 94, true),
            ("scala".to_string(), 12, true),
        ]
    }

    /// The reported failure mode is "which analyzer is slow" being invisible.
    /// The cell must name every running language with its own elapsed time.
    #[test]
    fn the_cell_names_each_running_language_with_its_elapsed() {
        let cell = semantic_langs_cell(&kotlin_scala());
        assert_eq!(cell, "kotlin 1m34s · scala 12s");
    }

    /// The documented-budget half of the item: the tail states the ceiling, so
    /// "kotlin 94s" reads as "inside its window", not as wedged.
    #[test]
    fn the_tail_states_the_budget_for_bounded_languages() {
        let tail = semantic_tail(&kotlin_scala(), 360, "2m 0s");
        assert!(
            tail.contains("language tools stop at 6m00s each"),
            "the ceiling must be stated, and scoped to what it applies to; got: {tail}"
        );
        assert!(
            tail.ends_with("2m 0s"),
            "total elapsed stays visible; got: {tail}"
        );
    }

    /// The builtin languages run in-process with no per-language timeout, so
    /// quoting one is a claim about a limit that does not exist. A native
    /// TypeScript pass that legitimately runs past the number would then read as
    /// wedged, which is what this heartbeat is here to prevent.
    #[test]
    fn an_unbounded_language_is_not_told_about_a_ceiling() {
        let tail = semantic_tail(&[("typescript".to_string(), 420, false)], 360, "7m 0s");
        assert!(
            !tail.contains("stop at") && !tail.contains("6m00s"),
            "a language with no timeout must not be quoted one; got: {tail}"
        );
        assert!(
            tail.contains("7m 0s"),
            "the elapsed still shows; got: {tail}"
        );
    }

    /// A mixed run has both kinds on one line. The ceiling is real for the
    /// sidecar half, so it is stated, but worded so it does not claim to cover
    /// the native half beside it.
    #[test]
    fn a_mixed_run_scopes_the_ceiling_to_the_bounded_half() {
        let tail = semantic_tail(
            &[
                ("kotlin".to_string(), 94, true),
                ("typescript".to_string(), 30, false),
            ],
            360,
            "2m 0s",
        );
        assert!(tail.contains("language tools stop at"), "got: {tail}");
        assert_eq!(
            travsr_plugin_host::phase_b::status::jargon_in(&tail),
            None,
            "{tail}"
        );
        assert!(
            !tail.contains("per language"),
            "the old wording claimed every language was bounded; got: {tail}"
        );
    }

    /// kotlin and scala run on a JVM, whose cold start is the whole reason this
    /// heartbeat exists — say so, keyed off the catalog's runtime_driver rather
    /// than a hardcoded language list, so a future JVM language inherits it.
    #[test]
    fn jvm_languages_get_the_warm_up_note() {
        let tail = semantic_tail(&kotlin_scala(), 300, "2m 0s");
        assert!(
            tail.contains("JVM startup is slow on first run"),
            "got: {tail}"
        );
        // java is JVM too.
        let tail = semantic_tail(&[("java".to_string(), 30, true)], 300, "1m 0s");
        assert!(tail.contains("JVM"), "got: {tail}");
    }

    /// A non-JVM analyzer must not carry a JVM excuse — a wrong explanation is
    /// worse than none.
    #[test]
    fn non_jvm_languages_do_not_get_the_jvm_note() {
        let tail = semantic_tail(&[("go".to_string(), 8, true)], 300, "30s");
        assert!(!tail.contains("JVM"), "got: {tail}");
        // An unknown language (not in the catalog) must not panic or claim JVM.
        let tail = semantic_tail(&[("nolang".to_string(), 8, true)], 300, "30s");
        assert!(!tail.contains("JVM"), "got: {tail}");
    }

    /// The `--json` heartbeat must stay parseable — it is the machine surface
    /// CI reads, and language names pass through the shared JSON escaper.
    #[test]
    fn the_json_heartbeat_parses_and_carries_the_fields() {
        let rep = ProgressReporter::new(false, true);
        let line = rep.describe_json(InitProgress::SemanticRunning {
            langs: kotlin_scala(),
            budget_secs: 360,
        });
        let parsed: serde_json::Value =
            serde_json::from_str(&line).expect("heartbeat JSON must parse");
        assert_eq!(parsed["phase"], "semantic");
        assert_eq!(parsed["budget_s"], 360);
        assert_eq!(parsed["running"][0]["lang"], "kotlin");
        assert_eq!(parsed["running"][0]["elapsed_s"], 94);
        assert_eq!(parsed["running"][0]["bounded"], true);
        assert_eq!(parsed["running"][1]["lang"], "scala");
    }

    /// A consumer keying on `budget_s` must not read a ceiling for a run that
    /// has none, so it is null rather than a number that applies to nothing.
    #[test]
    fn the_json_budget_is_null_when_nothing_is_bounded() {
        let rep = ProgressReporter::new(false, true);
        let line = rep.describe_json(InitProgress::SemanticRunning {
            langs: vec![("typescript".to_string(), 12, false)],
            budget_secs: 360,
        });
        let parsed: serde_json::Value =
            serde_json::from_str(&line).expect("heartbeat JSON must parse");
        assert!(parsed["budget_s"].is_null(), "got: {line}");
        assert_eq!(parsed["running"][0]["bounded"], false);
    }

    /// TTY and plain renderings both carry the language names — the heartbeat
    /// exists for the human watching either surface.
    #[test]
    fn tty_and_plain_lines_both_name_the_languages() {
        let rep = ProgressReporter::new(false, false);
        let ev = InitProgress::SemanticRunning {
            langs: kotlin_scala(),
            budget_secs: 300,
        };
        let plain = rep.describe_plain(ev.clone());
        assert!(
            plain.contains("kotlin") && plain.contains("scala"),
            "got: {plain}"
        );
        let tty = rep.compose("*", ev);
        assert!(
            tty.contains("kotlin") && tty.contains("scala"),
            "got: {tty}"
        );
    }
}
