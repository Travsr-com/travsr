//! `travsr serve` — SSE/HTTP MCP server for cloud and team deployments.
//!
//! Scans `tenants_dir` for subdirectories. Each dirname is a tenant_id.
//! Looks for `.travsr/graph.db` inside each tenant directory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;

use travsr_mcp::auth::fetch_signing_keys;
use travsr_mcp::sse::{AppState, TenantId};

/// Start the SSE MCP server on `0.0.0.0:<port>`.
///
/// Scans `tenants_dir` for tenant data. Each immediate subdirectory is treated
/// as a tenant; its `.travsr/graph.db` file (if present) is registered as the
/// single repo for that tenant.
pub async fn run(port: u16, tenants_dir: PathBuf) -> anyhow::Result<()> {
    // Build tenant_repos map by scanning tenants_dir.
    let tenant_repos: DashMap<TenantId, HashMap<String, PathBuf>> = DashMap::new();

    if tenants_dir.is_dir() {
        let read_dir = std::fs::read_dir(&tenants_dir).map_err(|e| {
            anyhow::anyhow!(
                "cannot read tenants directory {}: {e}",
                tenants_dir.display()
            )
        })?;

        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let tenant_id = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };

            let db_path = path.join(".travsr/graph.db");
            if db_path.exists() {
                let mut repos = HashMap::new();
                // Use the tenant_id as the single repo name.
                repos.insert(tenant_id.clone(), db_path);
                tenant_repos.insert(tenant_id.clone(), repos);
                tracing::info!(tenant = %tenant_id, "registered tenant");
            } else {
                tracing::debug!(
                    tenant = %tenant_id,
                    path = %path.display(),
                    "tenant directory has no .travsr/graph.db — skipped"
                );
            }
        }
    } else {
        tracing::warn!(
            path = %tenants_dir.display(),
            "tenants_dir does not exist or is not a directory — starting with zero tenants"
        );
    }

    // Fetch signing keys.
    let signing_keys = fetch_signing_keys()?;

    // Build shared state.
    let state = Arc::new(AppState::new(tenant_repos, signing_keys));

    // Build router.
    let router = travsr_mcp::sse_router(state);

    // Bind listener.
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind to {addr}: {e}"))?;

    tracing::info!(port, "SSE MCP server listening");

    axum::serve(listener, router).await?;

    Ok(())
}
