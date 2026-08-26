//! File-system watcher — spawns a notify-backed producer thread.
//!
//! Uses `notify` v8 (already a workspace dependency) directly rather than a
//! debouncer wrapper, to avoid pulling in a conflicting version of the notify
//! types crate. A 500 ms coalescing window is implemented with a HashMap of
//! pending events flushed by a background tick.
//!
//! The watcher runs on a plain OS thread so that kqueue/inotify descriptors
//! have a proper thread to live on. A `WatcherHandle` provides a shutdown
//! signal so that the watcher exits cleanly when the daemon stops.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// Events the daemon's indexer worker receives from the watcher.
#[derive(Debug)]
pub enum WatchEvent {
    Upsert(PathBuf),
    Remove(PathBuf),
}

/// A handle to the running watcher threads. Drop to signal shutdown.
pub struct WatcherHandle {
    stop: Arc<AtomicBool>,
    /// #801: raw events accepted from the OS, counted BEFORE any userspace
    /// filtering.
    ///
    /// Observable because the userspace filter makes the interesting failure
    /// invisible from the outside: `should_skip_all` discards `target/` events
    /// either way, so "no reindex happened" is equally true whether the tree is
    /// unwatched or whether every one of its 251,662 files is being watched,
    /// queued and stat'd first. That indistinguishability is precisely why the
    /// existing filter tests passed while the daemon grew 13 MB/s.
    raw_events: Arc<AtomicU64>,
    /// Raw events dropped because the bounded queue was full (#801).
    raw_dropped: Arc<AtomicU64>,
    _event_thread: std::thread::JoinHandle<()>,
    _flush_thread: std::thread::JoinHandle<()>,
}

impl WatcherHandle {
    /// Raw OS events accepted so far, before userspace filtering.
    pub fn raw_events(&self) -> u64 {
        self.raw_events.load(Ordering::Relaxed)
    }

    /// Raw OS events shed because the bounded queue was full.
    pub fn raw_dropped(&self) -> u64 {
        self.raw_dropped.load(Ordering::Relaxed)
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Debounce window — coalesce rapid saves into a single reindex call.
const DEBOUNCE_MS: u64 = 500;

/// Hard cap on the debounce table size.
///
/// At ~150 bytes per entry (PathBuf + PendingKind + Instant + HashMap overhead)
/// this limits the table to ~15 MB. During a pathological event flood (e.g. a
/// `git checkout` touching 100k+ files while the single indexer worker is
/// saturated), the bounded tokio channel back-pressures the flush thread which
/// lets the debounce table grow unboundedly without this cap.
///
/// Policy: updates to *already-pending* paths coalesce as normal. Brand-new
/// paths beyond the cap are dropped with a throttled warning. The post-commit
/// hook reconciles via `git diff-tree` on the next commit; `travsr init`
/// always fully recovers the graph.
const MAX_PENDING: usize = 100_000;

/// #801: capacity of the raw OS-event queue between the notify callback and the
/// event loop.
///
/// Sized well above any burst a human edit or a `git checkout` produces, and far
/// below the point where holding that many `notify::Event` values costs real
/// memory, so it only engages on a genuine flood. Deliberately smaller than
/// `MAX_PENDING`: this queue holds unfiltered events, most of which the very
/// next step discards, whereas the debounce table holds paths already known to
/// be relevant and worth keeping.
const RAW_EVENT_CAP: usize = 8_192;

/// Mirrored by `travsr-mcp`'s own `SKIP_DIRS` so `find_pattern` searches the
/// same file universe the graph is built from (#448). The dependency rules run
/// `travsr-daemon → travsr-mcp`, so the constant cannot be shared; keep the two
/// lists identical.
pub(crate) const SKIP_DIRS: &[&str] = &[
    ".claude",
    ".git",
    ".travsr",
    "target",
    "node_modules",
    "dist",
    ".next",
    ".vscode",
    ".vscode-test",
];

/// Internal pending-event entry for the debounce table.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingKind {
    Upsert,
    Remove,
}

/// Spawn the file-system watcher. Returns a `WatcherHandle`; drop it to stop.
///
/// Blocks until the underlying kqueue/inotify watch is fully established and
/// skip directories have been unwatched. This guarantees that when `spawn`
/// returns, the caller can safely create files in `.travsr/` (e.g. the daemon
/// control socket) without triggering kqueue ENOTSUP errors.
///
/// Events that originate from files whose mtime predates `start_time`
/// (FSEvents on macOS can replay old events on startup) are silently dropped.
pub fn spawn(
    repo_root: &Path,
    tx: mpsc::Sender<WatchEvent>,
    start_time: Instant,
) -> anyhow::Result<WatcherHandle> {
    let repo_root = repo_root.to_path_buf();
    let gitignore = build_ignore_matcher(&repo_root);
    let stop = Arc::new(AtomicBool::new(false));

    // Pending debounce table shared between the notify callback and the flush thread.
    let pending: Arc<Mutex<HashMap<PathBuf, (PendingKind, Instant)>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // #801: bounded. This channel sits UPSTREAM of the debounce table's
    // `MAX_PENDING` cap, so that cap never applied to it: a flood grew this
    // queue without limit, at ~13 MB/s on a repo with a populated `target/`,
    // until the machine thrashed. Shedding is the right policy for a
    // reindex trigger, exactly as it already is for the debounce table: the
    // next event for a path supersedes the one dropped, and the periodic
    // reconcile catches anything missed entirely.
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<notify::Event>(RAW_EVENT_CAP);
    let raw_events = Arc::new(AtomicU64::new(0));
    let raw_dropped = Arc::new(AtomicU64::new(0));

    // ── Flush thread ─────────────────────────────────────────────────────────
    // Every DEBOUNCE_MS, emit entries whose deadline has passed. Runs on an OS
    // thread so it doesn't interact with Tokio's blocking thread pool.
    let pending_flush = Arc::clone(&pending);
    let tx_flush = tx.clone();
    let stop_flush = Arc::clone(&stop);
    let flush_thread = std::thread::Builder::new()
        .name("travsr-watcher-flush".into())
        .spawn(move || {
            while !stop_flush.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(DEBOUNCE_MS));
                if stop_flush.load(Ordering::Acquire) {
                    break;
                }
                let now = Instant::now();
                let ready: Vec<(PathBuf, PendingKind)> = {
                    let mut guard = pending_flush.lock().unwrap_or_else(|e| e.into_inner());
                    let ready: Vec<(PathBuf, PendingKind)> = guard
                        .iter()
                        .filter(|(_, (_, deadline))| *deadline <= now)
                        .map(|(p, (k, _))| (p.clone(), k.clone()))
                        .collect();
                    for (path, _) in &ready {
                        guard.remove(path);
                    }
                    ready
                };
                for (path, kind) in ready {
                    let ev = match kind {
                        PendingKind::Remove => WatchEvent::Remove(path),
                        PendingKind::Upsert => WatchEvent::Upsert(path),
                    };
                    if tx_flush.blocking_send(ev).is_err() {
                        return; // channel closed — daemon shutting down
                    }
                }
            }
        })
        .expect("failed to spawn watcher flush thread");

    // Ready channel: the event thread signals Ok(()) once the watch is fully
    // set up and skip dirs are unwatched. spawn() blocks on this before
    // returning so the caller cannot create the socket before kqueue is done
    // scanning — preventing the ENOTSUP race on .travsr/daemon.sock.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();

    // ── Event-processing thread ───────────────────────────────────────────────
    // Owns the `RecommendedWatcher` so it lives on a stable OS thread.
    let raw_events_cb = Arc::clone(&raw_events);
    let raw_dropped_cb = Arc::clone(&raw_dropped);
    let stop_event = Arc::clone(&stop);
    let event_thread = std::thread::Builder::new()
        .name("travsr-watcher-event".into())
        .spawn(move || {
            // #403: the ignore matcher is rebuilt in-place when a
            // .gitignore/.travsrignore changes, so it must be mutable. It lives
            // only on this event thread, so no synchronisation is needed.
            let mut gitignore = gitignore;
            let mut watcher = match RecommendedWatcher::new(
                move |res: notify::Result<notify::Event>| {
                    if let Ok(ev) = res {
                        raw_events_cb.fetch_add(1, Ordering::Relaxed);
                        // try_send, never send: this runs on the notify thread,
                        // and blocking it stalls the OS event source itself.
                        if let Err(std::sync::mpsc::TrySendError::Full(_)) = raw_tx.try_send(ev) {
                            let n = raw_dropped_cb.fetch_add(1, Ordering::Relaxed) + 1;
                            if n == 1 || n % 10_000 == 0 {
                                tracing::warn!(
                                    dropped = n,
                                    cap = RAW_EVENT_CAP,
                                    "watcher event queue full; shedding events. \
                                     The periodic reconcile still catches missed paths."
                                );
                            }
                        }
                    }
                },
                notify::Config::default(),
            ) {
                Ok(w) => w,
                Err(e) => {
                    tracing::error!("failed to create file watcher: {e}");
                    let _ = ready_tx.send(Err(anyhow::anyhow!("failed to create watcher: {e}")));
                    return;
                }
            };

            if let Err(e) = watcher.watch(&repo_root, RecursiveMode::Recursive) {
                tracing::error!("failed to watch {}: {e}", repo_root.display());
                let _ = ready_tx.send(Err(anyhow::anyhow!("failed to watch repo: {e}")));
                return;
            }

            // #801: drop the watches this module has always claimed to drop.
            // Two doc comments promised it and no code did it, so a recursive
            // watch covered every SKIP_DIRS tree: on a built Rust repo that is
            // 15,664 directories and 251,662 files under `target/` alone,
            // producing events that were queued and stat'd before being
            // discarded by `should_skip_all`.
            //
            // This is not only a cost fix. `spawn`'s contract, which
            // `lib.rs` relies on before binding the control socket, is that
            // `.travsr/` is unwatched by the time it returns; otherwise the
            // kqueue backend opens `daemon.sock` and gets ENOTSUP, killing the
            // whole watch. That guarantee has never actually held.
            //
            // Failures are logged and not fatal: a skip dir that does not exist
            // in this repo is the common case, and losing the optimisation is
            // never worth refusing to watch the repo at all.
            for skip in SKIP_DIRS {
                let p = repo_root.join(skip);
                if p.is_dir() {
                    // WatchNotFound is the EXPECTED result on FSEvents (macOS
                    // default backend): it streams one recursive watch from the
                    // root and has no per-directory watch to remove, so there is
                    // nothing to unwatch and this call cannot help there. Verified
                    // against notify 8.2.0 rather than assumed. Backends with
                    // per-directory watches (inotify, the platform the 13 MB/s
                    // failure was measured on) drop the subtree here. On FSEvents
                    // the cost is instead bounded by the reordered filter and the
                    // bounded queue below, which is why those are not optional
                    // extras to this fix.
                    if let Err(e) = watcher.unwatch(&p) {
                        tracing::debug!("unwatch {} not applied (non-fatal): {e}", p.display());
                    }
                }
            }

            // Signal ready — watch is established and skip dirs are unwatched,
            // so the caller can proceed.
            let _ = ready_tx.send(Ok(()));

            // Block on raw events until the stop flag is set.
            // `raw_rx` unblocks when `raw_tx` is dropped (watcher is dropped above
            // when this scope exits, but we need to exit the loop first).
            // Use a timeout-based recv to periodically check the stop flag.
            let mut dropped_total: u64 = 0;
            loop {
                if stop_event.load(Ordering::Acquire) {
                    break;
                }
                match raw_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(event) => {
                        // #403: a change to any .gitignore/.travsrignore
                        // invalidates the cached matcher — rebuild it so newly
                        // ignored paths stop being indexed without a daemon
                        // restart. Done once per event before its paths are
                        // filtered so the new rules apply to this batch too.
                        if event.paths.iter().any(|p| is_ignore_file(p)) {
                            gitignore = build_ignore_matcher(&repo_root);
                        }
                        for path in &event.paths {
                            // #801: the cheap prefix test runs FIRST. It used to
                            // run after the `metadata` call below, so every event
                            // from a skipped tree paid a stat syscall before being
                            // rejected on a string compare. Ordering only: a
                            // skipped path is discarded either way, so nothing
                            // downstream sees a different set of paths.
                            if should_skip_all(path, &repo_root, &gitignore) {
                                continue;
                            }

                            // Drop events predating daemon start (FSEvents replay guard).
                            if let Ok(meta) = std::fs::metadata(path) {
                                if let Ok(modified) = meta.modified() {
                                    if modified
                                        .elapsed()
                                        .map(|age| age > start_time.elapsed())
                                        .unwrap_or(false)
                                    {
                                        continue;
                                    }
                                }
                            }

                            // Name(From) is the "old path" half of a rename — treat
                            // it as Remove so deleted nodes are tombstoned.
                            // Name(To) and all other events are Upsert.
                            // For Upsert: also apply the upsert-specific filter
                            // (is_file + language ext). NOT applied to Remove: the
                            // file no longer exists so is_file() returns false.
                            let kind = match event.kind {
                                EventKind::Remove(_)
                                | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                                    PendingKind::Remove
                                }
                                _ => {
                                    if should_skip_upsert(path) {
                                        continue;
                                    }
                                    PendingKind::Upsert
                                }
                            };

                            let deadline = Instant::now() + Duration::from_millis(DEBOUNCE_MS);
                            let mut guard = pending.lock().unwrap_or_else(|e| e.into_inner());
                            // Coalesce rules:
                            // - Upsert → Remove: Remove wins (file deleted, pending write discarded).
                            // - Remove → Upsert: Upsert wins (file recreated; must re-index).
                            // - Upsert → Upsert / Remove → Remove: update deadline, keep kind.
                            // Updates to existing pending paths always coalesce — only
                            // brand-new paths are gated by MAX_PENDING.
                            match guard.get(path) {
                                Some((PendingKind::Upsert, _)) if kind == PendingKind::Remove => {
                                    guard.insert(path.clone(), (PendingKind::Remove, deadline));
                                }
                                Some(_) => {
                                    guard.insert(path.clone(), (kind, deadline));
                                }
                                None => {
                                    if guard.len() < MAX_PENDING {
                                        guard.insert(path.clone(), (kind, deadline));
                                    } else {
                                        dropped_total += 1;
                                        if dropped_total == 1 || dropped_total % 10_000 == 0 {
                                            tracing::warn!(
                                                dropped = dropped_total,
                                                cap = MAX_PENDING,
                                                "watcher debounce table full, new paths \
                                                 dropped (run `travsr init` to reconcile)"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Normal — poll the stop flag and continue.
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        break;
                    }
                }
            }
            // `watcher` is dropped here, which stops the kernel watch.
        })
        .expect("failed to spawn watcher event thread");

    // Block until the event thread confirms the watch is established.
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => anyhow::bail!("watcher event thread exited before signalling ready"),
    }

    Ok(WatcherHandle {
        stop,
        raw_events,
        raw_dropped,
        _event_thread: event_thread,
        _flush_thread: flush_thread,
    })
}

/// Build the combined ignore matcher used to filter watcher events.
///
/// Collects every `.gitignore` and `.travsrignore` under `repo_root` into one
/// matcher (#403). `.travsrignore` files are added after `.gitignore` so their
/// rules take precedence, mirroring `init_repo`'s
/// `SKIP_DIRS < .gitignore < .travsrignore` ordering. The walk skips `SKIP_DIRS`
/// so it never descends into `node_modules/`, `target/`, etc. looking for nested
/// ignore files.
pub(crate) fn build_ignore_matcher(repo_root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(repo_root);
    // Two passes so all `.gitignore` rules are added before any `.travsrignore`
    // rule: later-added patterns win in gitignore semantics, so `.travsrignore`
    // (the user's Travsr-specific overrides) takes precedence.
    for target in [".gitignore", ".travsrignore"] {
        for entry in walkdir::WalkDir::new(repo_root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                // Do not descend into SKIP_DIRS — matches init and avoids a slow
                // first walk into node_modules/ on large repos.
                !(e.file_type().is_dir()
                    && e.file_name()
                        .to_str()
                        .is_some_and(|n| SKIP_DIRS.contains(&n)))
            })
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() && entry.file_name() == target {
                let _ = builder.add(entry.into_path());
            }
        }
    }
    builder.build().unwrap_or(Gitignore::empty())
}

/// True when `path` is a `.gitignore` / `.travsrignore` file whose creation,
/// edit, or removal should trigger a matcher rebuild (#403).
pub(crate) fn is_ignore_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(".gitignore" | ".travsrignore")
    )
}

/// Filter applied to ALL events (Upsert and Remove).
/// Skips SKIP_DIRS components and gitignored/travsrignored paths.
pub(crate) fn should_skip_all(path: &Path, repo_root: &Path, gitignore: &Gitignore) -> bool {
    let rel = path.strip_prefix(repo_root).unwrap_or(path);
    if rel
        .components()
        .any(|c| SKIP_DIRS.iter().any(|skip| c.as_os_str() == *skip))
    {
        return true;
    }
    // `matched_path_or_any_parents` (not `matched`) so a directory rule like
    // `vendor/` or `**/generated/` excludes the files *under* it (#403). Plain
    // `matched` only tests the exact path and never applies the parent-dir rule.
    gitignore
        .matched_path_or_any_parents(rel, false)
        .is_ignore()
}

/// Additional filter applied only to Upsert events.
/// Skips non-regular files and files with no indexable language extension.
/// NOT applied to Remove events: the file no longer exists on disk when the
/// event fires, so `is_file()` would return false for a just-deleted source file.
fn should_skip_upsert(path: &Path) -> bool {
    if !path.is_file() {
        return true;
    }
    // Skip files that are neither a recognized source/data-format extension nor
    // a name-recognized manifest (go.mod, *.csproj). Single source of truth so
    // the watcher and the init walk admit exactly the same files.
    !travsr_core::is_indexable_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #801: a SKIP_DIRS tree must not be WATCHED, not merely filtered.
    ///
    /// The distinction is the entire bug. `should_skip_all` works and always
    /// did, so every `target/` event was correctly discarded, but only AFTER
    /// being delivered, queued and stat'd. On a built repo (15,664 dirs,
    /// 251,662 files) that grew the daemon 13 MB/s until the machine thrashed.
    ///
    /// Asserting "no reindex happened" would therefore pass on the broken code
    /// too: the filter guarantees that either way. This asserts on `raw_events`,
    /// counted in the notify callback before any filtering, which is the only
    /// place the difference is observable from outside.
    ///
    /// Linux only, and not for convenience. `unwatch` needs per-directory
    /// watches, which inotify has and FSEvents does not: FSEvents streams one
    /// recursive watch from the root, and `unwatch` on a subdirectory returns
    /// `WatchNotFound` (verified against notify 8.2.0, not assumed). So this
    /// property is unachievable on the macOS default backend, and asserting it
    /// there would pin a behaviour no code can deliver. macOS is covered by
    /// `raw_event_queue_is_bounded_under_flood` below, which holds everywhere.
    /// Linux is also the platform the reported failure was measured on.
    #[cfg(target_os = "linux")]
    #[test]
    fn skip_dirs_are_not_watched_only_filtered() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("target/debug/deps")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();

        let (tx, _rx) = mpsc::channel::<WatchEvent>(1024);
        let handle = spawn(root, tx, Instant::now()).expect("watcher spawns");

        // Control arm first: a watched path must move the counter, otherwise the
        // assertion below is vacuous. Without this the whole test passes if the
        // counter simply never increments, which is the shape of guard that let
        // this ship in the first place.
        for i in 0..40 {
            std::fs::write(root.join(format!("src/f{i}.ts")), "export const x = 1;").unwrap();
        }
        wait_until(|| handle.raw_events() > 0, Duration::from_secs(5));
        let after_watched = handle.raw_events();
        assert!(
            after_watched > 0,
            "a write inside the repo must produce raw events, or this test proves nothing"
        );

        for i in 0..400 {
            std::fs::write(root.join(format!("target/debug/deps/x{i}.rlib")), "junk").unwrap();
        }
        std::thread::sleep(Duration::from_millis(600));
        let from_skip = handle.raw_events() - after_watched;
        assert!(
            from_skip < 40,
            "400 writes under target/ produced {from_skip} raw events: the tree is \
             still watched, so every file is queued and stat'd before the skip \
             filter discards it (#801). Expected the tree to be unwatched."
        );
    }

    /// #801: a full raw queue sheds rather than blocking the producer.
    ///
    /// Scope, stated because the obvious test here does not work. I first wrote
    /// this as "flood the watcher and assert it keeps accepting", and
    /// mutation-checked it by reverting to the unbounded channel: it still
    /// passed. An unbounded channel also keeps accepting, so that assertion
    /// discriminated nothing. Forcing a real overflow through the watcher is not
    /// reliable either, because the consumer drains roughly as fast as a test
    /// loop can create files, so the queue empties as fast as it fills and drop
    /// counts are timing dependent.
    ///
    /// So this pins the property that actually matters and can be asserted
    /// deterministically: when the queue is full, the producer sheds and keeps
    /// going. `send` would block here, and the producer is the notify callback,
    /// so blocking it stalls the OS event source itself: the watcher would stop
    /// noticing edits rather than merely fall behind.
    ///
    /// What this does NOT cover is the wiring, that the watcher's own channel is
    /// the bounded kind. `RAW_EVENT_CAP`'s use at the `sync_channel` call is the
    /// only place that is decided, and it is one line under review.
    #[test]
    fn a_full_raw_queue_sheds_instead_of_blocking() {
        let (tx, _rx) = std::sync::mpsc::sync_channel::<u32>(2);
        assert!(tx.try_send(1).is_ok());
        assert!(tx.try_send(2).is_ok());
        match tx.try_send(3) {
            Err(std::sync::mpsc::TrySendError::Full(v)) => assert_eq!(v, 3),
            other => panic!("a full bounded queue must report Full, got {other:?}"),
        }
        // And the producer is still usable afterwards: shedding is not fatal.
        let _ = _rx.recv().unwrap();
        assert!(
            tx.try_send(4).is_ok(),
            "after a slot frees the producer must resume, or a single burst would \
             permanently deafen the watcher"
        );
    }

    /// Poll `cond` until true or `budget` elapses. Watchers are inherently
    /// asynchronous, so a fixed sleep either flakes or wastes wall clock.
    ///
    /// Only the Linux watch test needs this, and `-D warnings` rejects it as
    /// dead code elsewhere, so it carries the same gate as its caller.
    #[cfg(target_os = "linux")]
    fn wait_until(cond: impl Fn() -> bool, budget: Duration) {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn ignore_matcher_honors_travsrignore_and_skip_dirs() {
        // #403: the incremental matcher must read .travsrignore (not just
        // .gitignore) so files init excluded stay excluded on edit, and must
        // still hard-skip SKIP_DIRS.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join(".travsrignore"), "vendor/\n**/generated/\n").unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::create_dir_all(root.join("src/generated")).unwrap();

        let m = build_ignore_matcher(root);

        assert!(
            should_skip_all(&root.join("vendor/lib.ts"), root, &m),
            "vendor/ excluded by .travsrignore must be skipped"
        );
        assert!(
            should_skip_all(&root.join("src/generated/api.ts"), root, &m),
            "**/generated/ excluded by .travsrignore must be skipped"
        );
        assert!(
            !should_skip_all(&root.join("src/app.ts"), root, &m),
            "a normal source file must not be skipped"
        );
        assert!(
            should_skip_all(&root.join("node_modules/x/index.js"), root, &m),
            "node_modules is always skipped (SKIP_DIRS)"
        );
    }

    #[test]
    fn travsrignore_takes_precedence_over_gitignore() {
        // .travsrignore is added after .gitignore, so its rules win: a negation
        // there re-includes a file .gitignore excluded (no parent dir excluded,
        // so gitignore re-inclusion semantics allow it).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join(".gitignore"), "secret.ts\n").unwrap();
        std::fs::write(root.join(".travsrignore"), "!secret.ts\n").unwrap();

        let m = build_ignore_matcher(root);
        assert!(
            !should_skip_all(&root.join("secret.ts"), root, &m),
            ".travsrignore negation must override the .gitignore exclusion"
        );
    }

    #[test]
    fn travsrignore_reincludes_gitignored_directory() {
        // #599: `.gitignore` excludes a whole subtree that `.travsrignore`
        // re-includes with `!vendored/`. init indexes it, so the watcher must
        // NOT skip edits to files under it — otherwise those files go
        // permanently stale (the state frozen at the last full init).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join(".gitignore"), "vendored/\n").unwrap();
        std::fs::write(root.join(".travsrignore"), "!vendored/\n").unwrap();
        std::fs::create_dir_all(root.join("vendored")).unwrap();

        let m = build_ignore_matcher(root);
        assert!(
            !should_skip_all(&root.join("vendored/dep.ts"), root, &m),
            "#599: .travsrignore `!vendored/` must re-include a gitignored dir so \
             the watcher keeps it fresh"
        );
    }

    /// #448: `travsr-mcp` keeps its own copy of this list so `find_pattern`
    /// searches the same file universe the walker builds the graph from. The
    /// crate dependency edge runs daemon -> mcp, so this side is the only one
    /// that can assert the two never drift.
    ///
    /// If this fails, a directory was added to or removed from one list only.
    /// Update `SKIP_DIRS` in `travsr-mcp/src/tools.rs` to match, or
    /// `find_pattern` will report matches from files the graph does not
    /// contain (or miss files it does).
    #[test]
    fn skip_dirs_matches_the_mcp_copy() {
        assert_eq!(
            SKIP_DIRS,
            travsr_mcp::SKIP_DIRS,
            "travsr-daemon and travsr-mcp SKIP_DIRS have drifted apart"
        );
    }
}
