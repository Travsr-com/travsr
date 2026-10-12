//! #974: stamp the binary with a digest of the workspace sources, so the daemon
//! skew check can tell two builds of one version apart.
//!
//! Computed here, not read from the executable at run time: hashing a 66 MB
//! release binary added 0.1 s to every daemon-routed command, and its byte
//! length, the cheap alternative, came out equal for two builds of different
//! code. Same sources give the same id, so a debug and a release build of one
//! tree agree, and so do two copies of one binary.

use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let crates = Path::new(&manifest_dir).join("..");
    let lock = crates.join("../Cargo.lock");

    let mut files = vec![lock.clone()];
    collect(&crates, &mut files);
    files.sort();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for file in &files {
        // Relative, so where the tree is checked out does not change the id.
        file.strip_prefix(&crates).unwrap_or(file).hash(&mut hasher);
        std::fs::read(file).unwrap_or_default().hash(&mut hasher);
    }
    println!("cargo:rustc-env=TRAVSR_BUILD_ID={:016x}", hasher.finish());
    println!("cargo:rerun-if-changed={}", crates.display());
    println!("cargo:rerun-if-changed={}", lock.display());
}

/// Every file under `dir`. Dotfiles are skipped: a `.DS_Store` the Finder
/// drops into a crate is not a change to the code.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect(&path, out);
        } else {
            out.push(path);
        }
    }
}
