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

pub fn run() -> anyhow::Result<()> {
    let repos = registry::all_repos()?;

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
