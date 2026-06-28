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

/// Read a file and compute its SHA-256 digest in one pass.
///
/// Returns `(digest, bytes)`. Use this instead of calling [`hash_file`] and
/// then re-reading the file: for sidecar plugins the content is passed directly
/// in `ParseRequest::source`, eliminating a second read inside the sandbox.
pub fn hash_and_read(path: &Path) -> anyhow::Result<([u8; 32], Vec<u8>)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let hash = Sha256::digest(&bytes).into();
    Ok((hash, bytes))
}
