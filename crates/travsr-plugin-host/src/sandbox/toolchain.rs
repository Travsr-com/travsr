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
        "rust" => rust_access(),
        "typescript" | "javascript" => typescript_access(),
        "python" => python_access(),
        // scip-clang reads compile_commands.json + source tree only; no module
        // caches, no network. System headers (/usr, /Library/Developer, /opt/homebrew)
        // are already readable via the base macOS sandbox profile and equivalent
        // bwrap binds on Linux. The only sandbox requirement is a writable scratch
        // dir, which is now injected via InvokeRequest::scratch.
        "c" | "cpp" => ToolchainAccess::default(),
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
    // travsr-dart-index-emitter (AOT binary) resolves the Dart SDK root as
    // ~/.travsr/ (one level above its own ~/.travsr/bin/). Grant read on
    // ~/.travsr/lib (symlinked to the real SDK lib dir) and ~/.travsr/version
    // so FolderBasedDartSdk can initialise inside the sandbox.
    if let Some(home) = home() {
        let pub_cache = home.join(".pub-cache");
        tracing::debug!(path = %pub_cache.display(), exists = pub_cache.exists(), "dart_access: pub-cache grant");
        read_paths.push(pub_cache);

        let travsr_lib = home.join(".travsr").join("lib");
        if travsr_lib.exists() {
            tracing::debug!(path = %travsr_lib.display(), "dart_access: granting read on ~/.travsr/lib (SDK lib symlink for AOT emitter)");
            read_paths.push(travsr_lib);
        }
        let travsr_version = home.join(".travsr").join("version");
        if travsr_version.exists() {
            tracing::debug!(path = %travsr_version.display(), "dart_access: granting read on ~/.travsr/version (SDK version for AOT emitter)");
            read_paths.push(travsr_version);
        }
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

    // TRAVSR_DART_EMITTER: the sidecar binary (travsr-lang-dart) finds the AOT
    // emitter via current_exe() → sibling lookup. When the sidecar is the
    // npm-installed binary (~/.nvm/.../bin/travsr-lang-dart), the sibling path
    // is ~/.nvm/.../bin/travsr-dart-index-emitter which does not exist — only
    // ~/.travsr/bin/travsr-dart-index-emitter exists (put there by `travsr lang
    // install dart`). Injecting this env var hits emitter_path() check 1,
    // bypassing the broken relative-path lookup regardless of which travsr-lang-dart
    // variant is on PATH.
    let mut env = Vec::new();
    if let Some(home) = home() {
        let emitter_bin = home
            .join(".travsr")
            .join("bin")
            .join("travsr-dart-index-emitter");
        if emitter_bin.exists() {
            tracing::debug!(
                path = %emitter_bin.display(),
                "dart_access: setting TRAVSR_DART_EMITTER"
            );
            env.push((
                "TRAVSR_DART_EMITTER".to_string(),
                emitter_bin.to_string_lossy().into_owned(),
            ));
        } else {
            tracing::debug!(
                path = %emitter_bin.display(),
                "dart_access: travsr-dart-index-emitter not found — TRAVSR_DART_EMITTER not set"
            );
        }
    }

    tracing::debug!(read_paths = ?read_paths, env_keys = ?env.iter().map(|(k,_)| k).collect::<Vec<_>>(), "dart_access: final grants");

    ToolchainAccess {
        read_paths,
        write_paths: vec![],
        env,
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

/// `scip-dotnet` resolves NuGet packages. Needs:
///   - NuGet global-packages dir (read+write) — `dotnet restore` downloads new packages here
///   - `~/.dotnet` (read) — dotnet global tools dir; scip-dotnet binary lives here
///   - dotnet runtime root (read) — non-standard for Homebrew installs on macOS
///   - `DOTNET_ROOT` env — scip-dotnet needs it when dotnet is not in /usr/local
///   - `HOME` env var — dotnet uses it for config resolution
fn csharp_access() -> ToolchainAccess {
    let mut read_paths = Vec::new();
    let mut write_paths = Vec::new();
    let mut env = Vec::new();

    // NuGet global-packages dir — restore reads cached packages AND writes new ones.
    // `dotnet nuget locals global-packages --list` prints:
    // "global-packages: /home/user/.nuget/packages"
    let nuget_packages: Option<PathBuf> =
        run_cmd_stdout("dotnet", &["nuget", "locals", "global-packages", "--list"])
            .and_then(|s| s.split_once(':').map(|(_, p)| PathBuf::from(p.trim())))
            .or_else(|| std::env::var("NUGET_PACKAGES").ok().map(PathBuf::from))
            .or_else(|| home().map(|h| h.join(".nuget").join("packages")));

    if let Some(ref p) = nuget_packages {
        tracing::debug!(path = %p.display(), exists = p.exists(), "csharp_access: NuGet packages grant (read+write)");
        read_paths.push(p.clone());
        write_paths.push(p.clone());
        env.push((
            "NUGET_PACKAGES".to_string(),
            p.to_string_lossy().into_owned(),
        ));
    }

    // ~/.dotnet — dotnet global tools live here (scip-dotnet binary + runtime shim).
    if let Some(h) = home() {
        let dotnet_dir = h.join(".dotnet");
        if dotnet_dir.exists() {
            tracing::debug!(path = %dotnet_dir.display(), "csharp_access: ~/.dotnet grant (read)");
            read_paths.push(dotnet_dir);
        }
    }

    // dotnet runtime root — required when dotnet is installed via Homebrew.
    // Homebrew: /opt/homebrew/bin/dotnet → /opt/homebrew/opt/dotnet/libexec/dotnet
    // Resolved from DOTNET_ROOT env, or by canonicalizing `which dotnet` → parent dir.
    // DOTNET_ROOT must point at the directory containing host/, sdk/, shared/.
    // Homebrew canonical path: …/Cellar/dotnet/<ver>/bin/dotnet
    //   parent(bin) → parent(install root) → join(libexec) = correct DOTNET_ROOT.
    let dotnet_root: Option<PathBuf> = std::env::var("DOTNET_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            let exe = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|d| d.join("dotnet"))
                .find(|p| p.is_file())?;
            let real = std::fs::canonicalize(&exe).ok()?;
            let libexec = real.parent()?.parent()?.join("libexec");
            if libexec.is_dir() {
                Some(libexec)
            } else {
                real.parent()
                    .and_then(|b| b.parent())
                    .map(|p| p.to_path_buf())
            }
        });
    if let Some(ref p) = dotnet_root {
        tracing::debug!(path = %p.display(), "csharp_access: DOTNET_ROOT grant (read)");
        read_paths.push(p.clone());
        env.push(("DOTNET_ROOT".to_string(), p.to_string_lossy().into_owned()));
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

/// `rust-analyzer lsif` needs read access to:
///   - `CARGO_HOME` (~/.cargo) — rust-analyzer binary + crates registry
///   - `RUSTUP_HOME` (~/.rustup) — active toolchain (used by rust-analyzer for stdlib analysis)
///   - `HOME` env so rustup/cargo can locate their homes at runtime
fn rust_access() -> ToolchainAccess {
    let mut read_paths = Vec::new();
    let mut env = Vec::new();

    let cargo_home = std::env::var("CARGO_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".cargo")));
    if let Some(ref p) = cargo_home {
        tracing::debug!(path = %p.display(), exists = p.exists(), "rust_access: CARGO_HOME grant (read)");
        read_paths.push(p.clone());
        env.push(("CARGO_HOME".to_string(), p.to_string_lossy().into_owned()));
    }

    let rustup_home = std::env::var("RUSTUP_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".rustup")));
    if let Some(ref p) = rustup_home {
        tracing::debug!(path = %p.display(), exists = p.exists(), "rust_access: RUSTUP_HOME grant (read)");
        read_paths.push(p.clone());
        env.push(("RUSTUP_HOME".to_string(), p.to_string_lossy().into_owned()));
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

/// `travsr-lsif-ts` is a Node.js binary. Node may be managed by nvm (~/.nvm) or
/// installed to a custom npm prefix (~/.npm-global). Grant both when present so
/// the sidecar can resolve `node` and its module tree inside the sandbox.
/// Homebrew node (/opt/homebrew) and system node (/usr/local) are already covered
/// by the macOS sandbox profile's base rules.
fn typescript_access() -> ToolchainAccess {
    let mut read_paths = Vec::new();
    let mut env = Vec::new();

    if let Some(home) = home() {
        let nvm_dir = std::env::var("NVM_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".nvm"));
        if nvm_dir.exists() {
            tracing::debug!(path = %nvm_dir.display(), "typescript_access: NVM_DIR grant (read)");
            read_paths.push(nvm_dir.clone());
            env.push((
                "NVM_DIR".to_string(),
                nvm_dir.to_string_lossy().into_owned(),
            ));
        }

        // Global npm prefix, e.g. ~/.npm-global — covers globally installed travsr-lsif-ts.
        let npm_global = std::env::var("NPM_CONFIG_PREFIX")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".npm-global"));
        if npm_global.exists() {
            tracing::debug!(path = %npm_global.display(), "typescript_access: npm global prefix grant (read)");
            read_paths.push(npm_global);
        }

        env.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    }

    ToolchainAccess {
        read_paths,
        write_paths: vec![],
        env,
    }
}

/// `scip-python` uses the Python runtime to resolve imports. Grant:
///   - `PYENV_ROOT` (~/.pyenv) if pyenv is in use
///   - `~/.local` — pip user-scheme install prefix (pip install --user)
fn python_access() -> ToolchainAccess {
    let mut read_paths = Vec::new();
    let mut env = Vec::new();

    if let Some(home) = home() {
        let pyenv_root = std::env::var("PYENV_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".pyenv"));
        if pyenv_root.exists() {
            tracing::debug!(path = %pyenv_root.display(), "python_access: PYENV_ROOT grant (read)");
            read_paths.push(pyenv_root.clone());
            env.push((
                "PYENV_ROOT".to_string(),
                pyenv_root.to_string_lossy().into_owned(),
            ));
        }

        // pip --user installs land in ~/.local/lib/pythonX.Y/site-packages and
        // ~/.local/bin — grant the whole prefix so the analyzer can import them.
        let local = home.join(".local");
        if local.exists() {
            tracing::debug!(path = %local.display(), "python_access: ~/.local grant (read)");
            read_paths.push(local);
        }

        env.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    }

    ToolchainAccess {
        read_paths,
        write_paths: vec![],
        env,
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
