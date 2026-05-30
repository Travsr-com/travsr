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
    },
    PhaseBEntry {
        language: "javascript",
        npm_package: Some("@travsr/typescript"),
        command: "travsr-lsif-ts",
        args: &["--project", "{tsconfig}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/typescript  (installs travsr-lsif-ts)",
    },
    PhaseBEntry {
        language: "rust",
        npm_package: Some("@travsr/rust"),
        command: "rust-analyzer",
        args: &["lsif", "{root}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/rust  (or: rustup component add rust-analyzer)",
    },
    PhaseBEntry {
        language: "go",
        npm_package: Some("@travsr/go"),
        command: "scip-go",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/go  (or: go install github.com/sourcegraph/scip-go/cmd/scip-go@latest)",
    },
    PhaseBEntry {
        language: "python",
        npm_package: Some("@travsr/python"),
        command: "scip-python",
        args: &["index", "--project-name", "project", "--project-version", "0.0.1", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/python  (or: pip install scip-python)",
    },
    PhaseBEntry {
        language: "java",
        npm_package: Some("@travsr/java"),
        command: "scip-java",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr/java  — PSE approval required (Maven/Gradle network access)",
    },
    PhaseBEntry {
        language: "kotlin",
        npm_package: Some("@travsr/kotlin"),
        command: "scip-java",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr/kotlin  — PSE approval required (scip-java covers Kotlin via Gradle)",
    },
    PhaseBEntry {
        language: "scala",
        npm_package: Some("@travsr/scala"),
        command: "scip-scala",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr/scala  — PSE approval required (sbt dependency resolution)",
    },
    PhaseBEntry {
        language: "ruby",
        npm_package: Some("@travsr/ruby"),
        command: "scip-ruby",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/ruby  (experimental)",
    },
    PhaseBEntry {
        language: "php",
        npm_package: Some("@travsr/php"),
        command: "scip-php",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/php",
    },
    PhaseBEntry {
        language: "csharp",
        npm_package: Some("@travsr/csharp"),
        command: "scip-dotnet",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "npm install -g @travsr/csharp  — PSE approval required (NuGet restore)",
    },
    PhaseBEntry {
        language: "cpp",
        npm_package: Some("@travsr/cpp"),
        command: "scip-clang",
        args: &["--compdb-path", "{root}/compile_commands.json"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/cpp  (requires compile_commands.json)",
    },
    PhaseBEntry {
        language: "c",
        npm_package: Some("@travsr/c"),
        command: "scip-clang",
        args: &["--compdb-path", "{root}/compile_commands.json"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g @travsr/c  (requires compile_commands.json)",
    },
];

/// Look up a Phase B entry by canonical language string.
pub fn lookup(language: &str) -> Option<&'static PhaseBEntry> {
    CATALOG.iter().find(|e| e.language == language)
}
