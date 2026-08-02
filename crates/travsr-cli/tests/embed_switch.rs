// #481/#482/#483: `travsr embed switch` used to always write the machine-global
// config (~/.travsr/embed.toml), which has no effect on an indexed repo's
// retrieval (that reads <repo>/.travsr/embed.toml). These tests pin the fix:
// switch writes the layer the user meant, and `status`/`list` stop disagreeing
// about which model is active.
//
// Every test here isolates the machine-global layer by pointing `$HOME` at a
// fresh tempdir, so a test run never reads or writes this machine's real
// `~/.travsr/`. That works on Unix (`dirs::home_dir()` reads `$HOME`), but not
// on Windows: `dirs-sys`'s Windows `home_dir()` calls `SHGetKnownFolderPath`
// directly and does not consult any environment variable, `HOME` or
// `USERPROFILE` included (verified by reading dirs-sys 0.4.1's source) - so
// there the CLI always resolves the real profile directory regardless of
// what a test sets, and the fake-install setup below never lands where the
// binary under test actually looks. `#[cfg_attr(windows, ignore)]` on each
// test records that as a testing-environment limitation, not a product bug.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use travsr_plugin_host::EmbedBackend;

fn git_init(dir: &Path) {
    StdCommand::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .current_dir(dir)
        .status()
        .expect("git init failed");
    StdCommand::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .status()
        .expect("git config email failed");
    StdCommand::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .status()
        .expect("git config name failed");
}

/// Marks a backend as fully installed (sidecar binary + model weights +
/// descriptor) under a fake `$HOME`, so `cmd_switch`/`cmd_status`/`cmd_list`
/// treat it as usable without a real download.
fn install_backend_files(home: &Path, backend: &EmbedBackend) {
    let bin_dir = home.join(".travsr").join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(bin_dir.join(&backend.binary_name), b"").unwrap();

    let model_dir = home.join(".travsr").join("models").join(&backend.id);
    std::fs::create_dir_all(&model_dir).unwrap();
    for f in &backend.model_files {
        std::fs::write(model_dir.join(&f.name), b"").unwrap();
    }
    std::fs::write(model_dir.join("model.toml"), b"").unwrap();
}

/// A git repo `travsr init` would recognise, with just enough `.travsr/`
/// state (an empty `graph.db`) for `cmd_switch`'s repo-detection check.
fn indexed_repo(dir: &Path) {
    git_init(dir);
    std::fs::create_dir_all(dir.join(".travsr")).unwrap();
    std::fs::write(dir.join(".travsr/graph.db"), b"").unwrap();
}

fn repo_config_active(repo: &Path) -> Option<String> {
    let content = std::fs::read_to_string(repo.join(".travsr/embed.toml")).ok()?;
    content
        .lines()
        .find_map(|l| l.strip_prefix("active = \"")?.strip_suffix('"'))
        .map(String::from)
}

fn machine_config_active(home: &Path) -> Option<String> {
    let content = std::fs::read_to_string(home.join(".travsr/embed.toml")).ok()?;
    let parsed: toml::Value = toml::from_str(&content).ok()?;
    parsed.get("active")?.as_str().map(String::from)
}

#[test]
#[cfg_attr(
    windows,
    ignore = "dirs::home_dir() on Windows ignores HOME/USERPROFILE entirely (SHGetKnownFolderPath) - this test's isolation cannot work there, see module doc comment"
)]
fn switch_in_a_repo_writes_the_repo_config_not_the_machine_config() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let backend = &travsr_plugin_host::embed_backends()[0];
    install_backend_files(home.path(), backend);
    indexed_repo(repo.path());

    Command::cargo_bin("travsr")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(repo.path())
        .args(["embed", "switch", &backend.id])
        .assert()
        .success();

    assert_eq!(
        repo_config_active(repo.path()).as_deref(),
        Some(backend.id.as_str()),
        "switch inside a repo must write the repo's own embed.toml"
    );
    assert_eq!(
        machine_config_active(home.path()),
        None,
        "switch inside a repo must NOT touch the machine-global config"
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "dirs::home_dir() on Windows ignores HOME/USERPROFILE entirely (SHGetKnownFolderPath) - this test's isolation cannot work there, see module doc comment"
)]
fn switch_with_global_writes_the_machine_config() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let backend = &travsr_plugin_host::embed_backends()[0];
    install_backend_files(home.path(), backend);
    indexed_repo(repo.path());

    Command::cargo_bin("travsr")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(repo.path())
        .args(["embed", "switch", &backend.id, "--global"])
        .assert()
        .success();

    assert_eq!(
        machine_config_active(home.path()).as_deref(),
        Some(backend.id.as_str()),
        "--global must write the machine-global config"
    );
    assert_eq!(
        repo_config_active(repo.path()),
        None,
        "--global must NOT touch the repo's own config"
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "dirs::home_dir() on Windows ignores HOME/USERPROFILE entirely (SHGetKnownFolderPath) - this test's isolation cannot work there, see module doc comment"
)]
fn status_names_both_layers_when_they_diverge() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let backends = travsr_plugin_host::embed_backends();
    let (a, b) = (&backends[0], &backends[1]);
    install_backend_files(home.path(), a);
    install_backend_files(home.path(), b);
    indexed_repo(repo.path());

    // Machine default is `a`, this repo is configured for `b` — a real
    // divergence (#481/#482/#483's scenario).
    std::fs::write(
        home.path().join(".travsr/embed.toml"),
        format!("active = \"{}\"\n", a.id),
    )
    .unwrap();
    std::fs::write(
        repo.path().join(".travsr/embed.toml"),
        format!("active = \"{}\"\n", b.id),
    )
    .unwrap();

    let out = Command::cargo_bin("travsr")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(repo.path())
        .args(["embed", "status"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains(&format!("Repo model     : {}", b.id)),
        "status must name the repo's own model as the winner, got:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("machine default is '{}'", a.id)),
        "status must name the diverging machine default when the two disagree, got:\n{stdout}"
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "dirs::home_dir() on Windows ignores HOME/USERPROFILE entirely (SHGetKnownFolderPath) - this test's isolation cannot work there, see module doc comment"
)]
fn status_stays_single_line_when_layers_agree() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let backend = &travsr_plugin_host::embed_backends()[0];
    install_backend_files(home.path(), backend);
    indexed_repo(repo.path());

    std::fs::write(
        home.path().join(".travsr/embed.toml"),
        format!("active = \"{}\"\n", backend.id),
    )
    .unwrap();
    std::fs::write(
        repo.path().join(".travsr/embed.toml"),
        format!("active = \"{}\"\n", backend.id),
    )
    .unwrap();

    let out = Command::cargo_bin("travsr")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(repo.path())
        .args(["embed", "status"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        !stdout.contains("machine default is"),
        "status must not print the divergence line when both layers agree, got:\n{stdout}"
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "dirs::home_dir() on Windows ignores HOME/USERPROFILE entirely (SHGetKnownFolderPath) - this test's isolation cannot work there, see module doc comment"
)]
fn list_reports_installed_from_model_files_not_the_shared_binary() {
    let home = tempfile::tempdir().unwrap();
    let backends = travsr_plugin_host::embed_backends();
    let (fully_installed, binary_only) = (&backends[0], &backends[1]);

    install_backend_files(home.path(), fully_installed);
    // #482's exact bug shape: the shared sidecar binary is present (as it
    // would be for ANY backend once one is installed) but this backend's own
    // model weights never were.
    let bin_dir = home.path().join(".travsr").join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(bin_dir.join(&binary_only.binary_name), b"").unwrap();

    let out = Command::cargo_bin("travsr")
        .unwrap()
        .env("HOME", home.path())
        .args(["embed", "list", "--json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let entries = parsed.as_array().unwrap();

    let find = |id: &str| entries.iter().find(|e| e["id"] == id).unwrap();
    assert_eq!(
        find(&fully_installed.id)["installed"],
        true,
        "a backend with model files on disk must report installed"
    );
    assert_eq!(
        find(&binary_only.id)["installed"],
        false,
        "a backend with only the shared sidecar binary (no model files) must NOT report installed"
    );
}
