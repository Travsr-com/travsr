//! Windows AppContainer + Job Object security tests (RFC-014 / ADR-017).
//! All tests require Windows and administrator or SeCreateSymbolicLinkPrivilege.
//! T1-T5 are marked #[ignore] (slow / need elevated privileges) and run in CI
//! with `cargo test --test sandbox_windows -- --ignored`.
//! T6 (fail_closed) is always run.

#[cfg(target_os = "windows")]
mod windows_sandbox {
    use travsr_plugin_host::sandbox::policy::SandboxPolicy;
    use travsr_plugin_host::sandbox::windows::build_sandboxed_command;

    fn standard_policy() -> SandboxPolicy {
        SandboxPolicy::Standard
    }

    fn temp_repo() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir for repo")
    }

    fn temp_scratch() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir for scratch")
    }

    // ── T1: AppContainer blocks outbound TCP ─────────────────────────────────

    /// Standard policy: the AppContainer has no internet client capability,
    /// so outbound TCP connections should fail.
    #[test]
    #[ignore = "requires Windows with AppContainer privilege; run explicitly in CI"]
    fn t1_appcontainer_blocks_outbound_tcp() {
        let repo = temp_repo();
        let scratch = temp_scratch();

        // PowerShell tries to open TCP to 8.8.8.8:80. We expect it to fail.
        let spawner = build_sandboxed_command(
            "powershell.exe",
            &[
                "-NonInteractive",
                "-Command",
                "try { $c = New-Object System.Net.Sockets.TcpClient; \
                 $c.Connect('8.8.8.8', 80); exit 0 } catch { exit 1 }",
            ],
            repo.path(),
            scratch.path(),
            &standard_policy(),
            "",
        )
        .expect("build_sandboxed_command must succeed");

        let output = spawner.output().expect("spawn");
        assert!(
            !output.status.success(),
            "AppContainer Standard policy should block outbound TCP (exit 0 means egress succeeded)"
        );
    }

    // ── T2: AppContainer blocks writes outside scratch ────────────────────────

    /// Attempting to write outside the scratch directory should fail inside the
    /// AppContainer (the AppContainer SID has no ACL on arbitrary system paths).
    #[test]
    #[ignore = "requires Windows with AppContainer privilege; run explicitly in CI"]
    fn t2_appcontainer_blocks_write_outside_scratch() {
        let repo = temp_repo();
        let scratch = temp_scratch();
        // Use a path the AppContainer definitely cannot write to.
        let target = std::env::temp_dir().join("travsr_appcontainer_breach_test.tmp");
        let target_str = target.display().to_string().replace('\'', "''");

        let script = format!(
            "try {{ [IO.File]::WriteAllText('{target_str}', 'x'); exit 0 }} catch {{ exit 1 }}"
        );
        let spawner = build_sandboxed_command(
            "powershell.exe",
            &["-NonInteractive", "-Command", &script],
            repo.path(),
            scratch.path(),
            &standard_policy(),
            "",
        )
        .expect("build_sandboxed_command must succeed");

        let output = spawner.output().expect("spawn");
        // The write should fail (exit 1) because temp_dir is not granted to the
        // AppContainer SID. If exit 0: the container escaped FS isolation.
        assert!(
            !output.status.success() || !target.exists(),
            "AppContainer allowed write outside scratch, FS confinement broken"
        );
        let _ = std::fs::remove_file(&target); // cleanup if write somehow succeeded
    }

    // ── T3: AppContainer allows reading from repo root ────────────────────────

    /// The AppContainer SID is granted FILE_GENERIC_READ on repo_root, so reads
    /// from within the repo should succeed.
    #[test]
    #[ignore = "requires Windows with AppContainer privilege; run explicitly in CI"]
    fn t3_appcontainer_allows_repo_read() {
        let repo = temp_repo();
        let scratch = temp_scratch();

        // Write a sentinel file into the repo.
        let sentinel = repo.path().join("sentinel.txt");
        std::fs::write(&sentinel, b"hello travsr").expect("write sentinel");
        let sentinel_str = sentinel.display().to_string().replace('\'', "''");

        let script = format!(
            "try {{ $c = [IO.File]::ReadAllText('{sentinel_str}'); \
             if ($c -eq 'hello travsr') {{ exit 0 }} else {{ exit 2 }} }} \
             catch {{ exit 1 }}"
        );
        let spawner = build_sandboxed_command(
            "powershell.exe",
            &["-NonInteractive", "-Command", &script],
            repo.path(),
            scratch.path(),
            &standard_policy(),
            "",
        )
        .expect("build_sandboxed_command must succeed");

        let output = spawner.output().expect("spawn");
        assert!(
            output.status.success(),
            "AppContainer should allow reading from repo root (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // ── T4: AppContainer allows writes to scratch ─────────────────────────────

    /// The AppContainer SID is granted FILE_ALL_ACCESS on scratch_dir, so writes
    /// inside it should succeed.
    #[test]
    #[ignore = "requires Windows with AppContainer privilege; run explicitly in CI"]
    fn t4_appcontainer_allows_scratch_write() {
        let repo = temp_repo();
        let scratch = temp_scratch();
        let test_file = scratch.path().join("write_test.txt");
        let test_file_str = test_file.display().to_string().replace('\'', "''");

        let script = format!(
            "try {{ [IO.File]::WriteAllText('{test_file_str}', 'ok'); exit 0 }} catch {{ exit 1 }}"
        );
        let spawner = build_sandboxed_command(
            "powershell.exe",
            &["-NonInteractive", "-Command", &script],
            repo.path(),
            scratch.path(),
            &standard_policy(),
            "",
        )
        .expect("build_sandboxed_command must succeed");

        let output = spawner.output().expect("spawn");
        assert!(
            output.status.success(),
            "AppContainer should allow writing to scratch dir (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            test_file.exists(),
            "scratch write succeeded but file not found on host"
        );
    }

    // ── T5: Job Object memory limit kills an over-allocating process ──────────

    /// The Job Object is configured with a 4 GiB memory limit. A process that
    /// tries to exceed this should be killed. We use a small test limit here
    /// (the production limit is 4 GiB; we verify the mechanism works).
    #[test]
    #[ignore = "allocates significant memory; run explicitly in CI"]
    fn t5_job_object_memory_limit_kills_process() {
        let repo = temp_repo();
        let scratch = temp_scratch();

        // Attempt to allocate 6 GiB (well above the 4 GiB job limit).
        // The process should be killed by the Job Object before completing.
        let spawner = build_sandboxed_command(
            "powershell.exe",
            &[
                "-NonInteractive",
                "-Command",
                // Try to allocate 6 GiB. The 4 GiB Job Object memory limit causes
                // VirtualAlloc to fail, which .NET surfaces as OutOfMemoryException.
                // PowerShell does not exit non-zero on OOM by default, so we
                // use try-catch to map OOM → exit 1, success → exit 0.
                "try { [void][byte[]]::new(6GB); exit 0 } catch { exit 1 }",
            ],
            repo.path(),
            scratch.path(),
            &standard_policy(),
            "",
        )
        .expect("build_sandboxed_command must succeed");

        let output = spawner.output().expect("spawn started");
        // exit 1 = OOM was thrown (Job Object limit enforced — correct).
        // exit 0 = allocation succeeded (Job Object limit not enforced — broken).
        assert!(
            !output.status.success(),
            "process allocating 6 GiB should have hit the 4 GiB Job Object memory limit \
             (VirtualAlloc failed → OutOfMemoryException → exit 1); exit 0 means the limit \
             is not being enforced"
        );
    }

    // ── T6: Fail-closed — no unsandboxed fallback ─────────────────────────────

    /// ADR-017 Rule 2: if the sandbox cannot be applied, the plugin is DISABLED.
    /// `build_sandboxed_command` must never return a wrapped unsandboxed command.
    /// On Windows, it must return `SandboxedSpawn::AppContainer`, never `Wrapped`.
    ///
    /// This test verifies at the type level: calling `build_sandboxed_command`
    /// returns the AppContainer variant, not the Wrapped fallback.
    #[test]
    fn t6_fail_closed_no_unsandboxed_fallback() {
        let repo = temp_repo();
        let scratch = temp_scratch();

        let result = build_sandboxed_command(
            "powershell.exe",
            &["-NonInteractive", "-Command", "exit 0"],
            repo.path(),
            scratch.path(),
            &standard_policy(),
            "",
        );

        // On Windows, build_sandboxed_command must succeed and return an
        // AppContainer spawn (not Err and not a Wrapped unsandboxed command).
        // If it returned Err, that would be correct (fail-closed).
        // If it returned Wrapped (unsandboxed), that violates ADR-017 Rule 2.
        match result {
            Err(_) => {
                // Fail-closed — correct. Sandbox unavailable → plugin disabled.
            }
            Ok(travsr_plugin_host::sandbox::SandboxedSpawn::AppContainer(_)) => {
                // Correct: Windows always returns AppContainer, never Wrapped.
            }
            Ok(travsr_plugin_host::sandbox::SandboxedSpawn::Wrapped(_)) => {
                panic!(
                    "ADR-017 Rule 2 VIOLATED: build_sandboxed_command returned an \
                     unsandboxed Wrapped command on Windows — plugin would run \
                     without AppContainer isolation"
                );
            }
        }
    }
}
