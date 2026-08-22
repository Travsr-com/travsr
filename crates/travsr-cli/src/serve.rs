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
pub async fn run(host: String, port: u16, tenants_dir: PathBuf) -> anyhow::Result<()> {
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

            // #410 L2: the token side enforces `[a-z0-9-]{1,64}` on tenant ids,
            // so a directory outside that charset registers a tenant no bearer
            // token can ever match. Left unchecked it fails as a silent 401
            // with nothing pointing at the directory name. Skipping it loudly
            // is the difference between a wrong answer and a diagnosable one.
            if !travsr_mcp::is_valid_tenant_id(&tenant_id) {
                tracing::warn!(
                    dir = %tenant_id,
                    "skipping tenant directory: name must be 1-64 chars of [a-z0-9-] to match \
                     the charset bearer tokens are validated against"
                );
                continue;
            }

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
                    "tenant directory has no .travsr/graph.db, skipped"
                );
            }
        }
    } else {
        tracing::warn!(
            path = %tenants_dir.display(),
            "tenants_dir does not exist or is not a directory, starting with zero tenants"
        );
    }

    // Fetch signing keys.
    let signing_keys = fetch_signing_keys()?;

    // Build shared state.
    let state = Arc::new(AppState::new(tenant_repos, signing_keys));

    // Build router.
    let router = travsr_mcp::sse_router(state);

    // Bind listener.
    //
    // #410 L1: defaults to loopback. This server speaks plaintext HTTP and
    // authenticates with bearer tokens, so binding every interface put those
    // tokens on the LAN in cleartext for anyone running it without a TLS
    // terminator in front. Production deployments that do have one (nginx on
    // the OCI instance) pass `--host 0.0.0.0` explicitly, which is the point:
    // exposure becomes a decision rather than the default.
    let addr = format!("{host}:{port}");
    if !is_loopback(&host) {
        tracing::warn!(
            %host,
            "binding a non-loopback address with plaintext HTTP, bearer tokens will \
             cross the network in cleartext unless a TLS terminator sits in front"
        );
    }
    // M8: append a --port hint when the port is already in use so the user
    // knows exactly what to do rather than seeing a raw EADDRINUSE error.
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            anyhow::anyhow!("cannot bind to {addr}: {e}; try --port <other>")
        } else {
            anyhow::anyhow!("cannot bind to {addr}: {e}")
        }
    })?;

    tracing::info!(port, "SSE MCP server listening");

    axum::serve(listener, router).await?;

    Ok(())
}

/// Whether `host` addresses only this machine.
///
/// #410 L1: drives the plaintext-exposure warning. Parsed rather than compared
/// as a string, so `127.0.0.53` and `::1` are recognised and a bare `0.0.0.0`
/// or a public address is not. An unparseable host (a DNS name) is treated as
/// non-loopback: it may well resolve off-box, and warning on a name that
/// happens to be `localhost` is much cheaper than staying silent on one that
/// is not.
fn is_loopback(host: &str) -> bool {
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    match trimmed.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => trimmed.eq_ignore_ascii_case("localhost"),
    }
}

#[cfg(test)]
mod tests {
    use super::is_loopback;

    #[test]
    fn loopback_addresses_are_recognised() {
        assert!(is_loopback("127.0.0.1"));
        assert!(
            is_loopback("127.0.0.53"),
            "the whole 127/8 block is loopback"
        );
        assert!(is_loopback("::1"));
        assert!(is_loopback("[::1]"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("LocalHost"));
    }

    #[test]
    fn exposed_addresses_warn() {
        // The #410 L1 case: the old hardcoded default.
        assert!(!is_loopback("0.0.0.0"));
        assert!(!is_loopback("::"));
        assert!(!is_loopback("10.0.0.5"));
        assert!(!is_loopback("203.0.113.7"));
    }

    #[test]
    fn an_unresolvable_host_is_treated_as_exposed() {
        // Fail towards warning: a name we cannot resolve here may resolve
        // off-box, and a spurious warning costs less than a missed one.
        assert!(!is_loopback("mcp.internal"));
        assert!(!is_loopback(""));
    }
}
