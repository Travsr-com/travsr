//! Static catalog of every known Phase B tool.
//! Adding a new language requires only a new entry — no code changes elsewhere.
//! None are active by default; users enable via `travsr lang install <lang>`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Lsif,
    Scip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxRequirement {
    /// No network, no build steps that download dependencies.
    Standard,
    /// Needs POSIX IPC queues/shm (e.g. scip-clang parallel workers) but not network.
    /// macOS sandbox-exec has no valid Seatbelt operation for mq_open; this policy
    /// bypasses sandbox-exec and applies ulimit caps only. No PSE approval required.
    NativeIpc,
    /// Needs network for dependency resolution (Maven/Gradle/NuGet/sbt).
    // ADR-017 Rule 1: RequiresElevated languages need an explicit host allowlist
    // recorded in ~/.travsr/lang.toml before the sandbox will permit network access.
    // Never surface "ADR-017" in user-facing output — use plain language instead.
    RequiresElevated,
}

/// Specifies a pre-built binary that travsr can download from a GitHub release.
#[derive(Debug, Clone, Copy)]
pub struct ScipBinarySpec {
    /// GitHub repo slug, e.g. `"sourcegraph/scip-java"`.
    pub repo: &'static str,
    /// Map a release tag and Rust target triple to the asset filename on the release page.
    /// Return `None` when no binary exists for the given platform.
    pub asset_fn: fn(tag: &str, target: &str) -> Option<String>,
    /// Binary name after installation in `~/.travsr/bin/`.
    pub install_name: &'static str,
    /// Full release tag used as fallback when the GitHub API is unreachable.
    pub version_fallback: &'static str,
    /// Whether the release page includes a `.sha256` sidecar file for integrity checking.
    pub verify_sha256: bool,
    /// #410 M2: expected sha256 for `(tag, target)`, for upstreams that publish
    /// no sidecar. `None` when the entry relies on a sidecar or on TLS alone.
    ///
    /// A vendored hash is only meaningful against a fixed asset, so an entry
    /// that supplies one is also pinned to `version_fallback` rather than
    /// resolving `releases/latest` — see `install_scip_github_binary`.
    pub sha256_fn: Option<fn(tag: &str, target: &str) -> Option<&'static str>>,
}

/// Specifies a zip archive on GitHub Releases that must be extracted rather than
/// installed directly as a binary (e.g. kotlin-language-server ships `server.zip`).
#[derive(Debug, Clone, Copy)]
pub struct ZipBinarySpec {
    /// GitHub repo slug, e.g. `"fwcd/kotlin-language-server"`.
    pub repo: &'static str,
    /// Map a release tag to the zip asset filename. Takes only `tag` (not target)
    /// because platform-independent zip archives don't vary by host triple.
    pub asset_fn: fn(tag: &str) -> String,
    /// Subdirectory under `~/.travsr/` to extract the zip into (e.g. `"kls"`).
    pub extract_dir: &'static str,
    /// Path to the actual binary within the extracted directory, used to write
    /// the `~/.travsr/bin/<install_name>` wrapper script (e.g. `"server/bin/kotlin-language-server"`).
    pub binary_subpath: &'static str,
    /// Wrapper script name placed in `~/.travsr/bin/`.
    pub install_name: &'static str,
    /// Fallback version tag when the GitHub API is unreachable.
    pub version_fallback: &'static str,
    /// #410 M2: expected sha256 of the zip for `tag`, for upstreams that
    /// publish no sidecar. Pins the entry to `version_fallback` when present.
    pub sha256_fn: Option<fn(tag: &str) -> Option<&'static str>>,
}

/// How to install the underlying SCIP tool once the travsr-lang wrapper is present.
#[derive(Debug, Clone, Copy)]
pub enum ScipInstall {
    /// Run this command to install the underlying tool automatically.
    Command(&'static [&'static str]),
    /// Download a pre-built binary directly from the tool's GitHub Releases.
    GithubBinary(ScipBinarySpec),
    /// Download a zip archive from GitHub Releases, extract it, and create a
    /// wrapper script in `~/.travsr/bin/`. Used for tools that ship as archives
    /// rather than standalone binaries (e.g. kotlin-language-server).
    ZipBinary(ZipBinarySpec),
    /// No automated install available; show `underlying_tool_hint` to the user.
    Manual,
}

// ── asset resolution functions ────────────────────────────────────────────────

/// scip-java ships a single platform-agnostic binary; the version is part of the name.
pub fn scip_java_asset(tag: &str, _target: &str) -> Option<String> {
    Some(format!("scip-java-{tag}"))
}

/// scip-ruby ships arm64-darwin and x86_64-linux binaries (no version in asset name).
pub fn scip_ruby_asset(_tag: &str, target: &str) -> Option<String> {
    match target {
        "aarch64-apple-darwin" => Some("scip-ruby-arm64-darwin".to_string()),
        "x86_64-unknown-linux-gnu" => Some("scip-ruby-x86_64-linux".to_string()),
        _ => None,
    }
}

/// #410 M2: vendored sha256 for pinned third-party assets whose upstreams
/// publish no `.sha256` sidecar (kotlin-language-server, scip-ruby,
/// scip-clang). scip-java is absent from this list on purpose — it does ship a
/// sidecar, so it verifies through that instead.
///
/// Each value is the asset GitHub served for that exact tag, fetched over TLS
/// on 2026-08-10. Pinned and checked, this detects an asset being replaced
/// after the fact, which TLS alone does not. It does not attest that anyone
/// reviewed the binary — a hash recorded this way is a fixity check, not a
/// provenance one, and should not be read as the latter.
pub fn kls_sha256(tag: &str) -> Option<&'static str> {
    match tag {
        "1.3.13" => Some("4fe7d71d087b307c7869036171bd9d8c6a4284cd7c25b89098b0a24eb2d9b6d2"),
        _ => None,
    }
}

pub fn scip_ruby_sha256(tag: &str, target: &str) -> Option<&'static str> {
    match (tag, target) {
        ("scip-ruby-v0.4.7", "aarch64-apple-darwin") => {
            Some("6a2bcda64ed385f0e99e92f9c5693296dc38325e4ed5ca91cd8e4b686ba14fb1")
        }
        ("scip-ruby-v0.4.7", "x86_64-unknown-linux-gnu") => {
            Some("a068c7c3b2042b9eac563ce77ce35dcaca666b418530b1db9f932a3dbc7175dd")
        }
        _ => None,
    }
}

pub fn scip_clang_sha256(tag: &str, target: &str) -> Option<&'static str> {
    match (tag, target) {
        ("v0.4.0", "aarch64-apple-darwin") => {
            Some("ff042fbc8a029f09f4b69fc7692e290e21c52923593207ee52d4e7439473ec64")
        }
        ("v0.4.0", "x86_64-unknown-linux-gnu") => {
            Some("06fd18c576f979a726c651594644ec4a35db4f471f2160b3f72eb89fa6001784")
        }
        _ => None,
    }
}

/// kotlin-language-server ships a single platform-independent `server.zip`.
pub fn kls_asset(_tag: &str) -> String {
    "server.zip".to_string()
}

/// scip-clang ships arm64-darwin and x86_64-linux binaries (no version in asset name).
pub fn scip_clang_asset(_tag: &str, target: &str) -> Option<String> {
    match target {
        "aarch64-apple-darwin" => Some("scip-clang-arm64-darwin".to_string()),
        "x86_64-unknown-linux-gnu" => Some("scip-clang-x86_64-linux".to_string()),
        _ => None,
    }
}

/// travsr-swift-index-emitter ships arm64-darwin and x86_64-linux binaries.
pub fn swift_index_emitter_asset(_tag: &str, target: &str) -> Option<String> {
    match target {
        "aarch64-apple-darwin" => {
            Some("travsr-swift-index-emitter-aarch64-apple-darwin".to_string())
        }
        "x86_64-unknown-linux-gnu" => {
            Some("travsr-swift-index-emitter-x86_64-unknown-linux-gnu".to_string())
        }
        _ => None,
    }
}

/// travsr-dart-index-emitter ships arm64-darwin and x86_64-linux binaries (AOT-compiled via dart compile exe).
pub fn dart_scip_emitter_asset(_tag: &str, target: &str) -> Option<String> {
    match target {
        "aarch64-apple-darwin" => {
            Some("travsr-dart-index-emitter-aarch64-apple-darwin".to_string())
        }
        "x86_64-unknown-linux-gnu" => {
            Some("travsr-dart-index-emitter-x86_64-unknown-linux-gnu".to_string())
        }
        _ => None,
    }
}

/// travsr-lang-objectivec — macOS-only (libclang-based); aarch64 and x86_64 Apple Darwin.
pub fn objc_index_emitter_asset(_tag: &str, target: &str) -> Option<String> {
    match target {
        "aarch64-apple-darwin" => Some("travsr-lang-objectivec-aarch64-apple-darwin".to_string()),
        "x86_64-apple-darwin" => Some("travsr-lang-objectivec-x86_64-apple-darwin".to_string()),
        _ => None,
    }
}

#[derive(Debug)]
pub struct PhaseBEntry {
    pub language: &'static str,
    /// npm package distributed via the travsr-lang repo (e.g. `@travsr-plugin/rust`).
    /// None for languages not yet published to npm.
    pub npm_package: Option<&'static str>,
    /// Underlying tool binary name (checked on PATH after install).
    pub command: &'static str,
    /// Args passed to binary. `{root}` = repo root. `{tsconfig}` = root/tsconfig.json.
    pub args: &'static [&'static str],
    pub output_format: OutputFormat,
    pub sandbox: SandboxRequirement,
    /// Shown by `travsr lang list` and `travsr lang install` when tool is absent.
    pub install_hint: &'static str,
    /// How to install the underlying tool (scip-*, rust-analyzer, etc.) when the
    /// travsr-lang-* wrapper is already installed but the tool itself is missing.
    /// Shown in the "wrapper-only" state by `travsr lang list`.
    /// Empty string for builtins where no separate tool install is needed.
    pub underlying_tool_hint: &'static str,
    /// The travsr-lang binary name for this language, e.g. "travsr-lang-go".
    /// None for in-tree builtins (rust, typescript) that spawn via __plugin.
    pub provider_binary: Option<&'static str>,
    /// For RequiresElevated languages: the network hosts their build tool contacts.
    /// Empty for Standard sandbox languages.
    pub elevated_hosts: &'static [&'static str],
    /// How to install the underlying SCIP tool automatically, if possible.
    pub scip_install: ScipInstall,
    /// File extensions used to detect this language in a repo (each with leading dot).
    pub extensions: &'static [&'static str],
    /// Fallback version string used by `travsr lang install` when the GitHub API
    /// is unreachable. Keep in sync with the latest published release.
    pub wrapper_version_fallback: &'static str,
    /// True for in-tree builtins (typescript, javascript) that are bundled with
    /// the travsr binary — no external binary on PATH required.
    pub builtin: bool,
    /// True when this language has a native (in-process, zero-install) Phase B
    /// implementation compiled into the daemon. When true, `lang list` shows
    /// "✓ active" even if the external enrichment tool is absent — the external
    /// tool is an optional upgrade, not a requirement for call-edge indexing.
    pub native_phase_b: bool,
    /// True when `travsr lang install` must also download a platform-independent
    /// `<provider_binary>-share.tar.gz` asset and extract it into
    /// ~/.travsr/share/<provider_binary>/. Used by languages whose sidecar
    /// spawns an external script rather than a compiled tool (e.g. dart).
    pub has_share_assets: bool,
}

pub static CATALOG: &[PhaseBEntry] = &[
    PhaseBEntry {
        language: "typescript",
        npm_package: Some("@travsr-plugin/typescript"),
        command: "travsr-lsif-ts",
        args: &["--project", "{tsconfig}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "travsr lang install typescript",
        underlying_tool_hint: "",
        provider_binary: None,
        elevated_hosts: &[],
        scip_install: ScipInstall::Command(&["npm", "install", "-g", "@travsr-plugin/typescript"]),
        extensions: &[".ts", ".tsx"],
        wrapper_version_fallback: "v0.1.0",
        builtin: true,
        native_phase_b: true,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "javascript",
        npm_package: Some("@travsr-plugin/typescript"),
        command: "travsr-lsif-ts",
        args: &["--project", "{tsconfig}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "travsr lang install javascript",
        underlying_tool_hint: "",
        provider_binary: None,
        elevated_hosts: &[],
        scip_install: ScipInstall::Command(&["npm", "install", "-g", "@travsr-plugin/typescript"]),
        extensions: &[".js", ".jsx", ".mjs", ".cjs"],
        wrapper_version_fallback: "v0.1.0",
        builtin: true,
        native_phase_b: true,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "rust",
        npm_package: Some("@travsr-plugin/rust"),
        command: "rust-analyzer",
        args: &["lsif", "{root}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "travsr lang install rust",
        underlying_tool_hint: "rustup component add rust-analyzer",
        provider_binary: None,
        elevated_hosts: &[],
        scip_install: ScipInstall::Command(&["rustup", "component", "add", "rust-analyzer"]),
        extensions: &[".rs"],
        wrapper_version_fallback: "v0.1.0",
        builtin: true,
        native_phase_b: true,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "go",
        npm_package: Some("@travsr-plugin/go"),
        command: "scip-go",
        args: &["--output", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "travsr lang install go  (or: go install github.com/scip-code/scip-go/cmd/scip-go@latest)",
        underlying_tool_hint: "go install github.com/scip-code/scip-go/cmd/scip-go@latest",
        provider_binary: Some("travsr-lang-go"),
        elevated_hosts: &[],
        scip_install: ScipInstall::Command(&[
            "go",
            "install",
            "github.com/scip-code/scip-go/cmd/scip-go@latest",
        ]),
        extensions: &[".go"],
        wrapper_version_fallback: "v0.1.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "python",
        npm_package: None,
        // travsr-lsif-py is bundled with the travsr binary (packages/travsr-lsif-py/).
        // It is resolved via current_exe walk-up — no PATH install required.
        // The `command` name is used by `travsr lang list` for display only; the
        // actual invocation goes through run_lsif_py_emitter() in-process.
        command: "travsr-lsif-py",
        args: &["--root", "{root}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "bundled — ships with travsr (packages/travsr-lsif-py)",
        underlying_tool_hint: "",
        provider_binary: None,
        elevated_hosts: &[],
        scip_install: ScipInstall::Command(&["npm", "install", "-g", "@travsr-plugin/python"]),
        extensions: &[".py", ".pyi"],
        wrapper_version_fallback: "v0.1.0",
        builtin: true,
        native_phase_b: true,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "java",
        npm_package: Some("@travsr-plugin/java"),
        command: "scip-java",
        args: &["index", "--output", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "travsr lang install java  (security approval required — run interactively)",
        underlying_tool_hint: "https://github.com/sourcegraph/scip-java/releases — download scip-java-<version> and place in ~/.travsr/bin/scip-java (chmod +x)",
        provider_binary: Some("travsr-lang-java"),
        elevated_hosts: &[
            "repo1.maven.org",
            "repo.maven.apache.org",
            "plugins.gradle.org",
            "jcenter.bintray.com",
        ],
        scip_install: ScipInstall::GithubBinary(ScipBinarySpec {
            repo: "sourcegraph/scip-java",
            asset_fn: scip_java_asset,
            install_name: "scip-java",
            version_fallback: "v0.12.3",
            verify_sha256: true,
            sha256_fn: None,
        }),
        extensions: &[".java"],
        wrapper_version_fallback: "v0.1.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "kotlin",
        npm_package: Some("@travsr-plugin/kotlin"),
        // The sidecar drives kotlin-language-server (KLS) over LSP — build-system
        // agnostic: KLS auto-detects Maven or Gradle and resolves the classpath itself.
        command: "kotlin-language-server",
        args: &[],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint:
            "travsr lang install kotlin  (security approval required — run interactively)",
        underlying_tool_hint: "travsr lang install kotlin  (auto-installs kotlin-language-server)",
        provider_binary: Some("travsr-lang-kotlin"),
        elevated_hosts: &[
            "repo1.maven.org",
            "repo.maven.apache.org",
            "plugins.gradle.org",
        ],
        // KLS ships as server.zip — extracted to ~/.travsr/kls/, wrapper created at
        // ~/.travsr/bin/kotlin-language-server automatically by `travsr lang install kotlin`.
        scip_install: ScipInstall::ZipBinary(ZipBinarySpec {
            repo: "fwcd/kotlin-language-server",
            asset_fn: kls_asset,
            extract_dir: "kls",
            binary_subpath: "server/bin/kotlin-language-server",
            install_name: "kotlin-language-server",
            version_fallback: "1.3.13",
            sha256_fn: Some(kls_sha256),
        }),
        extensions: &[".kt", ".kts"],
        wrapper_version_fallback: "v0.1.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "scala",
        npm_package: Some("@travsr-plugin/scala"),
        // The sidecar injects semanticdbEnabled := true and runs `sbt compile`.
        // scip-scala is not published to any accessible registry.
        command: "sbt",
        args: &[],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "travsr lang install scala  (security approval required — run interactively)",
        underlying_tool_hint: "https://www.scala-sbt.org/download.html — install sbt (usually already present in Scala projects)",
        provider_binary: Some("travsr-lang-scala"),
        elevated_hosts: &[
            "repo1.maven.org",
            "repo.maven.apache.org",
            "repo.scala-sbt.org",
            "plugins.sbt.org",
        ],
        scip_install: ScipInstall::Manual,
        extensions: &[".scala", ".sbt"],
        wrapper_version_fallback: "v0.1.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "ruby",
        npm_package: Some("@travsr-plugin/ruby"),
        command: "scip-ruby",
        // #712: scip-ruby has no `--index-file-path` option (the flag is
        // `--index-file`), and it requires a positional folder/file to index
        // ({root}). The wrong flag made this direct-scip fallback exit 1 and
        // write nothing whenever the travsr-lang-ruby wrapper was absent.
        args: &["--index-file", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "travsr lang install ruby  (experimental)",
        underlying_tool_hint: "https://github.com/sourcegraph/scip-ruby/releases — download scip-ruby-arm64-darwin or scip-ruby-x86_64-linux and place in ~/.travsr/bin/scip-ruby (chmod +x)",
        provider_binary: Some("travsr-lang-ruby"),
        elevated_hosts: &[],
        scip_install: ScipInstall::GithubBinary(ScipBinarySpec {
            repo: "sourcegraph/scip-ruby",
            asset_fn: scip_ruby_asset,
            install_name: "scip-ruby",
            version_fallback: "scip-ruby-v0.4.7",
            verify_sha256: false,
            sha256_fn: Some(scip_ruby_sha256),
        }),
        extensions: &[".rb"],
        wrapper_version_fallback: "v0.1.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "php",
        npm_package: Some("@travsr-plugin/php"),
        command: "scip-php",
        args: &["{root}", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "travsr lang install php",
        underlying_tool_hint: "https://github.com/davidrjenni/scip-php — community indexer: composer require --dev davidrjenni/scip-php (then use vendor/bin/scip-php)",
        provider_binary: Some("travsr-lang-php"),
        elevated_hosts: &[],
        scip_install: ScipInstall::Manual,
        extensions: &[".php"],
        wrapper_version_fallback: "v0.1.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "csharp",
        npm_package: Some("@travsr-plugin/csharp"),
        command: "scip-dotnet",
        args: &["{root}", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint:
            "travsr lang install csharp  (security approval required — run interactively)",
        underlying_tool_hint: "dotnet tool install --global scip-dotnet",
        provider_binary: Some("travsr-lang-csharp"),
        elevated_hosts: &["api.nuget.org", "www.nuget.org"],
        scip_install: ScipInstall::Command(&[
            "dotnet",
            "tool",
            "install",
            "--global",
            "scip-dotnet",
        ]),
        extensions: &[".cs", ".csx"],
        wrapper_version_fallback: "v0.1.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "cpp",
        npm_package: Some("@travsr-plugin/cpp"),
        command: "scip-clang",
        args: &[
            "--compdb-path",
            "{root}/compile_commands.json",
            "--output",
            "{output}",
        ],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::NativeIpc,
        install_hint: "travsr lang install cpp  (requires compile_commands.json)",
        underlying_tool_hint: "https://github.com/sourcegraph/scip-clang/releases — download scip-clang-arm64-darwin or scip-clang-x86_64-linux and place in ~/.travsr/bin/scip-clang (chmod +x)",
        provider_binary: Some("travsr-lang-cpp"),
        elevated_hosts: &[],
        scip_install: ScipInstall::GithubBinary(ScipBinarySpec {
            repo: "sourcegraph/scip-clang",
            asset_fn: scip_clang_asset,
            install_name: "scip-clang",
            version_fallback: "v0.4.0",
            verify_sha256: false,
            sha256_fn: Some(scip_clang_sha256),
        }),
        extensions: &[".cpp", ".cc", ".cxx", ".hpp"],
        wrapper_version_fallback: "v0.1.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "c",
        npm_package: Some("@travsr-plugin/c"),
        command: "scip-clang",
        args: &[
            "--compdb-path",
            "{root}/compile_commands.json",
            "--output",
            "{output}",
        ],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::NativeIpc,
        install_hint: "travsr lang install c  (requires compile_commands.json)",
        underlying_tool_hint: "https://github.com/sourcegraph/scip-clang/releases — download scip-clang-arm64-darwin or scip-clang-x86_64-linux and place in ~/.travsr/bin/scip-clang (chmod +x)",
        provider_binary: Some("travsr-lang-c"),
        elevated_hosts: &[],
        scip_install: ScipInstall::GithubBinary(ScipBinarySpec {
            repo: "sourcegraph/scip-clang",
            asset_fn: scip_clang_asset,
            install_name: "scip-clang",
            version_fallback: "v0.4.0",
            verify_sha256: false,
            sha256_fn: Some(scip_clang_sha256),
        }),
        extensions: &[".c", ".h"],
        wrapper_version_fallback: "v0.1.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "swift",
        npm_package: Some("@travsr-plugin/swift"),
        command: "travsr-swift-index-emitter",
        args: &[],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "travsr lang install swift",
        underlying_tool_hint: "",
        provider_binary: Some("travsr-lang-swift"),
        elevated_hosts: &[],
        scip_install: ScipInstall::GithubBinary(ScipBinarySpec {
            repo: "Travsr-com/travsr-lang",
            asset_fn: swift_index_emitter_asset,
            install_name: "travsr-swift-index-emitter",
            version_fallback: "v0.3.0",
            verify_sha256: true,
            sha256_fn: None,
        }),
        extensions: &[".swift"],
        wrapper_version_fallback: "v0.3.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "objectivec",
        npm_package: Some("@travsr-plugin/objectivec"),
        command: "travsr-lang-objectivec",
        args: &[],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "travsr lang install objectivec",
        underlying_tool_hint: "",
        provider_binary: Some("travsr-lang-objectivec"),
        elevated_hosts: &[],
        scip_install: ScipInstall::GithubBinary(ScipBinarySpec {
            repo: "Travsr-com/travsr-lang",
            asset_fn: objc_index_emitter_asset,
            install_name: "travsr-lang-objectivec",
            version_fallback: "v0.3.0",
            verify_sha256: true,
            sha256_fn: None,
        }),
        extensions: &[".m", ".mm"],
        wrapper_version_fallback: "v0.3.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "dart",
        npm_package: Some("@travsr-plugin/dart"),
        command: "travsr-dart-index-emitter",
        args: &[],
        output_format: OutputFormat::Scip,
        // travsr-dart-index-emitter is a Dart AOT binary linked against
        // Security.framework, CoreFoundation, Foundation, and CoreServices.
        // These macOS system frameworks need unrestricted system-file access
        // that can't be enumerated in a Seatbelt profile — the AOT runtime
        // reads its own binary to load the embedded snapshot and sandbox-exec
        // blocks that, producing "is not an AOT snapshot" errors. NativeIpc
        // bypasses sandbox-exec and applies ulimit caps only (same as scip-clang).
        sandbox: SandboxRequirement::NativeIpc,
        install_hint: "travsr lang install dart",
        underlying_tool_hint: "",
        provider_binary: Some("travsr-lang-dart"),
        elevated_hosts: &[],
        scip_install: ScipInstall::GithubBinary(ScipBinarySpec {
            repo: "Travsr-com/travsr-lang",
            asset_fn: dart_scip_emitter_asset,
            install_name: "travsr-dart-index-emitter",
            version_fallback: "v0.3.0",
            verify_sha256: true,
            sha256_fn: None,
        }),
        extensions: &[".dart"],
        wrapper_version_fallback: "v0.3.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
];

/// Look up a Phase B entry by canonical language string.
pub fn lookup(language: &str) -> Option<&'static PhaseBEntry> {
    CATALOG.iter().find(|e| e.language == language)
}

// ── RFC-025 SidecarSpec impls ───────────────────────────────────────────────
//
// The Phase B family joins the embed sidecar under the one shared version
// contract. There is no *known* behavioral floor for any scip-* tool or lang
// wrapper today (unlike embed's #376/#701), so every `min_version()` here is
// `Semver::ZERO` - the machinery is live and a real floor is one edit away, but
// no healthy install is ever refused. Per RFC-025 decision 3 a floor is only
// raised in the same commit that first relies on a tool behavior; until then
// declaring one would be dishonest.

use crate::sidecar_version::{Semver, SidecarSpec};

/// The GitHub repo the `travsr-lang-*` wrappers are released from.
const TRAVSR_LANG_REPO: &str = "Travsr-com/travsr-lang";

impl SidecarSpec for ScipBinarySpec {
    fn install_name(&self) -> &str {
        self.install_name
    }
    fn github_repo(&self) -> &str {
        self.repo
    }
    fn min_version(&self) -> Semver {
        Semver::ZERO
    }
    fn version_fallback(&self) -> &str {
        self.version_fallback
    }
    fn pinned(&self) -> bool {
        // A vendored hash is only meaningful against one exact asset, so an
        // entry that carries one is pinned to `version_fallback` (#410 M2).
        self.sha256_fn.is_some()
    }
}

impl SidecarSpec for ZipBinarySpec {
    fn install_name(&self) -> &str {
        self.install_name
    }
    fn github_repo(&self) -> &str {
        self.repo
    }
    fn min_version(&self) -> Semver {
        Semver::ZERO
    }
    fn version_fallback(&self) -> &str {
        self.version_fallback
    }
    fn pinned(&self) -> bool {
        self.sha256_fn.is_some()
    }
}

impl SidecarSpec for PhaseBEntry {
    /// The `travsr-lang-*` wrapper is the versioned sidecar for a language.
    /// Builtins (no wrapper) fall back to the display `command`.
    fn install_name(&self) -> &str {
        self.provider_binary.unwrap_or(self.command)
    }
    fn github_repo(&self) -> &str {
        TRAVSR_LANG_REPO
    }
    fn min_version(&self) -> Semver {
        Semver::ZERO
    }
    fn version_fallback(&self) -> &str {
        self.wrapper_version_fallback
    }
    fn pinned(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod vendored_hash_tests {
    use super::{kls_sha256, scip_clang_sha256, scip_ruby_sha256, ScipInstall, CATALOG};

    /// The `GithubBinary` spec of an entry, if it has one. Kept local to the
    /// tests rather than added to the production surface.
    fn binary_spec(e: &super::PhaseBEntry) -> Option<&super::ScipBinarySpec> {
        match &e.scip_install {
            ScipInstall::GithubBinary(spec) => Some(spec),
            _ => None,
        }
    }

    /// #410 M2: a vendored hash is only meaningful against one exact asset, so
    /// every entry carrying one must be pinned rather than resolving
    /// `releases/latest`. `install_scip_github_binary` keys that decision off
    /// `sha256_fn.is_some()`, so the invariant to hold here is that a hash is
    /// only ever returned for the tag the entry actually pins to.
    #[test]
    fn a_vendored_hash_exists_only_for_the_pinned_tag() {
        for e in CATALOG {
            let Some(scip) = binary_spec(e) else { continue };
            let Some(f) = scip.sha256_fn else { continue };
            let pinned = scip.version_fallback;
            for target in [
                "aarch64-apple-darwin",
                "x86_64-apple-darwin",
                "x86_64-unknown-linux-gnu",
            ] {
                if let Some(h) = f(pinned, target) {
                    assert_eq!(h.len(), 64, "{}: hash must be 64 hex chars", e.language);
                    assert!(
                        h.chars().all(|c| c.is_ascii_hexdigit()),
                        "{}: non-hex in vendored hash",
                        e.language
                    );
                }
                assert!(
                    f("some-other-tag", target).is_none(),
                    "{}: a hash was returned for a tag the entry is not pinned to — \
                     it would be checked against the wrong asset",
                    e.language
                );
            }
        }
    }

    #[test]
    fn the_vendored_hashes_cover_the_platforms_their_assets_ship_for() {
        // scip-ruby and scip-clang ship darwin-arm64 and linux-x86_64 only;
        // a missing hash there would fail the install rather than skip the check.
        assert!(scip_ruby_sha256("scip-ruby-v0.4.7", "aarch64-apple-darwin").is_some());
        assert!(scip_ruby_sha256("scip-ruby-v0.4.7", "x86_64-unknown-linux-gnu").is_some());
        assert!(scip_clang_sha256("v0.4.0", "aarch64-apple-darwin").is_some());
        assert!(scip_clang_sha256("v0.4.0", "x86_64-unknown-linux-gnu").is_some());
        assert!(kls_sha256("1.3.13").is_some());
    }

    /// scip-java publishes a `.sha256` sidecar, so it verifies through that and
    /// must NOT carry a vendored hash — one would pin it for no benefit and
    /// make every upstream release a catalog bump.
    #[test]
    fn entries_with_a_sidecar_carry_no_vendored_hash() {
        for e in CATALOG {
            let Some(scip) = binary_spec(e) else { continue };
            if scip.verify_sha256 {
                assert!(
                    scip.sha256_fn.is_none(),
                    "{}: verifies via sidecar, so it must not also be pinned to a vendored hash",
                    e.language
                );
            }
        }
    }
}
