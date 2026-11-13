//! Shared helpers for this crate's integration tests.

use std::path::PathBuf;

/// Path to a binary built by another crate in this workspace (the `travsr`
/// CLI lives in `travsr-cli`, not here).
///
/// Cargo only sets `CARGO_BIN_EXE_<name>` for the binaries of the crate under
/// test, so `assert_cmd::cargo::cargo_bin` falls back to looking next to the
/// test executable. Since Cargo 1.100 test executables live in a separate
/// build directory, so that fallback finds nothing. `CARGO_TARGET_TMPDIR` is
/// always `<target-dir>/tmp`, which puts the final binaries one level up.
pub fn cargo_bin(name: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(format!("CARGO_BIN_EXE_{name}")) {
        return path.into();
    }
    let target = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .map(PathBuf::from)
        .expect("invariant: CARGO_TARGET_TMPDIR is <target-dir>/tmp");
    let path = target
        .join("debug")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "{} not found; build it first (cargo test --workspace, or cargo build -p travsr-cli)",
        path.display()
    );
    path
}
