//! `travsr rerank install` / `travsr rerank status` — RFC-021 P5 model delivery.
//!
//! Thin CLI surface over `travsr_mcp`'s model-distribution logic, which is the
//! single source of truth (the daemon auto-fetches through the same code on
//! warm). The cross-encoder model is byte-identical across travsr versions, so
//! it lives on its own immutable `rerank-model-v1` GitHub release and is fetched
//! into `~/.travsr/models/rerank` — the daemon's default discovery location —
//! with a committed sha256 pin. A missing model just leaves the reranker in its
//! ships-dark lexical fallback.

use anyhow::Result;

#[derive(Debug, clap::Subcommand)]
pub enum RerankCommand {
    /// Download the cross-encoder reranker model into ~/.travsr/models/rerank.
    Install,
    /// Show whether the reranker model is installed.
    Status,
}

pub fn run(cmd: RerankCommand) -> Result<()> {
    match cmd {
        RerankCommand::Install => {
            let dir = travsr_mcp::install_rerank_model()?;
            println!("\u{2713} reranker model installed to {}", dir.display());
            println!("  restart the daemon to activate it: travsr daemon restart");
            Ok(())
        }
        RerankCommand::Status => {
            if travsr_mcp::rerank_model_installed() {
                println!("reranker model: installed");
            } else {
                println!(
                    "reranker model: not installed — run `travsr rerank install` (the daemon \
                     also auto-fetches it on start unless TRAVSR_NO_RERANK is set)"
                );
            }
            Ok(())
        }
    }
}
