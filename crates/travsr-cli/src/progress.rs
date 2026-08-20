//! Live progress UI for `travsr init` (issue #293).
//!
//! A large repo can take many minutes to index; with no output the command is
//! indistinguishable from a hang. This renders progress to **stderr** (stdout
//! stays clean for the final summary), adapting to context:
//!
//! - **TTY**: a single self-updating line — a pulsing graph-node spinner, an
//!   eighth-precision bar, `done/total`, percent, elapsed, and a rough ETA.
//!   Brand orange while working; the final summary node flips to fresh green.
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

use travsr_daemon::{InitProgress, InitStats};

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
    /// Muted secondary text (elapsed/eta/hints).
    pub fn dim(self, s: &str) -> String {
        self.paint("2", s)
    }
    /// Bold — the wordmark.
    fn bold(self, s: &str) -> String {
        self.paint("1", s)
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
                let tail = match eta(self.start, done, total) {
                    Some(e) => format!("{elapsed} · eta {}", fmt_dur(e)),
                    None => elapsed,
                };
                format!(
                    "  {spinner} indexing  {}  {}/{}  {pct}%   {}",
                    bar(pal, pct),
                    commas(done),
                    commas(total),
                    pal.dim(&tail)
                )
            }
            InitProgress::Finalizing => {
                format!(
                    "  {spinner} finalizing  semantic pass   {}",
                    pal.dim(&elapsed)
                )
            }
            InitProgress::PhaseBDeferred => {
                // Transient line — do not assert *when* semantic edges build (that
                // depends on whether a daemon is running, which init decides after
                // this pass). print_summary states it accurately; here just report
                // that the structural pass is done.
                format!(
                    "  {} structural index ready   {}",
                    pal.green("●"),
                    pal.dim(&elapsed)
                )
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
                let eta = eta(self.start, done, total)
                    .map(|e| format!("  eta {}", fmt_dur(e)))
                    .unwrap_or_default();
                format!(
                    "indexing {}/{} ({pct}%)  {elapsed}{eta}",
                    commas(done),
                    commas(total)
                )
            }
            InitProgress::Finalizing => format!("finalizing (semantic pass)  {elapsed}"),
            InitProgress::PhaseBDeferred => {
                format!("structural index ready  {elapsed}")
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
            InitProgress::Finalizing => {
                format!(r#"{{"phase":"finalizing","elapsed_s":{secs}}}"#)
            }
            InitProgress::PhaseBDeferred => {
                format!(r#"{{"phase":"phase_b_deferred","elapsed_s":{secs}}}"#)
            }
        }
    }
}

/// Print the final, on-brand summary for the human modes (TTY/plain) to stdout.
/// `--json` is handled by the caller; this is a no-op for it via the caller's
/// branch. The summary node is fresh green; the "try" hint is shown unless quiet.
pub fn print_summary(stats: &InitStats, elapsed: Duration, quiet: bool, daemon_running: bool) {
    let pal = Palette::for_stream(std::io::stdout().is_terminal());
    let node = pal.green("●");
    let dur = fmt_dur(elapsed);

    // UX-023: the ghost sweep's result is otherwise only a `tracing` event, which
    // the default `error` stderr filter hides (UX-002 downgraded these to WARN).
    // Surface it on stdout — in *both* the up-to-date and normal branches — so a
    // pruned-ghosts run, or a sweep that tripped the mass-delete breaker and
    // pruned nothing, is visible in the summary the user actually reads.
    let emit_ghost_note = || {
        if stats.ghost_prune_aborted {
            println!(
                "  {} ghost sweep skipped — an unusual number of indexed files \
                 vanished at once, so nothing was pruned; run \
                 `travsr fsck --fix --force` if that was intentional",
                pal.orange("⚠"),
            );
        } else if stats.ghosts_pruned > 0 {
            println!(
                "  {} pruned {} node(s) for files no longer on disk",
                pal.dim("ℹ"),
                commas(stats.ghosts_pruned),
            );
        }
    };

    if stats.nodes_written == 0 && stats.edges_written == 0 {
        // Re-run with nothing to do — already fresh.
        println!(
            "  {node} up to date · {} nodes · {} edges · {dur}",
            commas(stats.total_nodes),
            commas(stats.total_edges),
        );
        emit_ghost_note();
        // UX-2: "up to date" reports the Phase A delta only. When Phase B still
        // re-ran its analyzers (e.g. after `--force`, or a re-run following a
        // crash) the graph delta can be zero yet the semantic pass did execute —
        // say so, so the message is not read as "nothing happened".
        if let Some(report) = &stats.phase_b_report {
            if !report.ran.is_empty() {
                println!(
                    "  {} semantic analysis re-ran for: {} (no graph changes)",
                    pal.dim("ℹ"),
                    report.ran.join(", "),
                );
            }
        }
        return;
    }

    // UX-006: split the two kinds of skip so the counts reconcile with the
    // progress denominator. The bar counts indexable files (indexed + unchanged),
    // while `ignored` files never enter the bar at all. Bundling both into one
    // "N skipped" number made three unrelated totals (bar total, indexed, and
    // indexed+skipped) that added up to nothing. Naming them lets the reader see
    // `indexed + unchanged = bar total`, with `ignored` accounted separately.
    let mut skip_parts: Vec<String> = Vec::new();
    if stats.files_skipped_unchanged > 0 {
        skip_parts.push(format!(
            "{} unchanged",
            commas(stats.files_skipped_unchanged)
        ));
    }
    if stats.files_skipped_ignored > 0 {
        skip_parts.push(format!("{} ignored", commas(stats.files_skipped_ignored)));
    }
    let skipped_note = if skip_parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", skip_parts.join(", "))
    };
    // UX-003: the counts written this pass are a *delta*, not the graph total, so
    // "0 nodes · 6,028 edges" looked self-contradictory and matched neither the
    // real graph nor `travsr status`. Show `+delta/total` for both so the pass
    // change and the resulting totals (which `status` reports) are both explicit.
    println!(
        "  {node} indexed {} files{skipped_note} · +{}/{} nodes · +{}/{} edges · {dur}",
        commas(stats.files_indexed),
        commas(stats.nodes_written.max(0) as u64),
        commas(stats.total_nodes),
        commas(stats.edges_written),
        commas(stats.total_edges),
    );
    emit_ghost_note();

    match &stats.phase_b_report {
        None => {
            if daemon_running {
                // A daemon is up (interactive init spawned one, or one was already
                // running). It auto-arms Phase B on startup and indexes semantic
                // call edges in the background for the current commit — so this is
                // genuinely "in progress", not commit-gated.
                println!(
                    "  {} semantic call edges are indexing in the background — run `travsr status` to check progress",
                    pal.dim("ℹ"),
                );
            } else {
                // No daemon running (non-interactive / CI, or spawn failed): Phase
                // B waits for one. The git-commit hook starts a daemon, or the user
                // can build the edges now, synchronously.
                println!(
                    "  {} semantic call edges will build once a daemon is running — your next `git commit` starts one, or run `travsr init --semantic` to build them now",
                    pal.dim("ℹ"),
                );
            }
        }
        Some(report) => {
            // UX-8: `ran` includes languages whose analyzer executed but emitted
            // zero symbols. Calling those "enabled" reads to the user as "working",
            // so report only the languages that actually produced symbols here, and
            // call out the ones that ran dry separately (matches the `travsr status`
            // zero-node warning rather than masking it under "enabled").
            let produced: Vec<&str> = report
                .ran
                .iter()
                .filter(|l| !report.produced_no_nodes.contains(l))
                .map(String::as_str)
                .collect();
            if !produced.is_empty() {
                let langs = produced.join(", ");
                println!(
                    "  {} semantic analysis produced symbols for: {langs}",
                    pal.dim("ℹ"),
                );
            }
            if !report.produced_no_nodes.is_empty() {
                let langs = report.produced_no_nodes.join(", ");
                println!(
                    "  {} semantic analyzer ran but produced no symbols for: {langs} — see `travsr status` for why",
                    pal.orange("⚠"),
                );
                if report.produced_no_nodes.iter().any(|l| l == "java") {
                    if let Some(hint) = macos_java_bash_hint() {
                        println!("    {hint}");
                    }
                }
            }
            if !report.skipped_no_analyzer.is_empty() {
                let langs = report.skipped_no_analyzer.join(", ");
                println!(
                    "  {} no semantic analyzer for: {langs} — run `travsr lang install <lang>` to enable",
                    pal.dim("ℹ"),
                );
            }
            for lang in &report.skipped_needs_consent {
                println!(
                    "  {} full analysis for {lang} needs your permission — run `travsr lang allow-unsandboxed {lang}` to enable",
                    pal.dim("ℹ"),
                );
            }
            if !report.skipped_untrusted_corpus.is_empty() {
                let langs = report.skipped_untrusted_corpus.join(", ");
                println!(
                    "  {} semantic analysis not enabled here for: {langs} — run `travsr lang install <lang>` in this repository to enable",
                    pal.dim("ℹ"),
                );
            }
            if !report.skipped_no_compdb.is_empty() {
                let langs = report.skipped_no_compdb.join(", ");
                println!(
                    "  {} no compile_commands.json for: {langs} — generate one to enable semantic analysis",
                    pal.dim("ℹ"),
                );
            }
            if !report.crashed.is_empty() {
                let langs = report.crashed.join(", ");
                println!(
                    "  {} semantic analysis failed for: {langs} — rerun with RUST_LOG=travsr_plugin_host=debug",
                    pal.dim("⚠"),
                );
            }
        }
    }

    if stats.travsrignore_scaffolded {
        println!(
            "  {} created .travsrignore — customize to exclude generated dirs, vendored deps, etc.",
            pal.dim("ℹ"),
        );
    }

    if !quiet {
        println!(
            "    {}",
            pal.dim(r#"try: travsr ask "what calls PaymentService?""#)
        );
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
                "  {} complete — {} embedded in {}",
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

/// Minimum samples + elapsed window before an ETA is trustworthy. UX-005:
/// extrapolating from the first tick (1 file in 4 s) produced a 46-minute ETA on
/// a job that finished in 13 s, inviting a premature Ctrl-C. Withhold the
/// estimate until throughput has stabilised.
const ETA_WARMUP_FILES: u64 = 8;
const ETA_WARMUP_SECS: f64 = 2.0;

/// Rough ETA from average throughput so far. `None` once done, at start, or
/// still inside the warm-up window (see [`ETA_WARMUP_FILES`]).
fn eta(start: Instant, done: u64, total: u64) -> Option<Duration> {
    if done == 0 || done >= total {
        return None;
    }
    let secs = start.elapsed().as_secs_f64();
    // Warm-up floor: too few samples or too short a window still gives a wild
    // extrapolation. Hold the ETA back and show only elapsed until then.
    if done < ETA_WARMUP_FILES || secs < ETA_WARMUP_SECS {
        return None;
    }
    let rate = done as f64 / secs; // files/sec
    if rate <= 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64((total - done) as f64 / rate))
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
        "note: scip-java's javac shim requires bash 4.4+, but this Mac's `bash` is {major}.{minor}. Install a newer bash (`brew install bash`) and put it ahead of /bin/bash on PATH, otherwise Java semantic indexing silently produces no call edges."
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
    fn eta_none_at_edges() {
        let start = Instant::now();
        assert!(eta(start, 0, 100).is_none());
        assert!(eta(start, 100, 100).is_none());
        assert!(eta(start, 150, 100).is_none());
    }

    #[test]
    fn eta_withheld_during_warmup() {
        // UX-005: a fresh start with only a handful of files done must not emit an
        // ETA — the sample count is below the warm-up floor.
        let start = Instant::now();
        assert!(
            eta(start, 1, 566).is_none(),
            "one file in must be inside the warm-up window"
        );
        assert!(
            eta(start, ETA_WARMUP_FILES - 1, 566).is_none(),
            "still under the file floor => no ETA"
        );
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
}
