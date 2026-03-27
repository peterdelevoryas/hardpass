use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio::time::sleep;

use crate::lock::{lock_file, sibling_lock_path};
use crate::state::SshConfig;

#[derive(Debug)]
pub struct ExecOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

pub async fn ensure_ssh_key(identity_file: &Path) -> Result<String> {
    let _lock = lock_file(sibling_lock_path(identity_file)).await?;
    if !identity_file.is_file() || !identity_file.with_extension("pub").is_file() {
        if let Some(parent) = identity_file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let output = Command::new("ssh-keygen")
            .arg("-q")
            .arg("-t")
            .arg("ed25519")
            .arg("-N")
            .arg("")
            .arg("-f")
            .arg(identity_file)
            .arg("-C")
            .arg("hardpass")
            .output()
            .await
            .context("run ssh-keygen")?;
        if !output.status.success() {
            bail!(
                "ssh-keygen failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    let public_key = tokio::fs::read_to_string(identity_file.with_extension("pub")).await?;
    Ok(public_key.trim().to_string())
}

pub async fn wait_for_ssh(config: &SshConfig, timeout_secs: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match ssh_status(config, &["true"]).await {
            Ok(()) => return Ok(()),
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                sleep(Duration::from_millis(500)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

pub async fn open_session(config: &SshConfig, extra_args: &[String]) -> Result<()> {
    let status = Command::new("ssh")
        .args(common_ssh_args(config, false))
        .args(extra_args)
        .arg(format!("{}@{}", config.user, config.host))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("run ssh")?;
    if status.success() {
        Ok(())
    } else {
        bail!("ssh exited with status {status}");
    }
}

pub async fn exec(config: &SshConfig, command: &[String]) -> Result<()> {
    let output = exec_capture(config, command).await?;
    if output.status.success() {
        print!("{}", output.stdout);
        eprint!("{}", output.stderr);
        Ok(())
    } else {
        if !output.stdout.is_empty() {
            print!("{}", output.stdout);
        }
        if !output.stderr.is_empty() {
            eprint!("{}", output.stderr);
        }
        bail!("remote command exited with status {}", output.status);
    }
}

pub async fn exec_capture(config: &SshConfig, command: &[String]) -> Result<ExecOutput> {
    let remote_command = render_remote_command(command);
    let output = Command::new("ssh")
        .args(common_ssh_args(config, true))
        .arg(format!("{}@{}", config.user, config.host))
        .arg(&remote_command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("run ssh")?;
    Ok(ExecOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

pub async fn exec_checked(config: &SshConfig, command: &[String]) -> Result<ExecOutput> {
    let output = exec_capture(config, command).await?;
    if output.status.success() {
        Ok(output)
    } else {
        bail!("remote command exited with status {}", output.status);
    }
}

async fn ssh_status(config: &SshConfig, remote_command: &[&str]) -> Result<()> {
    let status = Command::new("ssh")
        .args(common_ssh_args(config, true))
        .arg("-o")
        .arg("ConnectTimeout=2")
        .arg(format!("{}@{}", config.user, config.host))
        .args(remote_command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("run ssh readiness probe")?;
    if status.success() {
        Ok(())
    } else {
        bail!("ssh not ready yet")
    }
}

fn common_ssh_args(config: &SshConfig, batch_mode: bool) -> Vec<String> {
    let mut args = vec![
        "-i".to_string(),
        config.identity_file.display().to_string(),
        "-p".to_string(),
        config.port.to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "IdentitiesOnly=yes".to_string(),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
    ];
    if batch_mode {
        args.extend(["-o".to_string(), "BatchMode=yes".to_string()]);
    }
    args
}

fn render_remote_command(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::{render_remote_command, shell_quote};

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's ready"), r#"'it'"'"'s ready'"#);
    }

    #[test]
    fn render_remote_command_preserves_shell_script_argument() {
        let command = vec![
            "sh".to_string(),
            "-lc".to_string(),
            "sudo apt-get update".to_string(),
        ];
        assert_eq!(
            render_remote_command(&command),
            "'sh' '-lc' 'sudo apt-get update'"
        );
    }

    #[test]
    fn render_remote_command_preserves_empty_arguments() {
        let command = vec!["printf".to_string(), "".to_string(), "done".to_string()];
        assert_eq!(render_remote_command(&command), "'printf' '' 'done'");
    }
}
