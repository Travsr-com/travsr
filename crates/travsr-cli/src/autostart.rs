use std::path::Path;

use anyhow::Context as _;

/// #507: the task name must be stable across travsr builds — `register` and
/// `unregister` typically run from different binaries (upgrade in between).
/// The previous `DefaultHasher` scheme is explicitly unstable across Rust
/// releases, so `daemon stop` after an upgrade computed a different `/tn`
/// and the old ONLOGON task was orphaned with no CLI way to remove it.
/// Reuse the repo identity scheme the control transport already uses
/// (`ControlAddr::for_repo`: canonicalized, case-folded, blake3-hashed).
fn task_name(repo_root: &Path) -> String {
    format!(
        r"Travsr\travsr-{}",
        travsr_ipc::ControlAddr::for_repo(repo_root)
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Register a Windows Task Scheduler ONLOGON task that restarts the daemon
/// after reboot. Writes a Task Scheduler 1.2 XML to a temp file, registers
/// it with `schtasks /create`, then removes the temp file.
///
/// Non-fatal — if schtasks is unavailable (Group Policy restriction, no UAC
/// elevation available) the daemon was already spawned and will work until
/// the next logout; the caller logs a warning and continues.
pub fn register(exe: &Path, repo_root: &Path) -> anyhow::Result<()> {
    let name = task_name(repo_root);
    let exe_str = xml_escape(&exe.to_string_lossy());
    let wd_str = xml_escape(&repo_root.to_string_lossy());
    let user = std::env::var("USERNAME").unwrap_or_default();
    let user_id_elem = if user.is_empty() {
        String::new()
    } else {
        format!("      <UserId>{}</UserId>\n", xml_escape(&user))
    };

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Travsr daemon auto-start on login</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
{user_id_elem}    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe_str}</Command>
      <Arguments>daemon start --foreground</Arguments>
      <WorkingDirectory>{wd_str}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    );

    // #507: the XML declares encoding="UTF-8", but schtasks /xml rejects
    // non-ASCII content in a BOM-less file — a non-ASCII username or repo
    // path failed registration. Write a UTF-8 BOM so schtasks decodes it as
    // declared.
    let mut xml_bytes = vec![0xEF, 0xBB, 0xBF];
    xml_bytes.extend_from_slice(xml.as_bytes());

    let tmp = std::env::temp_dir().join(format!("travsr-schtask-{}.xml", std::process::id()));
    std::fs::write(&tmp, &xml_bytes).context("writing task XML")?;

    // UX-020: capture stderr rather than discarding it, so a failure carries the
    // reason schtasks reported instead of a bare exit code.
    let output = std::process::Command::new("schtasks")
        .args([
            "/create",
            "/tn",
            &name,
            "/xml",
            tmp.to_str().unwrap_or(""),
            "/f",
        ])
        .output()
        .context("running schtasks /create")?;

    let _ = std::fs::remove_file(&tmp);

    if output.status.success() {
        // Prefer the Task Scheduler entry; drop any stale Startup-folder shim from
        // a previous fallback so the daemon is not started twice.
        let _ = remove_startup_shim(repo_root);
        return Ok(());
    }

    // UX-020: Task Scheduler can be blocked (Group Policy, no elevation, a
    // constrained XML parser). Rather than leave auto-start silently broken, fall
    // back to a per-user Startup-folder shim, which needs no elevation and no
    // Task Scheduler access. Surface the schtasks reason if the fallback also fails.
    let schtasks_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match install_startup_shim(exe, repo_root) {
        Ok(shim) => {
            tracing::info!(
                path = %shim.display(),
                "schtasks unavailable ({}); installed a Startup-folder auto-start shim instead",
                if schtasks_err.is_empty() { format!("exit {}", output.status) } else { schtasks_err.clone() }
            );
            Ok(())
        }
        Err(shim_err) => {
            let why = if schtasks_err.is_empty() {
                format!("schtasks /create exited {}", output.status)
            } else {
                format!("schtasks /create failed: {schtasks_err}")
            };
            anyhow::bail!(
                "{why}; Startup-folder fallback also failed ({shim_err:#}). \
                 The daemon is running now but will not auto-start after logout — \
                 start it manually with `travsr daemon start`."
            )
        }
    }
}

/// Per-user Startup folder: `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup`.
/// A `.cmd` dropped here runs on interactive logon with no elevation and no Task
/// Scheduler access — the UX-020 fallback when `schtasks` is unavailable.
fn startup_dir() -> anyhow::Result<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")
        .context("APPDATA is not set, cannot locate the Startup folder")?;
    Ok(Path::new(&appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup"))
}

/// Stable Startup-shim path for `repo_root`, keyed by the same build-stable repo
/// identity as the Task Scheduler name so register/unregister always agree.
fn startup_shim_path(repo_root: &Path) -> anyhow::Result<std::path::PathBuf> {
    let id = travsr_ipc::ControlAddr::for_repo(repo_root);
    Ok(startup_dir()?.join(format!("travsr-{id}.cmd")))
}

/// Write a Startup `.cmd` that launches the daemon on logon. `start "" /b` runs
/// it detached without leaving a console window open.
fn install_startup_shim(exe: &Path, repo_root: &Path) -> anyhow::Result<std::path::PathBuf> {
    let path = startup_shim_path(repo_root)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating Startup folder")?;
    }
    let script = format!(
        "@echo off\r\ncd /d \"{repo}\"\r\nstart \"\" /b \"{exe}\" daemon start\r\n",
        repo = repo_root.to_string_lossy(),
        exe = exe.to_string_lossy(),
    );
    std::fs::write(&path, script)
        .with_context(|| format!("writing Startup shim to {}", path.display()))?;
    Ok(path)
}

/// Remove the Startup shim for `repo_root` if present. Missing is success.
fn remove_startup_shim(repo_root: &Path) -> anyhow::Result<()> {
    let path = startup_shim_path(repo_root)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing Startup shim {}", path.display())),
    }
}

/// Remove the Task Scheduler auto-start task for this repo.
/// Silent if the task does not exist — schtasks exits non-zero in that case and
/// we treat it as already-gone.  Only propagates errors from failing to spawn
/// schtasks at all (e.g. binary not on PATH).
pub fn unregister(repo_root: &Path) -> anyhow::Result<()> {
    let name = task_name(repo_root);
    let _ = std::process::Command::new("schtasks")
        .args(["/delete", "/tn", &name, "/f"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("running schtasks /delete")?;
    // UX-020: also remove the Startup-folder fallback shim, so a repo registered
    // via the fallback is fully torn down by `daemon stop`.
    let _ = remove_startup_shim(repo_root);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_name_is_stable() {
        let p = Path::new(r"C:\Users\user\my-repo");
        assert_eq!(task_name(p), task_name(p));
        assert!(task_name(p).starts_with(r"Travsr\travsr-"));
        assert_eq!(task_name(p).len(), "Travsr\\travsr-".len() + 16);
    }

    /// #507: the name must come from the build-stable ControlAddr scheme,
    /// not DefaultHasher (unstable across Rust releases), so unregister
    /// finds the task registered by an older travsr build.
    #[test]
    fn task_name_uses_control_addr_identity() {
        let p = Path::new(r"C:\Users\user\my-repo");
        let expected = travsr_ipc::ControlAddr::for_repo(p).to_string();
        assert_eq!(task_name(p), format!(r"Travsr\travsr-{expected}"));
    }

    #[test]
    fn task_name_differs_per_repo() {
        let a = task_name(Path::new(r"C:\repo-a"));
        let b = task_name(Path::new(r"C:\repo-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn xml_escape_handles_special_chars() {
        assert_eq!(xml_escape("a&b<c>d\"e"), "a&amp;b&lt;c&gt;d&quot;e");
        assert_eq!(xml_escape(r"C:\normal\path"), r"C:\normal\path");
    }

    /// UX-020: the Startup-folder fallback shim is keyed by the same build-stable
    /// repo identity as the Task Scheduler name, so register and unregister always
    /// target the same file, and it lands under the per-user Startup folder.
    #[test]
    fn startup_shim_path_is_stable_and_under_startup() {
        std::env::set_var("APPDATA", r"C:\Users\user\AppData\Roaming");
        let p = Path::new(r"C:\Users\user\my-repo");
        let a = startup_shim_path(p).unwrap();
        let b = startup_shim_path(p).unwrap();
        assert_eq!(a, b, "shim path must be deterministic per repo");
        assert!(a.ends_with(format!(
            "travsr-{}.cmd",
            travsr_ipc::ControlAddr::for_repo(p)
        )));
        assert!(a.to_string_lossy().contains(r"Start Menu\Programs\Startup"));
    }
}
