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

    // Auto-prune stale entries silently on every list — test temp dirs and
    // deleted repos accumulate otherwise and pollute the output for new users.
    let pruned = registry::prune().unwrap_or_default();
    if !pruned.is_empty() {
        eprintln!(
            "(auto-pruned {} stale entr{} from registry)",
            pruned.len(),
            if pruned.len() == 1 { "y" } else { "ies" }
        );
    }

    let repos = registry::all_repos()?;

    if json {
        let arr: Vec<serde_json::Value> = {
            let mut entries: Vec<(String, std::path::PathBuf)> = repos.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries
                .into_iter()
                .map(|(name, db_path)| {
                    serde_json::json!({
                        "name": name,
                        "db_path": db_path.display().to_string(),
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

    let mut rows: Vec<Row> = repos
        .into_iter()
        .map(|(name, db_path)| Row {
            exists: if db_path.exists() {
                "yes".to_string()
            } else {
                "no (stale)".to_string()
            },
            db_path: db_path.display().to_string(),
            name,
        })
        .collect();

    rows.sort_by(|a, b| a.name.cmp(&b.name));
    println!("{}", Table::new(rows));
    Ok(())
}
