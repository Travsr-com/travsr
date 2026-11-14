//! Shared helpers for this crate's integration tests.

use std::path::PathBuf;

/// Path to a binary built by another crate in this workspace (the `travsr`
/// CLI lives in `travsr-cli`, not here).
///
/// Cargo only sets `CARGO_BIN_EXE_<name>` for the binaries of the crate under
/// test, so `assert_cmd::cargo::cargo_bin` falls back to looking next to the
/// test executable. Since Cargo 1.100 test executables live in a separate
/// build directory, so that fallback finds nothing. `CARGO_TARGET_TMPDIR` is
/// always `<target-dir>/tmp`, which puts the final binaries one level up, in
/// `<target-dir>/<profile>/` or, for a `--target` build,
/// `<target-dir>/<triple>/<profile>/`.
///
/// The profile is the one this test was built with (`release` under
/// `cargo test --release`, else `debug`); when more than one candidate exists
/// (a host and a `--target` build side by side) the newest is used, and a
/// custom `--profile` build is found by searching every profile folder.
pub fn cargo_bin(name: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(format!("CARGO_BIN_EXE_{name}")) {
        return path.into();
    }
    let target = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .map(PathBuf::from)
        .expect("invariant: CARGO_TARGET_TMPDIR is <target-dir>/tmp");
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let mut dirs = vec![target.join(profile)];
    if let Ok(entries) = std::fs::read_dir(&target) {
        dirs.extend(
            entries
                .filter_map(Result::ok)
                .map(|e| e.path().join(profile))
                .filter(|d| d.is_dir()),
        );
    }
    let newest = |paths: Vec<PathBuf>| {
        paths
            .into_iter()
            .filter(|p| p.is_file())
            .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
    };
    // A custom `--profile` builds into a folder named after it, which the test
    // cannot name; fall back to any profile folder under the target dir
    // (one level, or two for a --target triple), newest first.
    let subdirs = |d: &std::path::Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default()
    };
    let found = newest(dirs.into_iter().map(|d| d.join(&file)).collect()).or_else(|| {
        let mut any = Vec::new();
        for d in subdirs(&target) {
            any.push(d.join(&file));
            any.extend(subdirs(&d).into_iter().map(|dd| dd.join(&file)));
        }
        newest(any)
    });
    found.unwrap_or_else(|| {
        panic!(
            "{file} not found under {} for the {profile} profile; build it first \
             (cargo test --workspace, or cargo build -p travsr-cli)",
            target.display()
        )
    })
}
