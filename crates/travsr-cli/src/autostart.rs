use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::Context as _;

fn task_name(repo_root: &Path) -> String {
    let mut h = DefaultHasher::new();
    repo_root.hash(&mut h);
    format!(r"Travsr\travsr-{:016x}", h.finish())
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

    let tmp = std::env::temp_dir().join(format!("travsr-schtask-{}.xml", std::process::id()));
    std::fs::write(&tmp, xml.as_bytes()).context("writing task XML")?;

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
/// Non-fatal if the task does not exist (schtasks exits non-zero in that case).
pub fn unregister(repo_root: &Path) -> anyhow::Result<()> {
    let name = task_name(repo_root);
    let status = std::process::Command::new("schtasks")
        .args(["/delete", "/tn", &name, "/f"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("running schtasks /delete")?;
    anyhow::ensure!(status.success(), "schtasks /delete exited {status}");
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
