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
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use travsr_core::Language;

/// Events the daemon's indexer worker receives from the watcher.
#[derive(Debug)]
pub enum WatchEvent {
    Upsert(PathBuf),
    Remove(PathBuf),
}

/// A handle to the running watcher threads. Drop to signal shutdown.
pub struct WatcherHandle {
    stop: Arc<AtomicBool>,
    _event_thread: std::thread::JoinHandle<()>,
    _flush_thread: std::thread::JoinHandle<()>,
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

const SKIP_DIRS: &[&str] = &[
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
    let gitignore = build_gitignore(&repo_root);
    let stop = Arc::new(AtomicBool::new(false));

    // Pending debounce table shared between the notify callback and the flush thread.
    let pending: Arc<Mutex<HashMap<PathBuf, (PendingKind, Instant)>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Synchronous channel from the notify callback to our processing loop.
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<notify::Event>();

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
    let stop_event = Arc::clone(&stop);
    let event_thread = std::thread::Builder::new()
        .name("travsr-watcher-event".into())
        .spawn(move || {
            let mut watcher = match RecommendedWatcher::new(
                move |res: notify::Result<notify::Event>| {
                    if let Ok(ev) = res {
                        let _ = raw_tx.send(ev);
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

            // Signal ready — watch is established, caller can proceed.
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
                        for path in &event.paths {
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

                            if should_skip(path, &repo_root, &gitignore) {
                                continue;
                            }

                            // Name(From) is the "old path" half of a rename — treat
                            // it as Remove so deleted nodes are tombstoned.
                            // Name(To) and all other events are Upsert.
                            let kind = match event.kind {
                                EventKind::Remove(_)
                                | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                                    PendingKind::Remove
                                }
                                _ => PendingKind::Upsert,
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
                                                "watcher debounce table full — new paths \
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
        _event_thread: event_thread,
        _flush_thread: flush_thread,
    })
}

fn build_gitignore(repo_root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(repo_root);
    for entry in walkdir::WalkDir::new(repo_root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() && entry.file_name() == ".gitignore" {
            let _ = builder.add(entry.into_path());
        }
    }
    builder.build().unwrap_or(Gitignore::empty())
}

fn should_skip(path: &Path, repo_root: &Path, gitignore: &Gitignore) -> bool {
    // Skip directory components in the SKIP_DIRS list.
    let rel = path.strip_prefix(repo_root).unwrap_or(path);
    if rel
        .components()
        .any(|c| SKIP_DIRS.iter().any(|skip| c.as_os_str() == *skip))
    {
        return true;
    }
    // Skip gitignored paths.
    if gitignore.matched(rel, path.is_dir()).is_ignore() {
        return true;
    }
    // Sockets, directories, FIFOs, and other non-regular entries have no
    // indexable content — skip them rather than letting them fall through.
    if !path.is_file() {
        return true;
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    Language::from_extension(ext).is_none()
}
