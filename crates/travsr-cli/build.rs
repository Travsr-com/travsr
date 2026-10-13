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

    let mut files = vec![lock];
    if !git_sources(&crates, &mut files) {
        collect(&crates, &mut files);
    }
    // A crate's `tests/` and `benches/` are never compiled into the binary, so
    // an edit there must not change the id (and with it relink this crate and
    // restart the daemon).
    files.retain(|file| {
        let mut parts = file.strip_prefix(&crates).unwrap_or(file).components();
        let in_crate = parts.nth(1).and_then(|c| c.as_os_str().to_str());
        !matches!(in_crate, Some("tests" | "benches"))
    });
    files.sort();

    // `DefaultHasher` is stable for one toolchain, not across Rust releases.
    // The id only has to tell apart builds on one machine, where that holds; a
    // toolchain change in between costs one extra daemon restart.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for file in &files {
        // Relative, so where the tree is checked out does not change the id.
        file.strip_prefix(&crates).unwrap_or(file).hash(&mut hasher);
        std::fs::read(file).unwrap_or_default().hash(&mut hasher);
        // Per file, not the directory: a re-run recompiles this crate even
        // when the id comes out the same, and watching the directory re-ran on
        // any file dropped under it (a release relink of about 3 minutes).
        println!("cargo:rerun-if-changed={}", file.display());
    }
    println!("cargo:rustc-env=TRAVSR_BUILD_ID={:016x}", hasher.finish());
}

/// The files git knows under `dir`: tracked, plus untracked ones it does not
/// ignore (a module not yet added is still code). Ignored local files, such as
/// scratch examples, are not part of the build. False outside a git checkout,
/// where the caller walks the directory instead.
fn git_sources(dir: &Path, out: &mut Vec<PathBuf>) -> bool {
    let listed = std::process::Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(dir)
        .output();
    let Ok(listed) = listed else { return false };
    if !listed.status.success() || listed.stdout.is_empty() {
        return false;
    }
    for name in String::from_utf8_lossy(&listed.stdout).split_terminator('\0') {
        let path = dir.join(name);
        // A tracked file deleted from the working tree is still listed.
        if path.is_file() {
            out.push(path);
        }
    }
    true
}

/// Every file under `dir`, for a build outside git. Dotfiles are skipped: a `.DS_Store` the Finder
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
