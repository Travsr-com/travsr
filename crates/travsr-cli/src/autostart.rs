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

    let status = std::process::Command::new("schtasks")
        .args([
            "/create",
            "/tn",
            &name,
            "/xml",
            tmp.to_str().unwrap_or(""),
            "/f",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("running schtasks /create")?;

    let _ = std::fs::remove_file(&tmp);
    anyhow::ensure!(status.success(), "schtasks /create exited {status}");
    Ok(())
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
}
