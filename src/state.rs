use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;

const DEFAULT_RELEASE: &str = "24.04";
const DEFAULT_CPU_COUNT: u8 = 4;
const DEFAULT_MEMORY_MIB: u32 = 4096;
const DEFAULT_DISK_GIB: u32 = 24;
const DEFAULT_TIMEOUT_SECS: u64 = 180;
const DEFAULT_SSH_USER: &str = "ubuntu";
const DEFAULT_SSH_HOST: &str = "127.0.0.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum GuestArch {
    Amd64,
    Arm64,
}

impl GuestArch {
    pub fn host_native() -> Result<Self> {
        match std::env::consts::ARCH {
            "x86_64" => Ok(Self::Amd64),
            "aarch64" | "arm64" => Ok(Self::Arm64),
            other => bail!("unsupported host architecture: {other}"),
        }
    }

    pub fn ubuntu_arch(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        }
    }

    pub fn qemu_binary(self) -> &'static str {
        match self {
            Self::Amd64 => "qemu-system-x86_64",
            Self::Arm64 => "qemu-system-aarch64",
        }
    }
}

impl Display for GuestArch {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AccelMode {
    Auto,
    Hvf,
    Kvm,
    Tcg,
}

impl Display for AccelMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Hvf => "hvf",
            Self::Kvm => "kvm",
            Self::Tcg => "tcg",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortForward {
    pub host: u16,
    pub guest: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshConfig {
    pub user: String,
    pub host: String,
    pub port: u16,
    pub identity_file: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageConfig {
    pub release: String,
    pub arch: GuestArch,
    pub url: String,
    pub sha256_url: String,
    pub filename: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloudInitConfig {
    pub user_data_sha256: String,
    pub network_config_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceConfig {
    pub name: String,
    pub release: String,
    pub arch: GuestArch,
    pub accel: AccelMode,
    pub cpus: u8,
    pub memory_mib: u32,
    pub disk_gib: u32,
    pub timeout_secs: u64,
    pub ssh: SshConfig,
    pub forwards: Vec<PortForward>,
    pub image: ImageConfig,
    pub cloud_init: CloudInitConfig,
}

impl InstanceConfig {
    pub fn default_release() -> &'static str {
        DEFAULT_RELEASE
    }

    pub fn default_cpus() -> u8 {
        DEFAULT_CPU_COUNT
    }

    pub fn default_memory_mib() -> u32 {
        DEFAULT_MEMORY_MIB
    }

    pub fn default_disk_gib() -> u32 {
        DEFAULT_DISK_GIB
    }

    pub fn default_timeout_secs() -> u64 {
        DEFAULT_TIMEOUT_SECS
    }

    pub fn default_ssh_user() -> &'static str {
        DEFAULT_SSH_USER
    }

    pub fn default_ssh_host() -> &'static str {
        DEFAULT_SSH_HOST
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    Missing,
    Stopped,
    Running,
}

impl Display for InstanceStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Missing => "missing",
            Self::Stopped => "stopped",
            Self::Running => "running",
        })
    }
}

#[derive(Debug, Clone)]
pub struct HardpassState {
    root: PathBuf,
    manages_ssh_config: bool,
    auto_sync_ssh_config: bool,
}

impl HardpassState {
    pub async fn load() -> Result<Self> {
        let default_root = default_root_path()?;
        let (root, auto_sync_ssh_config) = if let Some(explicit) = std::env::var_os("HARDPASS_HOME")
        {
            (PathBuf::from(explicit), false)
        } else {
            (default_root.clone(), true)
        };
        let manages_ssh_config = paths_match(&root, &default_root)?;
        Self::load_with_flags(root, manages_ssh_config, auto_sync_ssh_config).await
    }

    pub(crate) async fn load_with_root(root: PathBuf) -> Result<Self> {
        let manages_ssh_config = paths_match(&root, &default_root_path()?)?;
        Self::load_with_flags(root, manages_ssh_config, false).await
    }

    async fn load_with_flags(
        root: PathBuf,
        manages_ssh_config: bool,
        auto_sync_ssh_config: bool,
    ) -> Result<Self> {
        tokio::fs::create_dir_all(root.join("images")).await?;
        tokio::fs::create_dir_all(root.join("instances")).await?;
        tokio::fs::create_dir_all(root.join("keys")).await?;
        tokio::fs::create_dir_all(root.join("locks")).await?;
        Ok(Self {
            root,
            manages_ssh_config,
            auto_sync_ssh_config,
        })
    }

    #[cfg(test)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn images_dir(&self) -> PathBuf {
        self.root.join("images")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }

    pub fn instances_dir(&self) -> PathBuf {
        self.root.join("instances")
    }

    pub fn keys_dir(&self) -> PathBuf {
        self.root.join("keys")
    }

    pub fn default_ssh_key_path(&self) -> PathBuf {
        self.keys_dir().join("id_ed25519")
    }

    pub fn ports_lock_path(&self) -> PathBuf {
        self.locks_dir().join("ports.lock")
    }

    pub fn ssh_config_lock_path(&self) -> PathBuf {
        self.locks_dir().join("ssh-config.lock")
    }

    pub fn manages_ssh_config(&self) -> bool {
        self.manages_ssh_config
    }

    pub fn should_auto_sync_ssh_config(&self) -> bool {
        self.manages_ssh_config && self.auto_sync_ssh_config
    }

    pub fn instance_paths(&self, name: &str) -> Result<InstancePaths> {
        validate_name(name)?;
        Ok(InstancePaths::new(self.instances_dir().join(name)))
    }

    pub async fn instance_names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut dir = tokio::fs::read_dir(self.instances_dir()).await?;
        while let Some(entry) = dir.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        Ok(names)
    }
}

#[derive(Debug, Clone)]
pub struct InstancePaths {
    pub dir: PathBuf,
    pub config: PathBuf,
    pub disk: PathBuf,
    pub seed: PathBuf,
    pub pid: PathBuf,
    pub qmp: PathBuf,
    pub serial: PathBuf,
    pub firmware_vars: PathBuf,
}

impl InstancePaths {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            config: dir.join("config.json"),
            disk: dir.join("disk.qcow2"),
            seed: dir.join("seed.img"),
            pid: dir.join("pid"),
            qmp: dir.join("qmp.sock"),
            serial: dir.join("serial.log"),
            firmware_vars: dir.join("firmware.vars.fd"),
            dir,
        }
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.with_extension("lock")
    }

    pub async fn ensure_dir(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        Ok(())
    }

    pub async fn read_config(&self) -> Result<InstanceConfig> {
        let content = tokio::fs::read_to_string(&self.config)
            .await
            .with_context(|| format!("read {}", self.config.display()))?;
        serde_json::from_str(&content).with_context(|| format!("parse {}", self.config.display()))
    }

    pub async fn write_config(&self, config: &InstanceConfig) -> Result<()> {
        self.ensure_dir().await?;
        let payload = serde_json::to_vec_pretty(config)?;
        atomic_write(&self.config, &payload).await
    }

    pub async fn read_pid(&self) -> Result<Option<u32>> {
        match tokio::fs::read_to_string(&self.pid).await {
            Ok(raw) => {
                let pid = raw
                    .trim()
                    .parse::<u32>()
                    .with_context(|| format!("parse pid file {}", self.pid.display()))?;
                Ok(Some(pid))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub async fn status(&self) -> Result<InstanceStatus> {
        if tokio::fs::metadata(&self.config).await.is_err() {
            return Ok(InstanceStatus::Missing);
        }
        let Some(pid) = self.read_pid().await? else {
            return Ok(InstanceStatus::Stopped);
        };
        if process_is_alive(pid) && process_matches_instance(pid, self).await {
            Ok(InstanceStatus::Running)
        } else {
            Ok(InstanceStatus::Stopped)
        }
    }

    pub async fn clear_runtime_artifacts(&self) -> Result<()> {
        remove_if_exists(&self.pid).await?;
        remove_if_exists(&self.qmp).await?;
        Ok(())
    }

    pub async fn remove_all(&self) -> Result<()> {
        match tokio::fs::remove_dir_all(&self.dir).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

pub async fn atomic_write(path: &Path, payload: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    tokio::fs::write(&tmp, payload).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

pub async fn remove_if_exists(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub async fn sha256_file(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<String> {
        let file = std::fs::File::open(&path)?;
        let mut reader = std::io::BufReader::new(file);
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 1024 * 1024];
        loop {
            let read = std::io::Read::read(&mut reader, &mut buf)?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await?
}

pub fn process_is_alive(pid: u32) -> bool {
    nix::sys::signal::kill(Pid::from_raw(pid as i32), None).is_ok()
}

async fn process_matches_instance(pid: u32, paths: &InstancePaths) -> bool {
    let output = match Command::new("ps")
        .arg("-ww")
        .arg("-o")
        .arg("command=")
        .arg("-p")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
    {
        Ok(output) if output.status.success() => output,
        _ => return false,
    };

    let command = String::from_utf8_lossy(&output.stdout);
    let pid_path = paths.pid.to_string_lossy().into_owned();
    let qmp_path = paths.qmp.to_string_lossy().into_owned();
    let serial_path = paths.serial.to_string_lossy().into_owned();
    let expected = ["qemu-system-".to_string(), pid_path, qmp_path, serial_path];
    expected
        .into_iter()
        .all(|needle| command.contains(needle.as_str()))
}

pub async fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("instance name must not be empty");
    }
    if name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        Ok(())
    } else {
        bail!("instance name may only contain ASCII letters, digits, '-' and '_'");
    }
}

fn default_root_path() -> Result<PathBuf> {
    dirs::home_dir()
        .ok_or_else(|| anyhow!("unable to determine home directory"))
        .map(|home| home.join(".hardpass"))
}

fn paths_match(left: &Path, right: &Path) -> Result<bool> {
    Ok(normalize_path_for_compare(left)? == normalize_path_for_compare(right)?)
}

fn normalize_path_for_compare(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use tempfile::tempdir;
    use tokio::process::Command;

    use super::{
        CloudInitConfig, HardpassState, ImageConfig, InstanceConfig, InstancePaths, InstanceStatus,
        PortForward, SshConfig, atomic_write,
    };
    use crate::state::{AccelMode, GuestArch};

    fn test_config(dir: &std::path::Path) -> InstanceConfig {
        InstanceConfig {
            name: "vm".into(),
            release: "24.04".into(),
            arch: GuestArch::Arm64,
            accel: AccelMode::Auto,
            cpus: 2,
            memory_mib: 2048,
            disk_gib: 12,
            timeout_secs: 30,
            ssh: SshConfig {
                user: "ubuntu".into(),
                host: "127.0.0.1".into(),
                port: 2222,
                identity_file: dir.join("id_ed25519"),
            },
            forwards: vec![PortForward {
                host: 8080,
                guest: 8080,
            }],
            image: ImageConfig {
                release: "24.04".into(),
                arch: GuestArch::Arm64,
                url: "https://example.invalid".into(),
                sha256_url: "https://example.invalid/SHA256SUMS".into(),
                filename: "ubuntu.img".into(),
                sha256: "abc".into(),
            },
            cloud_init: CloudInitConfig {
                user_data_sha256: "abc".into(),
                network_config_sha256: None,
            },
        }
    }

    #[tokio::test]
    async fn state_uses_env_override() {
        let dir = tempdir().expect("tempdir");
        let state = HardpassState::load_with_root(dir.path().to_path_buf())
            .await
            .expect("load");
        assert_eq!(state.root(), dir.path());
    }

    #[tokio::test]
    async fn status_missing_without_config() {
        let dir = tempdir().expect("tempdir");
        let paths = InstancePaths::new(dir.path().join("vm"));
        assert_eq!(
            paths.status().await.expect("status"),
            InstanceStatus::Missing
        );
    }

    #[tokio::test]
    async fn status_stopped_with_config_only() {
        let dir = tempdir().expect("tempdir");
        let paths = InstancePaths::new(dir.path().join("vm"));
        let config = test_config(dir.path());
        paths.write_config(&config).await.expect("write config");
        assert_eq!(
            paths.status().await.expect("status"),
            InstanceStatus::Stopped
        );
    }

    #[tokio::test]
    async fn status_ignores_alive_process_with_unrelated_pid() {
        let dir = tempdir().expect("tempdir");
        let paths = InstancePaths::new(dir.path().join("vm"));
        paths
            .write_config(&test_config(dir.path()))
            .await
            .expect("write config");

        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("sleep pid");

        atomic_write(&paths.pid, pid.to_string().as_bytes())
            .await
            .expect("write pid");

        assert_eq!(
            paths.status().await.expect("status"),
            InstanceStatus::Stopped
        );

        let _ = child.kill().await;
    }

    #[tokio::test]
    async fn status_accepts_matching_qemu_process_identity() {
        let dir = tempdir().expect("tempdir");
        let paths = InstancePaths::new(dir.path().join("vm"));
        paths
            .write_config(&test_config(dir.path()))
            .await
            .expect("write config");

        let qmp_arg = format!("unix:{},server=on,wait=off", paths.qmp.display());
        let serial_arg = format!("file:{}", paths.serial.display());
        let mut child = Command::new("python3")
            .arg("-c")
            .arg("import time; time.sleep(30)")
            .arg("qemu-system-aarch64")
            .arg("-pidfile")
            .arg(&paths.pid)
            .arg("-qmp")
            .arg(&qmp_arg)
            .arg("-serial")
            .arg(&serial_arg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn python");
        let pid = child.id().expect("python pid");

        atomic_write(&paths.pid, pid.to_string().as_bytes())
            .await
            .expect("write pid");

        assert_eq!(
            paths.status().await.expect("status"),
            InstanceStatus::Running
        );

        let _ = child.kill().await;
    }
}
