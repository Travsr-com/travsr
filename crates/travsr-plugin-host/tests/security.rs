//! ADR-017 security tests — merge-gating for P5-S1 onward.
//! These tests verify the sandbox policy enforcements defined in ADR-017.
//! Tests that require bwrap (Linux) are skipped on other platforms.

use travsr_plugin_host::phase_b::catalog::{lookup, SandboxRequirement};
use travsr_plugin_host::sandbox::policy::SandboxPolicy;
#[cfg(target_os = "linux")]
use travsr_plugin_host::sandbox::policy::SandboxUnavailable;

// 1. Egress allowed — Standard sandbox permits network (build tools need it)
//
// ADR-017 intentionally allows network for Standard/NativeIpc policies so that
// build tools (go mod, npm, pip, …) can fetch dependencies. FS confinement via
// bwrap still applies; only network namespace isolation is skipped.
#[test]
#[cfg(target_os = "linux")]
fn sandbox_standard_allows_network() {
    use travsr_plugin_host::sandbox::linux::build_sandboxed_command;

    let repo = tempfile::tempdir().expect("tempdir");
    let scratch = tempfile::tempdir().expect("scratch");

    let cmd = build_sandboxed_command(
        "sh",
        &[
            "-c",
            "curl -s --max-time 2 http://1.1.1.1/ > /dev/null 2>&1; echo $?",
        ],
        repo.path(),
        scratch.path(),
        &SandboxPolicy::Standard,
        "",
    );

    match cmd {
        Err(SandboxUnavailable(ref msg)) => {
            if std::env::var("CI").is_ok() {
                panic!("sandbox unavailable in CI: {msg}");
            }
            eprintln!("SKIP: {msg}");
        }
        Ok(spawner) => {
            let output = spawner.output().expect("spawn");
            let exit_code = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // The shell command is `sh -c "curl ...; echo $?"` — sh always exits 0
            // when bwrap runs successfully, so output.status.success() is always true
            // for a live sandbox. We inspect exit_code directly:
            //   "0"   → curl ran and network succeeded (expected: network is allowed)
            //   "127" → curl not found inside bwrap (acceptable: no curl in namespace)
            //   "126" → curl not executable (acceptable)
            //   other → curl found but network blocked → test FAILS
            let bwrap_failed = !output.status.success();
            let curl_succeeded = exit_code == "0";
            let curl_absent = exit_code == "127" || exit_code == "126";
            assert!(
                bwrap_failed || curl_absent || curl_succeeded,
                "Standard sandbox blocked network but ADR-017 Amendment A1 intentionally \
                 allows it for build tools; exit_code={exit_code}"
            );
        }
    }
}

// 2. Fail-closed — no sandbox → Err(SandboxUnavailable), never an unsandboxed command
#[test]
fn sandbox_unavailable_returns_err_not_fallback_command() {
    // On any platform, if sandbox is unavailable we get Err, not a fallback Command.
    // We verify this by calling the wrong-platform builder and checking the type.
    //
    // The sandbox builders return Result<Command, SandboxUnavailable>.
    // SandboxUnavailable must propagate upward — callers must not run the subprocess.
    // This test verifies the type contract is correct (no silent fallback).

    #[cfg(target_os = "linux")]
    {
        use travsr_plugin_host::sandbox::macos::build_sandboxed_command;
        let repo = tempfile::tempdir().expect("tempdir");
        let scratch = tempfile::tempdir().expect("scratch");
        let result = build_sandboxed_command(
            "sh",
            &["-c", "true"],
            repo.path(),
            scratch.path(),
            &SandboxPolicy::Standard,
        );
        // On Linux, the macOS builder always returns Err
        assert!(
            result.is_err(),
            "macOS sandbox builder should return Err on Linux"
        );
    }

    #[cfg(target_os = "macos")]
    {
        use travsr_plugin_host::sandbox::linux::build_sandboxed_command;
        let repo = tempfile::tempdir().expect("tempdir");
        let scratch = tempfile::tempdir().expect("scratch");
        let result = build_sandboxed_command(
            "sh",
            &["-c", "true"],
            repo.path(),
            scratch.path(),
            &SandboxPolicy::Standard,
            "",
        );
        assert!(
            result.is_err(),
            "Linux sandbox builder should return Err on macOS"
        );
    }

    // On Windows or other platforms, both builders return Err — validate both.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        use travsr_plugin_host::sandbox::linux::build_sandboxed_command as linux_build;
        use travsr_plugin_host::sandbox::macos::build_sandboxed_command as macos_build;
        let repo = tempfile::tempdir().expect("tempdir");
        let scratch = tempfile::tempdir().expect("scratch");
        assert!(linux_build(
            "sh",
            &[],
            repo.path(),
            scratch.path(),
            &SandboxPolicy::Standard,
            "",
        )
        .is_err());
        assert!(macos_build(
            "sh",
            &[],
            repo.path(),
            scratch.path(),
            &SandboxPolicy::Standard
        )
        .is_err());
    }
}

// 3. Trust gate — ADR-017 Rule 3, driven through the REAL spawn path (#414).
//
// These tests call `invoke_phase_b_all` (the production Phase B entry point)
// against a lang.toml that registers an external language whose provider
// binary is present on PATH, so the ONLY thing separating "spawned" from
// "skipped" is the per-corpus trust grant. The earlier versions of these
// tests only unit-tested `TrustConfig::is_trusted`, which production never
// called — they passed while the gate was dead code.

/// Serialises the trust-gate tests: they mutate process-global env
/// (TRAVSR_LANG_TOML, PATH) which must not interleave.
static TRUST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Drive `invoke_phase_b_all` for a registered external language ("go", fake
/// `travsr-lang-go` on PATH) under the given `trusted_corpora` list, and
/// return the outcome. Restores env before returning.
fn run_phase_b_with_trust(
    corpus: &str,
    trusted_corpora: &[&str],
) -> travsr_plugin_host::PhaseBOutcome {
    use std::io::Write as _;

    let _guard = TRUST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // lang.toml: "go" registered; trust list as given.
    let cfg_dir = tempfile::tempdir().expect("cfg tempdir");
    let lang_toml = cfg_dir.path().join("lang.toml");
    let trusted = trusted_corpora
        .iter()
        .map(|c| format!("{c:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        &lang_toml,
        format!("registered = [\"go\"]\ntrusted_corpora = [{trusted}]\n"),
    )
    .expect("write lang.toml");

    // Fake provider binary so the CatalogResolver surfaces "go" — without one
    // the language never reaches the spawn loop and the gate would be
    // untestable. The binary is a stub: if it is ever executed the handshake
    // fails and the language lands in `crashed`, which is itself proof the
    // spawn path was reached.
    let bin_dir = tempfile::tempdir().expect("bin tempdir");
    #[cfg(windows)]
    let fake = bin_dir.path().join("travsr-lang-go.bat");
    #[cfg(not(windows))]
    let fake = bin_dir.path().join("travsr-lang-go");
    {
        let mut f = std::fs::File::create(&fake).expect("create fake provider");
        #[cfg(windows)]
        f.write_all(b"@echo off\r\nexit /b 0\r\n").expect("write");
        #[cfg(not(windows))]
        f.write_all(b"#!/bin/sh\nexit 0\n").expect("write");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake provider");
    }

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<std::path::PathBuf> = vec![bin_dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(&old_path));
    let new_path = std::env::join_paths(paths).expect("join PATH");
    let old_lang_toml = std::env::var_os("TRAVSR_LANG_TOML");
    std::env::set_var("PATH", &new_path);
    std::env::set_var("TRAVSR_LANG_TOML", &lang_toml);

    let repo = tempfile::tempdir().expect("repo tempdir");
    std::fs::write(repo.path().join("main.go"), "package main\n").expect("write go file");

    let indexer = travsr_plugin_host::PluginIndexer::new(corpus);
    let inputs = travsr_plugin_host::PhaseBInputs {
        repo_root: repo.path(),
        present_languages: ["go".to_string()].into_iter().collect(),
        indexable_paths: &[],
    };
    let (.., outcome) = indexer.invoke_phase_b_all(&inputs);

    // Restore process env before any assertion can panic in the caller.
    std::env::set_var("PATH", old_path);
    match old_lang_toml {
        Some(v) => std::env::set_var("TRAVSR_LANG_TOML", v),
        None => std::env::remove_var("TRAVSR_LANG_TOML"),
    }

    outcome
}

#[test]
fn trust_gate_blocks_untrusted_corpus() {
    // A trust grant exists — but for a DIFFERENT corpus. The gate must be
    // per-corpus, not "any grant anywhere unlocks Phase B".
    let outcome = run_phase_b_with_trust(
        "github.com/acme/untrusted-repo",
        &["github.com/acme/some-other-repo"],
    );
    assert_eq!(
        outcome.skipped_untrusted_corpus,
        vec!["go".to_string()],
        "registered external language must be skipped before spawn when the \
         corpus has no trust grant"
    );
    assert!(
        outcome.ran.is_empty(),
        "nothing may run for an untrusted corpus: {:?}",
        outcome.ran
    );
    assert!(
        outcome.crashed.is_empty(),
        "no spawn may be attempted for an untrusted corpus (a crash entry \
         means the sidecar was spawned): {:?}",
        outcome.crashed
    );
}

#[test]
fn trust_gate_allows_explicitly_trusted_corpus() {
    let outcome = run_phase_b_with_trust("github.com/acme/my-repo", &["github.com/acme/my-repo"]);
    assert!(
        outcome.skipped_untrusted_corpus.is_empty(),
        "trusted corpus must pass the gate: {:?}",
        outcome.skipped_untrusted_corpus
    );
    // Past the gate, "go" must reach the spawn stage. The stub provider is not
    // a real plugin, so the language lands in `ran`, `crashed`, or
    // `version_mismatch` depending on how far the handshake gets — any of
    // these proves the spawn path executed, which the old assertion-only test
    // never did.
    let reached_spawn = outcome.ran.iter().any(|l| l == "go")
        || outcome.crashed.iter().any(|l| l == "go")
        || outcome.version_mismatch.iter().any(|(l, ..)| l == "go");
    assert!(
        reached_spawn,
        "trusted corpus must reach the spawn stage; outcome: ran={:?} crashed={:?} \
         version_mismatch={:?} skipped_no_analyzer={:?}",
        outcome.ran, outcome.crashed, outcome.version_mismatch, outcome.skipped_no_analyzer
    );
}

// 4. FS confinement — repo root is read-only in sandbox
#[test]
#[cfg(target_os = "linux")]
fn sandbox_repo_root_is_read_only() {
    use travsr_plugin_host::sandbox::linux::build_sandboxed_command;

    let repo = tempfile::tempdir().expect("tempdir");
    let scratch = tempfile::tempdir().expect("scratch");
    let breach_path = repo.path().join("sandbox_write_test.txt");

    let cmd = build_sandboxed_command(
        "sh",
        &[
            "-c",
            &format!("echo test > {} 2>&1; echo $?", breach_path.display()),
        ],
        repo.path(),
        scratch.path(),
        &SandboxPolicy::Standard,
        "",
    );

    match cmd {
        Err(SandboxUnavailable(ref msg)) => {
            if std::env::var("CI").is_ok() {
                panic!("sandbox unavailable in CI: {msg}");
            }
            eprintln!("SKIP: {msg}");
        }
        Ok(spawner) => {
            let _ = spawner.output();
            assert!(
                !breach_path.exists(),
                "sandbox allowed write to repo root — FS confinement broken"
            );
        }
    }
}

// 5. Scratch dir is writable
#[test]
#[cfg(target_os = "linux")]
fn sandbox_scratch_dir_is_writable() {
    use travsr_plugin_host::sandbox::linux::build_sandboxed_command;

    let repo = tempfile::tempdir().expect("tempdir");
    let scratch = tempfile::tempdir().expect("scratch");
    // Inside the sandbox the scratch area is a tmpfs at /travsr-scratch.
    // Verify the sandbox process can write there (status must be 0).
    let cmd = build_sandboxed_command(
        "sh",
        &["-c", "echo ok > /travsr-scratch/write_test.txt"],
        repo.path(),
        scratch.path(),
        &SandboxPolicy::Standard,
        "",
    );

    match cmd {
        Err(_) => {
            eprintln!("SKIP: bwrap not available");
        }
        Ok(spawner) => {
            let status = spawner.status().expect("spawn");
            assert!(
                status.success(),
                "sandbox blocked write to /travsr-scratch — should be writable"
            );
        }
    }
}

// 6. Cache integrity — daemon-computed key; version bump causes miss
#[test]
fn cache_key_is_daemon_computed() {
    use travsr_plugin_host::cache::{CacheKey, ParseCache};
    use travsr_plugin_protocol::ParseResponse;

    let mut cache = ParseCache::new();
    let hash = [1u8; 32];
    let version = "0.5.1";

    let key = CacheKey {
        plugin_version: version.to_string(),
        file_hash: hash,
    };
    cache.insert(key, ParseResponse::default());

    // Same key hits
    assert!(cache.get(version, hash).is_some(), "cache miss on same key");
    // Different version misses
    assert!(
        cache.get("0.5.2", hash).is_none(),
        "version bump should cause cache miss"
    );
    // Different hash misses
    let other_hash = [2u8; 32];
    assert!(
        cache.get(version, other_hash).is_none(),
        "different hash should miss"
    );
}

// 7. RequiresElevated catalog entries are correctly classified
#[test]
fn elevated_languages_require_pse_approval() {
    for lang in &["java", "kotlin", "csharp", "scala"] {
        let entry = lookup(lang).unwrap_or_else(|| panic!("{lang} not in catalog"));
        assert_eq!(
            entry.sandbox,
            SandboxRequirement::RequiresElevated,
            "{lang} should be RequiresElevated"
        );
    }
}

#[test]
fn standard_languages_do_not_require_approval() {
    for lang in &["rust", "go", "python", "php", "ruby", "typescript"] {
        let entry = lookup(lang).unwrap_or_else(|| panic!("{lang} not in catalog"));
        assert_eq!(
            entry.sandbox,
            SandboxRequirement::Standard,
            "{lang} should be Standard sandbox"
        );
    }
}

// 8. SandboxPolicy::Elevated validation rejects empty fields
#[test]
fn elevated_policy_rejects_empty_reason() {
    let policy = SandboxPolicy::Elevated {
        permitted_hosts: vec!["example.com".to_string()],
        reason: String::new(), // empty — should fail
        approved_by: "pse-handle".to_string(),
        approved_date: "2026-05-31".to_string(),
    };
    assert!(
        policy.validate().is_err(),
        "empty reason should fail validation"
    );
}

#[test]
fn elevated_policy_rejects_empty_approved_by() {
    let policy = SandboxPolicy::Elevated {
        permitted_hosts: vec!["example.com".to_string()],
        reason: "test reason".to_string(),
        approved_by: String::new(), // empty
        approved_date: "2026-05-31".to_string(),
    };
    assert!(
        policy.validate().is_err(),
        "empty approved_by should fail validation"
    );
}

#[test]
fn elevated_policy_accepts_valid_fields() {
    let policy = SandboxPolicy::Elevated {
        permitted_hosts: vec!["repo1.maven.org".to_string()],
        reason: "Maven dependency resolution".to_string(),
        approved_by: "pse-handle".to_string(),
        approved_date: "2026-05-31".to_string(),
    };
    assert!(
        policy.validate().is_ok(),
        "valid Elevated policy should pass validation"
    );
}

#[test]
fn elevated_policy_rejects_empty_approved_date() {
    let policy = SandboxPolicy::Elevated {
        permitted_hosts: vec!["repo1.maven.org".to_string()],
        reason: "Maven dependency resolution".to_string(),
        approved_by: "pse-handle".to_string(),
        approved_date: String::new(), // empty — re-review date is mandatory
    };
    assert!(
        policy.validate().is_err(),
        "empty approved_date should fail validation"
    );
}

#[test]
fn elevated_policy_rejects_empty_permitted_hosts() {
    let policy = SandboxPolicy::Elevated {
        permitted_hosts: vec![], // no allowlist — cannot mean "all hosts"
        reason: "Maven dependency resolution".to_string(),
        approved_by: "pse-handle".to_string(),
        approved_date: "2026-05-31".to_string(),
    };
    assert!(
        policy.validate().is_err(),
        "empty permitted_hosts should fail validation"
    );
}

// ADR-017 Rule 1: explicit allowlist only — no wildcards, no CIDR.
#[test]
fn elevated_policy_rejects_wildcard_and_cidr_hosts() {
    for bad in &[
        "*.maven.org",                   // wildcard
        "*",                             // full wildcard
        "10.0.0.0/8",                    // CIDR range
        "https://repo.maven.apache.org", // scheme
        "repo.maven.apache.org:443",     // port
        "repo .maven.org",               // whitespace
    ] {
        let policy = SandboxPolicy::Elevated {
            permitted_hosts: vec![bad.to_string()],
            reason: "test".to_string(),
            approved_by: "pse-handle".to_string(),
            approved_date: "2026-05-31".to_string(),
        };
        assert!(
            policy.validate().is_err(),
            "permitted host '{bad}' should be rejected (no wildcards/CIDR/scheme/port)"
        );
    }
}

#[test]
fn validate_permitted_host_accepts_bare_hostnames() {
    use travsr_plugin_host::sandbox::policy::validate_permitted_host;
    for ok in &[
        "repo1.maven.org",
        "repo.maven.apache.org",
        "plugins.gradle.org",
    ] {
        assert!(
            validate_permitted_host(ok).is_ok(),
            "bare hostname '{ok}' should be accepted"
        );
    }
}

// ── Crash isolation ───────────────────────────────────────────────────────────

#[test]
fn sidecar_stub_disabled_health_is_isolated() {
    use std::path::PathBuf;
    use travsr_plugin_host::transport::{PluginHealth, Sidecar, Transport};
    use travsr_plugin_protocol::{InvokeRequest, ParseRequest};

    // A disabled Sidecar (stub) returns Err without panicking — daemon continues
    let sidecar = Sidecar::stub("java");
    assert!(
        matches!(sidecar.health(), PluginHealth::Disabled(_)),
        "stub sidecar should report Disabled health"
    );

    let req = ParseRequest {
        path: PathBuf::from("Test.java"),
        vname_path: "Test.java".into(),
        corpus: "test".into(),
        package: "".into(),
        source: None,
    };
    // parse() returns Err, not panic — daemon can handle the error and continue
    let result = sidecar.parse(req);
    assert!(
        result.is_err(),
        "disabled sidecar parse should return Err, not panic"
    );

    // invoke_phase_b also returns Err gracefully
    let invoke_req = InvokeRequest {
        corpus: String::new(),
        root: PathBuf::from("."),
        scratch: PathBuf::default(),
        files: None,
    };
    let invoke_result = sidecar.invoke_phase_b(invoke_req);
    assert!(
        invoke_result.is_err(),
        "disabled sidecar invoke_phase_b should return Err"
    );
}

#[test]
fn supervisor_disables_language_after_repeated_crashes() {
    use travsr_plugin_host::supervisor::Supervisor;

    let mut sup = Supervisor::new();
    assert!(!sup.is_disabled("rust"), "rust should start enabled");

    // Record crashes up to the limit (MAX_CRASHES = 3)
    sup.record_crash("rust", "segfault in grammar");
    assert!(!sup.is_disabled("rust"), "not disabled after 1 crash");

    sup.record_crash("rust", "segfault again");
    assert!(!sup.is_disabled("rust"), "not disabled after 2 crashes");

    sup.record_crash("rust", "segfault again");
    assert!(
        sup.is_disabled("rust"),
        "should be disabled after 3 crashes"
    );

    // Other languages unaffected
    assert!(
        !sup.is_disabled("go"),
        "unrelated language should not be disabled"
    );
}
