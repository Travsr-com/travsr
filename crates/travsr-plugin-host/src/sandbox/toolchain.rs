//! Per-language toolchain access for Phase B sandboxes.
//!
//! Build-tool analyzers (`scip-go`, `scip-java`, …) drive the language's real
//! build to resolve symbols. To do that they must read their language's module /
//! build caches and see the toolchain's environment variables. The default
//! `Standard` sandbox (ADR-017 Rule 1) `env_clear`s everything and denies reads
//! outside the repo + scratch, so the analyzer resolves **zero** packages and
//! emits an empty index (observed: scip-go indexes `[0/0]` packages under the
//! sandbox vs 244 implementations unsandboxed).
//!
//! This module computes the **minimal** extra read/write paths + env each
//! language's analyzer needs, so the sandbox can grant exactly those and nothing
//! more. Languages with no out-of-repo toolchain needs (e.g. builtins) return an
//! empty grant set — a no-op.
//!
//! Extending to a new language = add a match arm with that toolchain's caches +
//! env (e.g. java → `~/.gradle`, `~/.m2`, `JAVA_HOME`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

/// Extra sandbox grants a language's Phase B analyzer needs beyond the repo +
/// scratch. All paths are host paths; the sandbox layer canonicalizes and maps
/// them per-platform (sandbox-exec subpaths on macOS, bwrap binds on Linux).
#[derive(Debug, Clone, Default)]
pub struct ToolchainAccess {
    /// Directories the analyzer must be able to **read** (toolchain, module cache).
    pub read_paths: Vec<PathBuf>,
    /// Directories the analyzer must be able to **write** (build cache).
    pub write_paths: Vec<PathBuf>,
    /// Env vars to pass through into the otherwise-cleared sandbox env.
    pub env: Vec<(String, String)>,
}

/// Compute the toolchain grants for a language's Phase B analyzer.
/// Empty for languages with no out-of-repo toolchain needs.
pub fn toolchain_access(language: &str) -> ToolchainAccess {
    match language {
        "go" => go_access(),
        "dart" => dart_access(),
        "java" | "kotlin" => java_access(),
        "scala" => scala_access(),
        "php" => php_access(),
        "csharp" => csharp_access(),
        "ruby" => ruby_access(),
        "swift" => swift_access(),
        _ => ToolchainAccess::default(),
    }
}

/// `dart run emit.dart` needs:
///   - `~/.pub-cache/` (read) — package:analyzer and its transitive deps
///   - The emitter script's package root (read) — emit.dart + .dart_tool/
///
/// The Dart SDK itself is under /opt/homebrew (macOS homebrew) which the
/// Standard sandbox already allows via `(subpath "/opt/homebrew")`.
///
/// Emitter package root is discovered by finding `travsr-lang-dart` on PATH
/// and computing `<binary-dir>/../share/travsr-lang-dart/` (installed layout).
/// Falls back to the dev path `<binary-dir>/../../packages/dart-scip-emitter/`.
fn dart_access() -> ToolchainAccess {
    let mut read_paths = Vec::new();

    // Dart pub package cache — required by package:analyzer at runtime.
    if let Some(home) = home() {
        let pub_cache = home.join(".pub-cache");
        tracing::debug!(path = %pub_cache.display(), exists = pub_cache.exists(), "dart_access: pub-cache grant");
        read_paths.push(pub_cache);
    }

    // The dart binary needs to read its own SDK snapshot files at startup
    // (e.g. dartdev_aot.dart.snapshot in <sdk>/bin/snapshots/).
    // `dart --version` works without these (version is baked into the VM) but
    // `dart run` fails if the dartdev snapshot directory is sandbox-denied.
    // Resolve symlinks so we grant the REAL bin/ dir, not just the symlink dir.
    let dart_sdk_bin = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("dart"))
        .find(|p| p.is_file())
        .and_then(|dart_bin| {
            let real = std::fs::canonicalize(&dart_bin)
                .ok()
                .and_then(|r| r.parent().map(|p| p.to_path_buf()));
            tracing::debug!(
                symlink = %dart_bin.display(),
                real_bin_dir = ?real,
                "dart_access: dart SDK bin dir"
            );
            real.or_else(|| dart_bin.parent().map(|p| p.to_path_buf()))
        });
    if let Some(sdk_bin) = dart_sdk_bin {
        tracing::debug!(path = %sdk_bin.display(), "dart_access: granting read on dart SDK bin dir (snapshots)");
        read_paths.push(sdk_bin);
    }

    // Emitter script directory: discover by resolving travsr-lang-dart on PATH.
    // The sidecar's emitter_path() checks the same relative locations.
    let dart_bin = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("travsr-lang-dart"))
        .find(|p| p.is_file());

    tracing::debug!(found = dart_bin.is_some(), path = ?dart_bin, "dart_access: travsr-lang-dart binary on PATH");

    let emitter_dir = dart_bin
    .and_then(|bin| bin.parent().map(|p| p.to_path_buf()))
    .and_then(|bin_dir| {
        // Installed: <prefix>/share/travsr-lang-dart/
        let installed = bin_dir.parent().map(|prefix| {
            prefix.join("share").join("travsr-lang-dart")
        });
        if let Some(ref p) = installed {
            tracing::debug!(path = %p.display(), exists = p.exists(), "dart_access: checking installed emitter dir");
            if p.exists() {
                return installed;
            }
        }
        // Dev: <bin_dir>/../../packages/dart-scip-emitter/
        let dev = bin_dir
            .parent()
            .and_then(|p| p.parent())
            .map(|root| root.join("packages").join("dart-scip-emitter"));
        if let Some(ref p) = dev {
            tracing::debug!(path = %p.display(), exists = p.exists(), "dart_access: checking dev emitter dir");
        }
        dev
    });

    tracing::debug!(emitter_dir = ?emitter_dir, "dart_access: resolved emitter dir");

    if let Some(dir) = emitter_dir {
        if dir.exists() {
            tracing::debug!(path = %dir.display(), "dart_access: granting read on emitter dir");
            read_paths.push(dir);
        } else {
            tracing::debug!(path = %dir.display(), "dart_access: emitter dir does not exist — not granted");
        }
    }

    tracing::debug!(read_paths = ?read_paths, "dart_access: final grants");

    ToolchainAccess {
        read_paths,
        write_paths: vec![],
        env: vec![],
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `scip-java index` drives Gradle (and Maven) to resolve dependencies. Needs:
///   - `JAVA_HOME`       (read) — JDK installation dir
///   - `~/.gradle`       (read+write) — Gradle daemon, caches, wrapper downloads
///   - `~/.m2`           (read) — Maven local repository
///   - `HOME` + `GRADLE_USER_HOME` env vars so Gradle finds its home
///
/// Shared by `"java"` and `"kotlin"` — both invoke `scip-java index`.
fn java_access() -> ToolchainAccess {
    let mut read_paths = Vec::new();
    let mut write_paths = Vec::new();
    let mut env = Vec::new();

    // JAVA_HOME: env var takes priority; fall back to `java -XshowSettings:properties`.
    let java_home: Option<PathBuf> =
        std::env::var("JAVA_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                // Output goes to stderr for this flag.
                let output = Command::new("java")
                    .args(["-XshowSettings:properties", "-version"])
                    .output()
                    .ok()?;
                let text = String::from_utf8_lossy(&output.stderr);
                text.lines()
                    .find(|l| l.contains("java.home"))
                    .and_then(|l| l.split('=').nth(1))
                    .map(|v| PathBuf::from(v.trim()))
            });
    if let Some(ref p) = java_home {
        tracing::debug!(path = %p.display(), "java_access: JAVA_HOME grant");
        read_paths.push(p.clone());
        env.push(("JAVA_HOME".to_string(), p.to_string_lossy().into_owned()));
    }

    // Gradle home: env var, then ~/.gradle.
    let gradle_home: Option<PathBuf> = std::env::var("GRADLE_USER_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".gradle")));
    if let Some(ref p) = gradle_home {
        tracing::debug!(path = %p.display(), "java_access: GRADLE_USER_HOME grant (read+write)");
        read_paths.push(p.clone());
        write_paths.push(p.clone());
        env.push((
            "GRADLE_USER_HOME".to_string(),
            p.to_string_lossy().into_owned(),
        ));
    }

    // Maven local repository (~/.m2) — read-only.
    if let Some(h) = home() {
        let m2 = h.join(".m2");
        tracing::debug!(path = %m2.display(), exists = m2.exists(), "java_access: ~/.m2 grant (read)");
        read_paths.push(m2);
    }

    if let Some(h) = home() {
        env.push(("HOME".to_string(), h.to_string_lossy().into_owned()));
    }

    ToolchainAccess {
        read_paths,
        write_paths,
        env,
    }
}

/// `scip-scala` drives sbt to resolve dependencies. Needs:
///   - `~/.ivy2` (read)       — Ivy2 artifact cache (sbt's primary resolver)
///   - `~/.sbt`  (read+write) — sbt home: launchers, plugins, boot dir
///   - `HOME` and `SBT_OPTS` env vars
fn scala_access() -> ToolchainAccess {
    let mut read_paths = Vec::new();
    let mut write_paths = Vec::new();
    let mut env = Vec::new();

    if let Some(h) = home() {
        let ivy2 = h.join(".ivy2");
        tracing::debug!(path = %ivy2.display(), exists = ivy2.exists(), "scala_access: ~/.ivy2 grant (read)");
        read_paths.push(ivy2);

        let sbt = h.join(".sbt");
        tracing::debug!(path = %sbt.display(), exists = sbt.exists(), "scala_access: ~/.sbt grant (read+write)");
        read_paths.push(sbt.clone());
        write_paths.push(sbt);

        env.push(("HOME".to_string(), h.to_string_lossy().into_owned()));
    }

    if let Ok(sbt_opts) = std::env::var("SBT_OPTS") {
        env.push(("SBT_OPTS".to_string(), sbt_opts));
    }

    ToolchainAccess {
        read_paths,
        write_paths,
        env,
    }
}

/// `scip-php` resolves Composer packages. Needs the global Composer cache:
///   - Composer home dir (read) — `composer config --global home`, then
///     `COMPOSER_HOME` env, then `~/.composer`, then `~/.config/composer`
fn php_access() -> ToolchainAccess {
    let mut read_paths = Vec::new();
    let mut env = Vec::new();

    let composer_home: Option<PathBuf> =
        run_cmd_stdout("composer", &["config", "--global", "home"])
            .map(PathBuf::from)
            .or_else(|| std::env::var("COMPOSER_HOME").ok().map(PathBuf::from))
            .or_else(|| home().map(|h| h.join(".composer")))
            .or_else(|| home().map(|h| h.join(".config").join("composer")));

    if let Some(ref p) = composer_home {
        tracing::debug!(path = %p.display(), exists = p.exists(), "php_access: COMPOSER_HOME grant (read)");
        read_paths.push(p.clone());
        env.push((
            "COMPOSER_HOME".to_string(),
            p.to_string_lossy().into_owned(),
        ));
    }

    ToolchainAccess {
        read_paths,
        write_paths: vec![],
        env,
    }
}

/// `scip-dotnet` resolves NuGet packages. Needs the global NuGet package cache:
///   - NuGet global-packages dir (read) — `dotnet nuget locals global-packages --list`,
///     then `NUGET_PACKAGES` env, then `~/.nuget/packages`
///   - `HOME` env var — dotnet uses it for config resolution
fn csharp_access() -> ToolchainAccess {
    let mut read_paths = Vec::new();
    let mut env = Vec::new();

    // `dotnet nuget locals global-packages --list` prints:
    // "global-packages: /home/user/.nuget/packages"
    let nuget_packages: Option<PathBuf> =
        run_cmd_stdout("dotnet", &["nuget", "locals", "global-packages", "--list"])
            .and_then(|s| s.split_once(':').map(|(_, p)| PathBuf::from(p.trim())))
            .or_else(|| std::env::var("NUGET_PACKAGES").ok().map(PathBuf::from))
            .or_else(|| home().map(|h| h.join(".nuget").join("packages")));

    if let Some(ref p) = nuget_packages {
        tracing::debug!(path = %p.display(), exists = p.exists(), "csharp_access: NuGet packages grant (read)");
        read_paths.push(p.clone());
        env.push((
            "NUGET_PACKAGES".to_string(),
            p.to_string_lossy().into_owned(),
        ));
    }

    if let Some(h) = home() {
        env.push(("HOME".to_string(), h.to_string_lossy().into_owned()));
    }

    ToolchainAccess {
        read_paths,
        write_paths: vec![],
        env,
    }
}

/// `scip-ruby` resolves gem dependencies. Needs the RubyGems install dirs:
///   - `GEM_HOME` (read) — primary gem dir from `gem environment gemdir`
///   - all `GEM_PATH` entries (read) — from `gem environment gempath`
///   - `HOME` env var
fn ruby_access() -> ToolchainAccess {
    let mut read_paths = Vec::new();
    let mut env = Vec::new();

    let gem_home: Option<PathBuf> = run_cmd_stdout("gem", &["environment", "gemdir"])
        .map(PathBuf::from)
        .or_else(|| std::env::var("GEM_HOME").ok().map(PathBuf::from));
    if let Some(ref p) = gem_home {
        tracing::debug!(path = %p.display(), exists = p.exists(), "ruby_access: GEM_HOME grant (read)");
        read_paths.push(p.clone());
        env.push(("GEM_HOME".to_string(), p.to_string_lossy().into_owned()));
    }

    let gem_path_str: Option<String> = run_cmd_stdout("gem", &["environment", "gempath"])
        .or_else(|| std::env::var("GEM_PATH").ok());
    if let Some(ref gp) = gem_path_str {
        for dir in gp.split(':') {
            let p = PathBuf::from(dir.trim());
            if !p.as_os_str().is_empty() {
                tracing::debug!(path = %p.display(), "ruby_access: GEM_PATH entry grant (read)");
                read_paths.push(p);
            }
        }
        env.push(("GEM_PATH".to_string(), gp.clone()));
    }

    if let Some(h) = home() {
        env.push(("HOME".to_string(), h.to_string_lossy().into_owned()));
    }

    ToolchainAccess {
        read_paths,
        write_paths: vec![],
        env,
    }
}

/// `travsr-swift-index-emitter` uses SwiftSyntax for pure parse-based indexing
/// (no compilation). Defensively grant the Xcode SDK path (via `xcrun
/// --show-sdk-path`) and the Swift toolchain bin dir in case the emitter is
/// dynamically linked against Swift stdlib dylibs. If `xcrun` is absent
/// (Linux, non-Xcode environments) we return empty grants — the emitter is
/// expected to be self-contained there.
fn swift_access() -> ToolchainAccess {
    let mut read_paths = Vec::new();

    if let Some(sdk) = run_cmd_stdout("xcrun", &["--show-sdk-path"]).map(PathBuf::from) {
        tracing::debug!(path = %sdk.display(), exists = sdk.exists(), "swift_access: Xcode SDK grant (read, defensive)");
        read_paths.push(sdk);
    } else {
        tracing::debug!("swift_access: xcrun not found — no SDK path granted");
    }

    // Swift toolchain bin dir — for dynamically loaded stdlib components.
    let swift_bin_dir = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("swift"))
        .find(|p| p.is_file())
        .and_then(|swift_bin| {
            std::fs::canonicalize(&swift_bin)
                .ok()
                .and_then(|r| r.parent().map(|p| p.to_path_buf()))
                .or_else(|| swift_bin.parent().map(|p| p.to_path_buf()))
        });
    if let Some(ref p) = swift_bin_dir {
        tracing::debug!(path = %p.display(), "swift_access: Swift toolchain bin dir grant (read)");
        read_paths.push(p.clone());
    }

    ToolchainAccess {
        read_paths,
        write_paths: vec![],
        env: vec![],
    }
}

/// Run `go env <keys>` and return key→value. Empty if `go` is not on PATH or fails.
/// `go env KEY1 KEY2` prints one value per line in argument order.
fn go_env(keys: &[&str]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(output) = Command::new("go").arg("env").args(keys).output() else {
        return out;
    };
    if !output.status.success() {
        return out;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for (k, v) in keys.iter().zip(text.lines()) {
        let v = v.trim();
        if !v.is_empty() {
            out.insert((*k).to_string(), v.to_string());
        }
    }
    out
}

/// Run an arbitrary command and return its trimmed stdout, or `None` if the
/// command is absent, fails, or produces empty output.
fn run_cmd_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// scip-go uses `go/packages` (i.e. the real `go` toolchain) to load packages, so
/// it needs: the module cache (`GOMODCACHE`, read), the build cache (`GOCACHE`,
/// read+write), `GOPATH` (covers `~/go/bin/scip-go` itself + `pkg/mod`), and the
/// `GO*`/`HOME` env so the toolchain finds them. Paths resolved via `go env`, with
/// `$HOME`-based fallbacks if `go` is not reachable at sandbox-build time.
fn go_access() -> ToolchainAccess {
    let env_map = go_env(&["GOPATH", "GOCACHE", "GOMODCACHE"]);

    let gopath = env_map
        .get("GOPATH")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join("go")));
    let gomodcache = env_map
        .get("GOMODCACHE")
        .map(PathBuf::from)
        .or_else(|| gopath.as_ref().map(|p| p.join("pkg/mod")));
    let gocache = env_map.get("GOCACHE").map(PathBuf::from).or_else(|| {
        // Default GOCACHE: macOS → $HOME/Library/Caches/go-build, Linux → $HOME/.cache/go-build.
        home().map(|h| {
            if cfg!(target_os = "macos") {
                h.join("Library/Caches/go-build")
            } else {
                h.join(".cache/go-build")
            }
        })
    });

    let mut read_paths = Vec::new();
    let mut write_paths = Vec::new();
    let mut env = Vec::new();

    if let Some(p) = &gopath {
        read_paths.push(p.clone()); // covers ~/go/bin/scip-go + pkg/mod
        env.push(("GOPATH".to_string(), p.to_string_lossy().into_owned()));
    }
    if let Some(p) = &gomodcache {
        read_paths.push(p.clone()); // modules are read-only
        env.push(("GOMODCACHE".to_string(), p.to_string_lossy().into_owned()));
    }
    if let Some(p) = &gocache {
        read_paths.push(p.clone());
        write_paths.push(p.clone()); // `go build` writes compiled artifacts here
        env.push(("GOCACHE".to_string(), p.to_string_lossy().into_owned()));
    }
    if let Some(h) = home() {
        // Go tooling consults $HOME for defaults; pass it through.
        env.push(("HOME".to_string(), h.to_string_lossy().into_owned()));
    }

    ToolchainAccess {
        read_paths,
        write_paths,
        env,
    }
}
