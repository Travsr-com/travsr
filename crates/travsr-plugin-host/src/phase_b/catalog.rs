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
    /// npm package distributed via the travsr-lang repo (e.g. `@travsr/rust`).
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
        npm_package: Some("@travsr/typescript"),
        command: "travsr-lsif-ts",
        args: &["--project", "{tsconfig}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/typescript  (installs travsr-lsif-ts)",
        provider_binary: None,
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "javascript",
        npm_package: Some("@travsr/typescript"),
        command: "travsr-lsif-ts",
        args: &["--project", "{tsconfig}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/typescript  (installs travsr-lsif-ts)",
        provider_binary: None,
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "rust",
        npm_package: Some("@travsr/rust"),
        command: "rust-analyzer",
        args: &["lsif", "{root}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/rust  (or: rustup component add rust-analyzer)",
        provider_binary: None,
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "go",
        npm_package: Some("@travsr/go"),
        command: "scip-go",
        args: &["--output", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/go  (or: go install github.com/sourcegraph/scip-go/cmd/scip-go@latest)",
        provider_binary: Some("travsr-lang-go"),
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "python",
        npm_package: Some("@travsr/python"),
        command: "scip-python",
        args: &["index", "--project-name", "project", "--project-version", "0.0.1", "--output", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/python  (or: pip install scip-python)",
        provider_binary: Some("travsr-lang-python"),
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "java",
        npm_package: Some("@travsr/java"),
        command: "scip-java",
        args: &["index", "--output", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr/java  — PSE approval required (Maven/Gradle network access)",
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
        npm_package: Some("@travsr/kotlin"),
        command: "scip-java",
        args: &["index", "--output", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr/kotlin  — PSE approval required (scip-java covers Kotlin via Gradle)",
        provider_binary: Some("travsr-lang-kotlin"),
        elevated_hosts: &[
            "repo1.maven.org",
            "repo.maven.apache.org",
            "plugins.gradle.org",
        ],
    },
    PhaseBEntry {
        language: "scala",
        npm_package: Some("@travsr/scala"),
        command: "scip-scala",
        args: &["{root}", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr/scala  — PSE approval required (sbt dependency resolution)",
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
        npm_package: Some("@travsr/ruby"),
        command: "scip-ruby",
        args: &["--index-file-path", "{output}", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/ruby  (experimental)",
        provider_binary: Some("travsr-lang-ruby"),
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "php",
        npm_package: Some("@travsr/php"),
        command: "scip-php",
        args: &["{root}", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/php",
        provider_binary: Some("travsr-lang-php"),
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "csharp",
        npm_package: Some("@travsr/csharp"),
        command: "scip-dotnet",
        args: &["{root}", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr/csharp  — PSE approval required (NuGet restore)",
        provider_binary: Some("travsr-lang-csharp"),
        elevated_hosts: &["api.nuget.org", "www.nuget.org"],
    },
    PhaseBEntry {
        language: "cpp",
        npm_package: Some("@travsr/cpp"),
        command: "scip-clang",
        args: &["--compdb-path", "{root}/compile_commands.json", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/cpp  (requires compile_commands.json)",
        provider_binary: Some("travsr-lang-cpp"),
        elevated_hosts: &[],
    },
    PhaseBEntry {
        language: "c",
        npm_package: Some("@travsr/c"),
        command: "scip-clang",
        args: &["--compdb-path", "{root}/compile_commands.json", "--output", "{output}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/c  (requires compile_commands.json)",
        provider_binary: Some("travsr-lang-c"),
        elevated_hosts: &[],
    },
];

/// Look up a Phase B entry by canonical language string.
pub fn lookup(language: &str) -> Option<&'static PhaseBEntry> {
    CATALOG.iter().find(|e| e.language == language)
}
