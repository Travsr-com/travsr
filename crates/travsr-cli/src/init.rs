// DEBT(travsr-010): Direct indexer/store deps bypass travsr-daemon.
// Migrate init_repo() into travsr-daemon in Sprint 2.

use std::time::Instant;

use anyhow::Context as _;
use ignore::WalkBuilder;
use travsr_indexer::Indexer;
use travsr_store::{SqliteStore, Store};

use crate::repo::find_git_root;

pub fn run() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo_root = find_git_root(&cwd)?;

    let travsr_dir = repo_root.join(".travsr");
    std::fs::create_dir_all(&travsr_dir).context("creating .travsr directory")?;

    let db_path = travsr_dir.join("graph.db");
    let mut store = SqliteStore::open(&db_path)
        .with_context(|| format!("opening graph database at {}", db_path.display()))?;

    let indexer = Indexer::new();
    let start = Instant::now();

    let mut files_indexed: u64 = 0;
    let mut nodes_written: u64 = 0;
    let mut edges_written: u64 = 0;

    let walker = WalkBuilder::new(&repo_root)
        .hidden(false)
        .git_ignore(true)
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("walk error: {err}");
                continue;
            }
        };

        let abs_path = entry.path().to_path_buf();
        if !abs_path.is_file() {
            continue;
        }

        let ext = abs_path.extension().and_then(|e| e.to_str());
        if !matches!(ext, Some("ts" | "tsx")) {
            continue;
        }

        let out = match indexer.parse_file(&abs_path) {
            Ok(o) => o,
            Err(err) => {
                tracing::warn!("skipping {}: {err}", abs_path.display());
                continue;
            }
        };

        for node in &out.nodes {
            match store.put_node(node) {
                Ok(_) => nodes_written += 1,
                Err(err) => tracing::warn!("node write error: {err}"),
            }
        }
        for edge in &out.edges {
            match store.put_edge(edge) {
                Ok(_) => edges_written += 1,
                Err(err) => tracing::warn!("edge write error: {err}"),
            }
        }
        files_indexed += 1;
    }

    let elapsed = start.elapsed();
    println!(
        "indexed {files_indexed} files, {nodes_written} nodes, {edges_written} edges \
         in {:.1}s → {}",
        elapsed.as_secs_f64(),
        db_path.display()
    );

    Ok(())
}
