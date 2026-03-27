use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::cli::CreateArgs;
use crate::instance::{InstanceManager, VmInfo};
use crate::ssh::ExecOutput;
use crate::state::{
    AccelMode, GuestArch, HardpassState, InstanceStatus, PortForward, validate_name,
};

pub struct Hardpass {
    manager: Arc<InstanceManager>,
}

impl Hardpass {
    pub async fn load() -> Result<Self> {
        let state = HardpassState::load().await?;
        Ok(Self::from_state(state))
    }

    pub async fn with_root(root: impl AsRef<Path>) -> Result<Self> {
        let state = HardpassState::load_with_root(root.as_ref().to_path_buf()).await?;
        Ok(Self::from_state(state))
    }

    pub async fn doctor(&self) -> Result<()> {
        self.manager.doctor().await
    }

    pub async fn create(&self, spec: VmSpec) -> Result<Vm> {
        let name = spec.name.clone();
        self.manager.create_silent(spec.into_create_args()).await?;
        self.vm(name)
    }

    pub fn vm(&self, name: impl Into<String>) -> Result<Vm> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Vm {
            manager: Arc::clone(&self.manager),
            name,
        })
    }

    fn from_state(state: HardpassState) -> Self {
        Self {
            manager: Arc::new(InstanceManager::new(state)),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct VmSpec {
    pub name: String,
    pub release: Option<String>,
    pub arch: Option<GuestArch>,
    pub accel: Option<AccelMode>,
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
    pub disk_gib: Option<u32>,
    pub ssh_key: Option<PathBuf>,
    pub forwards: Vec<PortForward>,
    pub timeout_secs: Option<u64>,
    pub cloud_init_user_data: Option<PathBuf>,
    pub cloud_init_network_config: Option<PathBuf>,
}

impl VmSpec {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    pub fn release(mut self, release: impl Into<String>) -> Self {
        self.release = Some(release.into());
        self
    }

    pub fn arch(mut self, arch: GuestArch) -> Self {
        self.arch = Some(arch);
        self
    }

    pub fn accel(mut self, accel: AccelMode) -> Self {
        self.accel = Some(accel);
        self
    }

    pub fn cpus(mut self, cpus: u8) -> Self {
        self.cpus = Some(cpus);
        self
    }

    pub fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = Some(memory_mib);
        self
    }

    pub fn disk_gib(mut self, disk_gib: u32) -> Self {
        self.disk_gib = Some(disk_gib);
        self
    }

    pub fn ssh_key(mut self, ssh_key: impl AsRef<Path>) -> Self {
        self.ssh_key = Some(ssh_key.as_ref().to_path_buf());
        self
    }

    pub fn forward(mut self, host: u16, guest: u16) -> Self {
        self.forwards.push(PortForward { host, guest });
        self
    }

    pub fn timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = Some(timeout_secs);
        self
    }

    pub fn cloud_init_user_data(mut self, path: impl AsRef<Path>) -> Self {
        self.cloud_init_user_data = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn cloud_init_network_config(mut self, path: impl AsRef<Path>) -> Self {
        self.cloud_init_network_config = Some(path.as_ref().to_path_buf());
        self
    }

    fn into_create_args(self) -> CreateArgs {
        CreateArgs {
            name: self.name,
            release: self.release,
            arch: self.arch,
            accel: self.accel,
            cpus: self.cpus,
            memory_mib: self.memory_mib,
            disk_gib: self.disk_gib,
            ssh_key: self.ssh_key.map(|path| path.display().to_string()),
            forwards: self
                .forwards
                .into_iter()
                .map(|forward| (forward.host, forward.guest))
                .collect(),
            timeout_secs: self.timeout_secs,
            cloud_init_user_data: self
                .cloud_init_user_data
                .map(|path| path.display().to_string()),
            cloud_init_network_config: self
                .cloud_init_network_config
                .map(|path| path.display().to_string()),
        }
    }
}

pub struct Vm {
    manager: Arc<InstanceManager>,
    name: String,
}

impl Vm {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn start(&self) -> Result<()> {
        self.manager.start_silent(&self.name).await?;
        Ok(())
    }

    pub async fn info(&self) -> Result<VmInfo> {
        self.manager.vm_info(&self.name).await
    }

    pub async fn status(&self) -> Result<InstanceStatus> {
        self.manager.status(&self.name).await
    }

    pub async fn wait_for_ssh(&self) -> Result<VmInfo> {
        self.manager.wait_for_ssh_ready(&self.name).await
    }

    pub async fn exec<I, S>(&self, command: I) -> Result<ExecOutput>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let command = command.into_iter().map(Into::into).collect::<Vec<_>>();
        self.manager.exec_capture(&self.name, &command).await
    }

    pub async fn exec_checked<I, S>(&self, command: I) -> Result<ExecOutput>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let command = command.into_iter().map(Into::into).collect::<Vec<_>>();
        self.manager.exec_checked(&self.name, &command).await
    }

    pub async fn stop(&self) -> Result<()> {
        self.manager.stop_silent(&self.name).await
    }

    pub async fn delete(&self) -> Result<()> {
        self.manager.delete_silent(&self.name).await
    }
}
