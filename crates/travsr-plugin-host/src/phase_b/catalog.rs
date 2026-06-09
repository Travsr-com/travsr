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
        command: "scip-python",
        args: &[
            "index",
            "--project-name",
            "{corpus}",
            "--project-version",
            "0.0.1",
            "--output",
            "{output}",
            "{root}",
        ],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @sourcegraph/scip-python",
        underlying_tool_hint: "npm install -g @sourcegraph/scip-python",
        provider_binary: None,
        elevated_hosts: &[],
        scip_install: ScipInstall::Command(&["npm", "install", "-g", "@sourcegraph/scip-python"]),
        extensions: &[".py"],
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
        args: &["--index-file-path", "{output}", "{root}"],
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
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "travsr lang install swift",
        underlying_tool_hint: "cd travsr-lang/packages/swift-index-emitter && swift build -c release  (then copy .build/release/swift-index-emitter to ~/.travsr/bin/travsr-swift-index-emitter)",
        provider_binary: Some("travsr-lang-swift"),
        elevated_hosts: &["localhost"],
        scip_install: ScipInstall::Manual,
        extensions: &[".swift"],
        wrapper_version_fallback: "v0.1.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: false,
    },
    PhaseBEntry {
        language: "dart",
        npm_package: Some("@travsr-plugin/dart"),
        command: "dart",
        args: &[],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "travsr lang install dart",
        underlying_tool_hint: "https://dart.dev/get-dart — install Dart SDK, then: cd travsr-lang/packages/dart-scip-emitter && dart pub get",
        provider_binary: Some("travsr-lang-dart"),
        elevated_hosts: &["localhost"],
        scip_install: ScipInstall::Manual,
        extensions: &[".dart"],
        wrapper_version_fallback: "v0.1.0",
        builtin: false,
        native_phase_b: false,
        has_share_assets: true,
    },
];

/// Look up a Phase B entry by canonical language string.
pub fn lookup(language: &str) -> Option<&'static PhaseBEntry> {
    CATALOG.iter().find(|e| e.language == language)
}
