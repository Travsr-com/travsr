use tabled::{Table, Tabled};
use travsr_store::registry;

#[derive(Tabled)]
struct Row {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "DB Path")]
    db_path: String,
    #[tabled(rename = "Exists")]
    exists: String,
}

/// `travsr repos [--prune] [--remove NAME] [--json]`.
///
/// Default lists registered repos as a table. `--remove` drops one entry,
/// `--prune` drops every entry whose graph.db is gone, `--json` emits machine
/// -readable output for the VS Code extension.
pub fn run(prune: bool, remove: Option<&str>, json: bool) -> anyhow::Result<()> {
    if let Some(name) = remove {
        if registry::unregister(name)? {
            println!("removed repo '{name}'");
        } else {
            println!("no repo named '{name}' in the registry");
        }
        return Ok(());
    }

    if prune {
        let removed = registry::prune()?;
        if removed.is_empty() {
            println!("registry is clean — no stale repos to prune");
        } else {
            println!("pruned {} stale repo(s):", removed.len());
            for name in &removed {
                println!("  - {name}");
            }
        }
        return Ok(());
    }

    // UX-017: a plain `repos` list is a read command — it must not silently mutate
    // the registry. (It previously auto-pruned every stale entry, so `repos`
    // deleted hundreds of rows as a surprising side effect.) Stale entries are
    // shown with a marker below and a one-line hint points at `repos --prune`.
    let repos = registry::all_repos()?;

    // UX-018: strip the Windows `\\?\` verbatim prefix from any already-registered
    // entry (new registrations are normalized at write time) so the display is
    // consistent and never leaks the extended-length path form.
    let clean = |s: &str| registry::strip_verbatim_prefix(s).into_owned();

    if json {
        let arr: Vec<serde_json::Value> = {
            let mut entries: Vec<(String, std::path::PathBuf)> = repos.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries
                .into_iter()
                .map(|(name, db_path)| {
                    serde_json::json!({
                        "name": clean(&name),
                        "db_path": clean(&db_path.display().to_string()),
                        "exists": db_path.exists(),
                    })
                })
                .collect()
        };
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    if repos.is_empty() {
        println!("no repos registered — run `travsr init` in a repo first");
        return Ok(());
    }

    let stale = repos.values().filter(|p| !p.exists()).count();
    let mut rows: Vec<Row> = repos
        .into_iter()
        .map(|(name, db_path)| Row {
            exists: if db_path.exists() {
                "yes".to_string()
            } else {
                "no (stale)".to_string()
            },
            db_path: clean(&db_path.display().to_string()),
            name: clean(&name),
        })
        .collect();

    rows.sort_by(|a, b| a.name.cmp(&b.name));
    println!("{}", Table::new(rows));
    if stale > 0 {
        println!(
            "\n{stale} stale entr{} (db missing) — run `travsr repos --prune` to remove",
            if stale == 1 { "y" } else { "ies" }
        );
    }
    Ok(())
}
