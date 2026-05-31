//! Static catalog of every known Phase B tool.
//! Adding a new language requires only a new entry — no code changes elsewhere.
//! None are active by default; users enable via `travsr lang add <lang>`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Lsif,
    Scip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxRequirement {
    /// No network, no build steps that download dependencies.
    Standard,
    /// Needs network for dependency resolution (Maven/Gradle/NuGet/sbt).
    /// Requires SandboxPolicy::Elevated with PSE sign-off (ADR-017 Rule 1).
    RequiresElevated,
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
    /// Shown by `travsr lang list` and `travsr lang add` when tool is absent.
    pub install_hint: &'static str,
    /// How to install the underlying tool (scip-*, rust-analyzer, etc.) when the
    /// travsr-lang-* wrapper is already installed but the tool itself is missing.
    /// Shown in the "wrapper-only" state by `travsr lang list`.
    /// Empty string for builtins where no separate tool install is needed
    /// (e.g. typescript/javascript install travsr-lsif-ts via their npm package).
    pub underlying_tool_hint: &'static str,
    /// The travsr-lang binary name for this language, e.g. "travsr-lang-go".
    /// None for in-tree builtins (rust, typescript) that spawn via __plugin.
    pub provider_binary: Option<&'static str>,
    /// For RequiresElevated languages: the network hosts their build tool contacts.
    /// Empty for Standard sandbox languages.
    pub elevated_hosts: &'static [&'static str],
}

pub static CATALOG: &[PhaseBEntry] = &[
    PhaseBEntry {
        language: "typescript",
        npm_package: Some("@travsr-plugin/typescript"),
        command: "travsr-lsif-ts",
        args: &["--project", "{tsconfig}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr-plugin/typescript  (installs travsr-lsif-ts)",
        underlying_tool_hint: "",
        provider_binary: None,
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "javascript",
        npm_package: Some("@travsr-plugin/typescript"),
        command: "travsr-lsif-ts",
        args: &["--project", "{tsconfig}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr-plugin/typescript  (installs travsr-lsif-ts)",
        underlying_tool_hint: "",
        provider_binary: None,
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "rust",
        npm_package: Some("@travsr-plugin/rust"),
        command: "rust-analyzer",
        args: &["lsif", "{root}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "rustup component add rust-analyzer",
        underlying_tool_hint: "rustup component add rust-analyzer",
        provider_binary: None,
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "go",
        npm_package: Some("@travsr-plugin/go"),
        command: "scip-go",
        args: &["--output", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr-plugin/go  (or: go install github.com/sourcegraph/scip-go/cmd/scip-go@latest)",
        underlying_tool_hint: "go install github.com/sourcegraph/scip-go/cmd/scip-go@latest",
        provider_binary: Some("travsr-lang-go"),
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "python",
        npm_package: Some("@travsr-plugin/python"),
        command: "scip-python",
        args: &["index", "--project-name", "project", "--project-version", "0.0.1", "--output", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr-plugin/python  (or: pip install scip-python)",
        underlying_tool_hint: "pip install scip-python",
        provider_binary: Some("travsr-lang-python"),
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "java",
        npm_package: Some("@travsr-plugin/java"),
        command: "scip-java",
        args: &["index", "--output", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr-plugin/java  — PSE approval required (Maven/Gradle network access)",
        underlying_tool_hint: "see scip-java docs — install via Maven or Gradle plugin",
        provider_binary: Some("travsr-lang-java"),
        elevated_hosts: &[
            "repo1.maven.org",
            "repo.maven.apache.org",
            "plugins.gradle.org",
            "jcenter.bintray.com",
        ],
    },
    PhaseBEntry {
        language: "kotlin",
        npm_package: Some("@travsr-plugin/kotlin"),
        command: "scip-java",
        args: &["index", "--output", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr-plugin/kotlin  — PSE approval required (scip-java covers Kotlin via Gradle)",
        underlying_tool_hint: "see scip-java docs — covers Kotlin via Gradle plugin",
        provider_binary: Some("travsr-lang-kotlin"),
        elevated_hosts: &[
            "repo1.maven.org",
            "repo.maven.apache.org",
            "plugins.gradle.org",
        ],
    },
    PhaseBEntry {
        language: "scala",
        npm_package: Some("@travsr-plugin/scala"),
        command: "scip-scala",
        args: &["{root}", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr-plugin/scala  — PSE approval required (sbt dependency resolution)",
        underlying_tool_hint: "see scip-scala docs — install via sbt plugin",
        provider_binary: Some("travsr-lang-scala"),
        elevated_hosts: &[
            "repo1.maven.org",
            "repo.maven.apache.org",
            "plugins.sbt.org",
            "jcenter.bintray.com",
        ],
    },
    PhaseBEntry {
        language: "ruby",
        npm_package: Some("@travsr-plugin/ruby"),
        command: "scip-ruby",
        args: &["--index-file-path", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr-plugin/ruby  (experimental)",
        underlying_tool_hint: "see scip-ruby docs",
        provider_binary: Some("travsr-lang-ruby"),
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "php",
        npm_package: Some("@travsr-plugin/php"),
        command: "scip-php",
        args: &["{root}", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr-plugin/php",
        underlying_tool_hint: "see scip-php docs",
        provider_binary: Some("travsr-lang-php"),
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "csharp",
        npm_package: Some("@travsr-plugin/csharp"),
        command: "scip-dotnet",
        args: &["{root}", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr-plugin/csharp  — PSE approval required (NuGet restore)",
        underlying_tool_hint: "dotnet tool install --global scip-dotnet",
        provider_binary: Some("travsr-lang-csharp"),
        elevated_hosts: &["api.nuget.org", "www.nuget.org"],
    },
    PhaseBEntry {
        language: "cpp",
        npm_package: Some("@travsr-plugin/cpp"),
        command: "scip-clang",
        args: &["--compdb-path", "{root}/compile_commands.json", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr-plugin/cpp  (requires compile_commands.json)",
        underlying_tool_hint: "see scip-clang releases (requires compile_commands.json)",
        provider_binary: Some("travsr-lang-cpp"),
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "c",
        npm_package: Some("@travsr-plugin/c"),
        command: "scip-clang",
        args: &["--compdb-path", "{root}/compile_commands.json", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr-plugin/c  (requires compile_commands.json)",
        underlying_tool_hint: "see scip-clang releases (requires compile_commands.json)",
        provider_binary: Some("travsr-lang-c"),
        elevated_hosts: &[],
    },
];

/// Look up a Phase B entry by canonical language string.
pub fn lookup(language: &str) -> Option<&'static PhaseBEntry> {
    CATALOG.iter().find(|e| e.language == language)
}
