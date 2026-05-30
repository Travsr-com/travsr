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
        command: "travsr-lsif-ts",
        args: &["--project", "{tsconfig}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g travsr-lsif-ts",
    },
    PhaseBEntry {
        language: "javascript",
        command: "travsr-lsif-ts",
        args: &["--project", "{tsconfig}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "npm install -g travsr-lsif-ts",
    },
    PhaseBEntry {
        language: "rust",
        command: "rust-analyzer",
        args: &["lsif", "{root}"],
        output_format: OutputFormat::Lsif,
        sandbox: SandboxRequirement::Standard,
        install_hint: "rustup component add rust-analyzer",
    },
    PhaseBEntry {
        language: "go",
        command: "scip-go",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "go install github.com/sourcegraph/scip-go/cmd/scip-go@latest",
    },
    PhaseBEntry {
        language: "python",
        command: "scip-python",
        args: &["index", "--project-name", "project", "--project-version", "0.0.1", "{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "pip install scip-python",
    },
    PhaseBEntry {
        language: "java",
        command: "scip-java",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "Download scip-java from https://github.com/sourcegraph/scip-java — PSE approval required for Elevated sandbox (Maven/Gradle network access)",
    },
    PhaseBEntry {
        language: "kotlin",
        command: "scip-java",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "scip-java covers Kotlin via Gradle — PSE approval required for Elevated sandbox",
    },
    PhaseBEntry {
        language: "scala",
        command: "scip-scala",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "Add scip-scala sbt plugin — PSE approval required for Elevated sandbox",
    },
    PhaseBEntry {
        language: "ruby",
        command: "scip-ruby",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "gem install scip-ruby (experimental)",
    },
    PhaseBEntry {
        language: "php",
        command: "scip-php",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "Download scip-php from https://github.com/sourcegraph/scip-php",
    },
    PhaseBEntry {
        language: "csharp",
        command: "scip-dotnet",
        args: &["{root}"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::RequiresElevated,
        install_hint: "dotnet tool install -g Sourcegraph.Scip.Dotnet — PSE approval required for Elevated sandbox (NuGet restore)",
    },
    PhaseBEntry {
        language: "cpp",
        command: "scip-clang",
        args: &["--compdb-path", "{root}/compile_commands.json"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "Download scip-clang from https://github.com/sourcegraph/scip-clang — requires compile_commands.json",
    },
    PhaseBEntry {
        language: "c",
        command: "scip-clang",
        args: &["--compdb-path", "{root}/compile_commands.json"],
        output_format: OutputFormat::Scip,
        sandbox: SandboxRequirement::Standard,
        install_hint: "Download scip-clang — requires compile_commands.json",
    },
];

/// Look up a Phase B entry by canonical language string.
pub fn lookup(language: &str) -> Option<&'static PhaseBEntry> {
    CATALOG.iter().find(|e| e.language == language)
}
