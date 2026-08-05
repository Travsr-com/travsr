//! Cross-platform executable resolution (#502).
//!
//! On Windows, executables carry `PATHEXT` extensions (`scip-go.exe`,
//! `npm.cmd`), so probing `dir.join(name)` with the bare name never finds
//! them. Worse, npm installs a bare extensionless POSIX `#!/bin/sh` shim
//! *next to* its `.cmd` wrapper, so a bare-name probe can "succeed" with a
//! file `CreateProcessW` rejects ("not a valid Win32 application").
//!
//! [`resolve_executable`] and [`resolve_executable_in`] are the one shared
//! lookup every caller should use: they try each `PATHEXT` candidate,
//! preferring natively spawnable images (`.exe`, `.com`) over shell shims
//! (`.cmd`, `.bat`), and try the bare name only as the last resort. The
//! resolved `.cmd`/`.bat` path can be handed straight to
//! `std::process::Command`, whose standard library spawns batch files via
//! `cmd.exe` with strict argument escaping (Rust's CVE-2024-24576 handling).
//!
//! On non-Windows platforms both functions degrade to the classic bare-name
//! `is_file()` probe.

use std::path::PathBuf;

/// Candidate file names for `name` on this platform, best-first.
#[cfg(windows)]
fn candidate_names(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let has_ext = std::path::Path::new(name).extension().is_some();
    if has_ext {
        // Already explicit (e.g. "travsr.exe") — honour it first.
        out.push(name.to_string());
    }

    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let mut exts: Vec<String> = pathext
        .split(';')
        .map(str::trim)
        .filter(|e| e.starts_with('.'))
        .map(str::to_ascii_lowercase)
        .collect();
    // Natively spawnable images first, shell shims after: CreateProcessW can
    // exec .exe/.com directly; .cmd/.bat go through cmd.exe.
    exts.sort_by_key(|e| match e.as_str() {
        ".exe" => 0,
        ".com" => 1,
        ".cmd" => 2,
        ".bat" => 3,
        _ => 4,
    });
    exts.dedup();
    for ext in &exts {
        out.push(format!("{name}{ext}"));
    }

    if !has_ext {
        // Bare extensionless file LAST: on Windows it is usually npm's POSIX
        // shim, which is not a valid Win32 executable (#502 item 4).
        out.push(name.to_string());
    }
    out
}

#[cfg(not(windows))]
fn candidate_names(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

/// Resolve `name` to an executable file inside `dirs`, searched in order.
///
/// Directory order wins over extension order (PATH semantics): the first
/// directory containing any candidate is used, and within a directory the
/// best candidate per [`candidate_names`] is picked.
pub fn resolve_executable_in<I, P>(dirs: I, name: &str) -> Option<PathBuf>
where
    I: IntoIterator<Item = P>,
    P: Into<PathBuf>,
{
    let names = candidate_names(name);
    for dir in dirs {
        let dir: PathBuf = dir.into();
        for n in &names {
            let candidate = dir.join(n);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve `name` to an executable file on `PATH`.
pub fn resolve_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    resolve_executable_in(std::env::split_paths(&path), name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "travsr-exec-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn missing_binary_resolves_to_none() {
        let dir = unique_dir("none");
        assert_eq!(resolve_executable_in([dir], "definitely-not-here"), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_finds_exe_from_bare_name() {
        let dir = unique_dir("exe");
        fs::write(dir.join("scip-go.exe"), b"MZ").unwrap();
        assert_eq!(
            resolve_executable_in([dir.clone()], "scip-go"),
            Some(dir.join("scip-go.exe"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefers_exe_over_cmd() {
        let dir = unique_dir("exe-over-cmd");
        fs::write(dir.join("tool.cmd"), b"@echo off\r\n").unwrap();
        fs::write(dir.join("tool.exe"), b"MZ").unwrap();
        assert_eq!(
            resolve_executable_in([dir.clone()], "tool"),
            Some(dir.join("tool.exe"))
        );
    }

    /// #502 item 4: npm installs a bare POSIX shim next to the .cmd wrapper.
    /// The .cmd must win — the bare shim is not a valid Win32 executable.
    #[cfg(windows)]
    #[test]
    fn windows_prefers_cmd_over_bare_posix_shim() {
        let dir = unique_dir("cmd-over-bare");
        fs::write(dir.join("pyright"), b"#!/bin/sh\n").unwrap();
        fs::write(dir.join("pyright.cmd"), b"@echo off\r\n").unwrap();
        assert_eq!(
            resolve_executable_in([dir.clone()], "pyright"),
            Some(dir.join("pyright.cmd"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_falls_back_to_bare_name_when_nothing_else_exists() {
        let dir = unique_dir("bare-last");
        fs::write(dir.join("odd-tool"), b"MZ").unwrap();
        assert_eq!(
            resolve_executable_in([dir.clone()], "odd-tool"),
            Some(dir.join("odd-tool"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_explicit_extension_is_honoured() {
        let dir = unique_dir("explicit-ext");
        fs::write(dir.join("travsr.exe"), b"MZ").unwrap();
        assert_eq!(
            resolve_executable_in([dir.clone()], "travsr.exe"),
            Some(dir.join("travsr.exe"))
        );
    }

    /// PATH semantics: the first directory wins even if a later directory
    /// holds a "better" extension.
    #[test]
    fn earlier_directory_wins() {
        let first = unique_dir("dir-order-1");
        let second = unique_dir("dir-order-2");
        let (first_name, second_name) = if cfg!(windows) {
            ("tool.cmd", "tool.exe")
        } else {
            ("tool", "tool")
        };
        fs::write(first.join(first_name), b"x").unwrap();
        fs::write(second.join(second_name), b"x").unwrap();
        assert_eq!(
            resolve_executable_in([first.clone(), second], "tool"),
            Some(first.join(first_name))
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_finds_bare_name() {
        let dir = unique_dir("unix-bare");
        fs::write(dir.join("scip-go"), b"#!/bin/sh\n").unwrap();
        assert_eq!(
            resolve_executable_in([dir.clone()], "scip-go"),
            Some(dir.join("scip-go"))
        );
    }
}
