use std::path::Path;

use anyhow::Context as _;
use sha2::{Digest, Sha256};

/// Compute the SHA-256 digest of a file's contents.
///
/// Returns the raw 32-byte digest. Callers that need a hex string can use
/// `hex::encode` or format it manually with `{:02x}` formatting.
pub fn hash_file(path: &Path) -> anyhow::Result<[u8; 32]> {
    let bytes = std::fs::read(path).with_context(|| format!("hashing {}", path.display()))?;
    Ok(Sha256::digest(bytes).into())
}
