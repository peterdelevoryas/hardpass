use std::collections::BTreeSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::sleep;

use crate::cli::{CreateArgs, PrefetchImageArgs};
use crate::cloud_init::{create_seed_image, render_cloud_init};
use crate::images::ensure_image;
use crate::lock::lock_file;
use crate::ports::{reserve_ports, validate_forwards};
use crate::qemu::{discover_aarch64_firmware, launch_vm, system_powerdown};
use crate::ssh::{
    ExecOutput as SshExecOutput, ensure_ssh_key, exec as ssh_exec,
    exec_capture as ssh_exec_capture, exec_checked as ssh_exec_checked, open_session, wait_for_ssh,
};
use crate::ssh_config::{SshAliasEntry, SshConfigManager};
use crate::state::{
    AccelMode, CloudInitConfig, GuestArch, HardpassState, ImageConfig, InstanceConfig,
    InstancePaths, InstanceStatus, PortForward, SshConfig, command_exists, process_is_alive,
    validate_name,
};

#[derive(Clone)]
pub struct InstanceManager {
    state: HardpassState,
    client: reqwest::Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostDependency {
    QemuImg,
    QemuSystem,
    Ssh,
    SshKeygen,
    Aarch64Firmware,
}

impl HostDependency {
    fn label(self, guest_arch: GuestArch) -> String {
        match self {
            Self::QemuImg => "qemu-img".to_string(),
            Self::QemuSystem => guest_arch.qemu_binary().to_string(),
            Self::Ssh => "ssh".to_string(),
            Self::SshKeygen => "ssh-keygen".to_string(),
            Self::Aarch64Firmware => "aarch64-firmware".to_string(),
        }
    }

    fn is_qemu_related(self) -> bool {
        matches!(
            self,
            Self::QemuImg | Self::QemuSystem | Self::Aarch64Firmware
        )
    }
}

impl InstanceManager {
    pub fn new(state: HardpassState) -> Self {
        Self {
            state,
            client: reqwest::Client::builder()
                .user_agent(concat!("hardpass/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("reqwest client"),
        }
    }

    pub async fn doctor(&self) -> Result<()> {
        let host_arch = GuestArch::host_native()?;
        let required_tools = [
            "qemu-img".to_string(),
            host_arch.qemu_binary().to_string(),
            "ssh".to_string(),
            "ssh-keygen".to_string(),
        ];
        let mut missing = false;
        println!("Host architecture: {host_arch}");
        for tool in required_tools {
            if let Some(path) = resolve_command_path(&tool).await? {
                println!("ok    {tool:<20} {path}");
            } else {
                println!("fail  {tool:<20} not found");
                missing = true;
            }
        }

        if host_arch == GuestArch::Arm64 {
            match discover_aarch64_firmware() {
                Ok(firmware) => {
                    println!(
                        "ok    {:<20} code={} vars={}",
                        "aarch64-firmware",
                        firmware.code.display(),
                        firmware.vars_template.display()
                    );
                }
                Err(err) => {
                    println!("fail  {:<20} {err}", "aarch64-firmware");
                    missing = true;
                }
            }
        }

        if cfg!(target_os = "linux") && !Path::new("/dev/kvm").exists() {
            println!(
                "warn  {:<20} /dev/kvm unavailable; `hp start` with auto/kvm will fail, but `--accel tcg` remains available for slower emulated guests",
                "kvm"
            );
        }

        if missing {
            bail!("doctor found missing requirements");
        }
        Ok(())
    }

    pub async fn prefetch_image(&self, args: PrefetchImageArgs) -> Result<()> {
        let release = args
            .release
            .unwrap_or_else(|| InstanceConfig::default_release().to_string());
        let arch = args.arch.unwrap_or(GuestArch::host_native()?);
        let image = ensure_image(&self.client, &self.state.images_dir(), &release, arch).await?;
        let size_bytes = tokio::fs::metadata(&image.local_path).await?.len();
        println!("Prefetched Ubuntu {release} {arch} image");
        println!("path: {}", image.local_path.display());
        println!("sha256: {}", image.config.sha256);
        println!("size_bytes: {size_bytes}");
        Ok(())
    }

    pub async fn create(&self, args: CreateArgs) -> Result<()> {
        let info = self.create_with_output(args).await?;
        self.auto_configure_ssh_if_enabled().await;
        self.print_created(&info);
        Ok(())
    }

    pub async fn start(&self, name: &str) -> Result<()> {
        let info = self.start_inner(name, true).await?;
        self.print_ready(&info);
        Ok(())
    }

    pub async fn stop(&self, name: &str) -> Result<()> {
        let paths = self.state.instance_paths(name)?;
        let _lock = lock_file(paths.lock_path()).await?;
        self.stop_inner(name, true).await
    }

    pub async fn delete(&self, name: &str) -> Result<()> {
        let paths = self.state.instance_paths(name)?;
        let _lock = lock_file(paths.lock_path()).await?;
        self.delete_inner(name, true).await?;
        drop(_lock);
        self.auto_configure_ssh_if_enabled().await;
        Ok(())
    }

    pub async fn list(&self) -> Result<()> {
        let names = self.state.instance_names().await?;
        if names.is_empty() {
            println!("No Hardpass instances found");
            return Ok(());
        }
        let mut rows = Vec::new();
        for name in names {
            let paths = self.state.instance_paths(&name)?;
            if !paths.config.is_file() {
                continue;
            }
            let config = paths.read_config().await?;
            let status = paths.status().await?;
            rows.push(ListRow {
                name: config.name,
                status: status.to_string(),
                arch: config.arch.to_string(),
                release: config.release,
                ssh: format!("{}:{}", config.ssh.host, config.ssh.port),
            });
        }
        if rows.is_empty() {
            println!("No Hardpass instances found");
            return Ok(());
        }
        print!("{}", render_list_table(&rows));
        Ok(())
    }

    pub async fn info(&self, name: &str, json: bool) -> Result<()> {
        let output = self.vm_info(name).await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("name: {}", output.name);
            println!("status: {}", output.status);
            println!("release: {}", output.release);
            println!("arch: {}", output.arch);
            println!(
                "ssh: {}@{}:{}",
                output.ssh.user, output.ssh.host, output.ssh.port
            );
            println!("ssh alias: {}", output.ssh.alias);
            println!("instance_dir: {}", output.instance_dir.display());
            println!("serial_log: {}", output.serial_log.display());
            if output.forwards.is_empty() {
                println!("forwards: none");
            } else {
                let forwards = output
                    .forwards
                    .iter()
                    .map(|forward| format!("{}:{}", forward.host, forward.guest))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("forwards: {forwards}");
            }
        }
        Ok(())
    }

    pub async fn ssh(&self, name: &str, ssh_args: &[String]) -> Result<()> {
        let (_, config) = self.running_instance(name).await?;
        open_session(&config.ssh, ssh_args).await
    }

    pub async fn exec(&self, name: &str, command: &[String]) -> Result<()> {
        let (_, config) = self.running_instance(name).await?;
        ssh_exec(&config.ssh, command).await
    }

    pub(crate) async fn create_silent(&self, args: CreateArgs) -> Result<VmInfo> {
        self.create_inner(args, false).await
    }

    async fn create_with_output(&self, args: CreateArgs) -> Result<VmInfo> {
        self.create_inner(args, true).await
    }

    async fn create_inner(&self, args: CreateArgs, allow_prompt: bool) -> Result<VmInfo> {
        validate_name(&args.name)?;
        let paths = self.state.instance_paths(&args.name)?;
        let _lock = lock_file(paths.lock_path()).await?;
        let host_arch = GuestArch::host_native()?;
        let guest_arch = args.arch.unwrap_or(host_arch);
        let accel = args.accel.unwrap_or(AccelMode::Auto);
        validate_arch_accel_combo(host_arch, guest_arch, accel)?;
        match paths.status().await? {
            InstanceStatus::Missing => {
                self.ensure_create_dependencies(guest_arch, allow_prompt)
                    .await?;
                let config = self.create_instance(&paths, &args).await?;
                Ok(VmInfo::from_config(
                    &config,
                    &paths,
                    InstanceStatus::Stopped,
                ))
            }
            InstanceStatus::Stopped | InstanceStatus::Running => bail!(
                "instance {} already exists; use `hp start {}` or `hp delete {}`",
                args.name,
                args.name,
                args.name
            ),
        }
    }

    pub(crate) async fn start_silent(&self, name: &str) -> Result<VmInfo> {
        self.start_inner(name, false).await
    }

    pub(crate) async fn stop_silent(&self, name: &str) -> Result<()> {
        let paths = self.state.instance_paths(name)?;
        let _lock = lock_file(paths.lock_path()).await?;
        self.stop_inner(name, false).await
    }

    pub(crate) async fn delete_silent(&self, name: &str) -> Result<()> {
        let paths = self.state.instance_paths(name)?;
        let _lock = lock_file(paths.lock_path()).await?;
        self.delete_inner(name, false).await
    }

    pub(crate) async fn wait_for_ssh_ready(&self, name: &str) -> Result<VmInfo> {
        let (paths, config) = self.running_instance(name).await?;
        self.ensure_start_dependencies(config.arch, false, false)
            .await?;
        wait_for_ssh(&config.ssh, config.timeout_secs).await?;
        Ok(VmInfo::from_config(&config, &paths, paths.status().await?))
    }

    pub(crate) async fn vm_info(&self, name: &str) -> Result<VmInfo> {
        let (paths, config) = self.instance(name).await?;
        Ok(VmInfo::from_config(&config, &paths, paths.status().await?))
    }

    pub(crate) async fn status(&self, name: &str) -> Result<InstanceStatus> {
        let paths = self.state.instance_paths(name)?;
        paths.status().await
    }

    pub(crate) async fn exec_capture(
        &self,
        name: &str,
        command: &[String],
    ) -> Result<SshExecOutput> {
        let (_, config) = self.running_instance(name).await?;
        self.ensure_start_dependencies(config.arch, false, false)
            .await?;
        ssh_exec_capture(&config.ssh, command).await
    }

    pub(crate) async fn exec_checked(
        &self,
        name: &str,
        command: &[String],
    ) -> Result<SshExecOutput> {
        let (_, config) = self.running_instance(name).await?;
        self.ensure_start_dependencies(config.arch, false, false)
            .await?;
        ssh_exec_checked(&config.ssh, command).await
    }

    async fn create_instance(
        &self,
        paths: &InstancePaths,
        args: &CreateArgs,
    ) -> Result<InstanceConfig> {
        let host_arch = GuestArch::host_native()?;
        let arch = args.arch.unwrap_or(host_arch);
        let accel = args.accel.unwrap_or(AccelMode::Auto);
        validate_arch_accel_combo(host_arch, arch, accel)?;
        let ssh_key_path = self.resolve_ssh_key_path(args.ssh_key.as_deref())?;
        let public_key = ensure_ssh_key(&ssh_key_path).await?;
        let user_data_path = args
            .cloud_init_user_data
            .as_deref()
            .map(expand_path)
            .transpose()?;
        let network_config_path = args
            .cloud_init_network_config
            .as_deref()
            .map(expand_path)
            .transpose()?;
        let render = render_cloud_init(
            &args.name,
            &public_key,
            user_data_path.as_deref(),
            network_config_path.as_deref(),
        )
        .await?;

        let forwards = args
            .forwards
            .iter()
            .copied()
            .map(|(host, guest)| PortForward { host, guest })
            .collect::<Vec<_>>();

        let release = args
            .release
            .clone()
            .unwrap_or_else(|| InstanceConfig::default_release().to_string());
        let image = ensure_image(&self.client, &self.state.images_dir(), &release, arch).await?;

        let port_reservation = self.reserve_host_ports(&forwards).await?;
        let ssh_port = port_reservation.ssh_port;
        validate_forwards(&forwards, ssh_port)?;
        let config = InstanceConfig {
            name: args.name.clone(),
            release,
            arch,
            accel,
            cpus: args.cpus.unwrap_or_else(InstanceConfig::default_cpus),
            memory_mib: args
                .memory_mib
                .unwrap_or_else(InstanceConfig::default_memory_mib),
            disk_gib: args
                .disk_gib
                .unwrap_or_else(InstanceConfig::default_disk_gib),
            timeout_secs: args
                .timeout_secs
                .unwrap_or_else(InstanceConfig::default_timeout_secs),
            ssh: SshConfig {
                user: InstanceConfig::default_ssh_user().to_string(),
                host: InstanceConfig::default_ssh_host().to_string(),
                port: ssh_port,
                identity_file: ssh_key_path,
            },
            forwards,
            image: ImageConfig {
                sha256: image.config.sha256.clone(),
                ..image.config
            },
            cloud_init: CloudInitConfig {
                user_data_sha256: render.user_data_sha256.clone(),
                network_config_sha256: render.network_config_sha256.clone(),
            },
        };

        paths.ensure_dir().await?;
        crate::qemu::create_overlay_disk(&image.local_path, &paths.disk, config.disk_gib).await?;
        create_seed_image(&paths.seed, &render).await?;
        paths.write_config(&config).await?;
        Ok(config)
    }

    async fn start_inner(&self, name: &str, show_serial: bool) -> Result<VmInfo> {
        let paths = self.state.instance_paths(name)?;
        let _lock = lock_file(paths.lock_path()).await?;
        self.start_locked(&paths, name, show_serial).await
    }

    async fn start_locked(
        &self,
        paths: &InstancePaths,
        name: &str,
        show_serial: bool,
    ) -> Result<VmInfo> {
        match paths.status().await? {
            InstanceStatus::Missing => {
                bail!("instance {name} does not exist; use `hp create {name}` first")
            }
            InstanceStatus::Stopped => {
                let config = paths.read_config().await?;
                self.ensure_start_dependencies(config.arch, true, show_serial)
                    .await?;
                self.ensure_existing_artifacts(paths).await?;
                paths.clear_runtime_artifacts().await?;
                launch_vm(&config, paths).await?;
                let _ = self.wait_for_pid(paths).await?;
                if show_serial {
                    self.wait_for_ssh_with_serial(&config, paths).await?;
                } else {
                    wait_for_ssh(&config.ssh, config.timeout_secs).await?;
                }
                Ok(VmInfo::from_config(&config, paths, paths.status().await?))
            }
            InstanceStatus::Running => {
                let config = paths.read_config().await?;
                self.ensure_start_dependencies(config.arch, false, show_serial)
                    .await?;
                wait_for_ssh(&config.ssh, config.timeout_secs).await?;
                Ok(VmInfo::from_config(&config, paths, paths.status().await?))
            }
        }
    }

    async fn stop_inner(&self, name: &str, report: bool) -> Result<()> {
        let paths = self.state.instance_paths(name)?;
        match paths.status().await? {
            InstanceStatus::Missing => bail!("instance {name} does not exist"),
            InstanceStatus::Stopped => {
                paths.clear_runtime_artifacts().await?;
                if report {
                    println!("{name} is already stopped");
                }
                Ok(())
            }
            InstanceStatus::Running => {
                let pid = paths
                    .read_pid()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("missing pid file"))?;
                if paths.qmp.is_file() {
                    let _ = system_powerdown(&paths.qmp).await;
                } else {
                    send_signal(pid, Signal::SIGTERM)?;
                }
                if !wait_for_process_exit(pid, Duration::from_secs(20)).await {
                    let _ = send_signal(pid, Signal::SIGTERM);
                    if !wait_for_process_exit(pid, Duration::from_secs(5)).await {
                        send_signal(pid, Signal::SIGKILL)?;
                        let _ = wait_for_process_exit(pid, Duration::from_secs(2)).await;
                    }
                }
                paths.clear_runtime_artifacts().await?;
                if report {
                    println!("Stopped {name}");
                }
                Ok(())
            }
        }
    }

    async fn delete_inner(&self, name: &str, report: bool) -> Result<()> {
        let paths = self.state.instance_paths(name)?;
        if matches!(paths.status().await?, InstanceStatus::Running) {
            self.stop_inner(name, report).await?;
        }
        if !paths.dir.exists() {
            if report {
                println!("Instance {name} does not exist");
            }
            return Ok(());
        }
        paths.remove_all().await?;
        if report {
            println!("Deleted {name}");
        }
        Ok(())
    }

    async fn ensure_existing_artifacts(&self, paths: &InstancePaths) -> Result<()> {
        if !paths.disk.is_file() {
            bail!(
                "missing VM disk at {}; delete and recreate",
                paths.disk.display()
            );
        }
        if !paths.seed.is_file() {
            bail!(
                "missing cloud-init seed image at {}; delete and recreate",
                paths.seed.display()
            );
        }
        Ok(())
    }

    async fn ensure_create_dependencies(
        &self,
        guest_arch: GuestArch,
        allow_prompt: bool,
    ) -> Result<()> {
        let mut missing = self.collect_create_missing_dependencies(guest_arch).await;
        if self
            .maybe_offer_brew_install(guest_arch, &missing, allow_prompt)
            .await?
        {
            missing = self.collect_create_missing_dependencies(guest_arch).await;
        }
        ensure_host_dependencies(guest_arch, &missing)
    }

    async fn ensure_start_dependencies(
        &self,
        guest_arch: GuestArch,
        needs_launch: bool,
        allow_prompt: bool,
    ) -> Result<()> {
        let mut missing = self
            .collect_start_missing_dependencies(guest_arch, needs_launch)
            .await;
        if self
            .maybe_offer_brew_install(guest_arch, &missing, allow_prompt)
            .await?
        {
            missing = self
                .collect_start_missing_dependencies(guest_arch, needs_launch)
                .await;
        }
        ensure_host_dependencies(guest_arch, &missing)
    }

    async fn collect_create_missing_dependencies(
        &self,
        guest_arch: GuestArch,
    ) -> Vec<HostDependency> {
        let mut missing = Vec::new();
        if !command_exists("qemu-img").await {
            missing.push(HostDependency::QemuImg);
        }
        if !command_exists(guest_arch.qemu_binary()).await {
            missing.push(HostDependency::QemuSystem);
        }
        if !command_exists("ssh-keygen").await {
            missing.push(HostDependency::SshKeygen);
        }
        if guest_arch == GuestArch::Arm64 && discover_aarch64_firmware().is_err() {
            missing.push(HostDependency::Aarch64Firmware);
        }
        missing
    }

    async fn collect_start_missing_dependencies(
        &self,
        guest_arch: GuestArch,
        needs_launch: bool,
    ) -> Vec<HostDependency> {
        let mut missing = Vec::new();
        if needs_launch && !command_exists(guest_arch.qemu_binary()).await {
            missing.push(HostDependency::QemuSystem);
        }
        if !command_exists("ssh").await {
            missing.push(HostDependency::Ssh);
        }
        if needs_launch && guest_arch == GuestArch::Arm64 && discover_aarch64_firmware().is_err() {
            missing.push(HostDependency::Aarch64Firmware);
        }
        missing
    }

    async fn maybe_offer_brew_install(
        &self,
        guest_arch: GuestArch,
        missing: &[HostDependency],
        allow_prompt: bool,
    ) -> Result<bool> {
        if !allow_prompt {
            return Ok(false);
        }
        if !should_offer_brew_install(
            std::env::consts::OS,
            missing,
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
            command_exists("brew").await,
        ) {
            return Ok(false);
        }

        let prompt = brew_install_prompt(guest_arch, missing);
        if !prompt_yes_no(&prompt).await? {
            return Ok(false);
        }

        let status = Command::new("brew")
            .arg("install")
            .arg("qemu")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .context("run brew install qemu")?;
        if status.success() {
            Ok(true)
        } else {
            bail!("`brew install qemu` failed with status {status}")
        }
    }

    async fn instance(&self, name: &str) -> Result<(InstancePaths, InstanceConfig)> {
        let paths = self.state.instance_paths(name)?;
        if !paths.config.is_file() {
            bail!("instance {name} does not exist");
        }
        let config = paths.read_config().await?;
        Ok((paths, config))
    }

    async fn running_instance(&self, name: &str) -> Result<(InstancePaths, InstanceConfig)> {
        let (paths, config) = self.instance(name).await?;
        if !matches!(paths.status().await?, InstanceStatus::Running) {
            bail!("instance {name} is not running; use `hp start {name}` first");
        }
        Ok((paths, config))
    }

    async fn reserve_host_ports(
        &self,
        forwards: &[PortForward],
    ) -> Result<crate::ports::PortReservation> {
        let _lock = lock_file(self.state.ports_lock_path()).await?;
        let occupied = self.collect_reserved_host_ports().await?;
        reserve_ports(forwards, &occupied).await
    }

    async fn collect_reserved_host_ports(&self) -> Result<BTreeSet<u16>> {
        let mut occupied = BTreeSet::new();
        for name in self.state.instance_names().await? {
            let paths = self.state.instance_paths(&name)?;
            if !paths.config.is_file() {
                continue;
            }
            let Ok(config) = paths.read_config().await else {
                continue;
            };
            occupied.insert(config.ssh.port);
            occupied.extend(config.forwards.into_iter().map(|forward| forward.host));
        }
        Ok(occupied)
    }

    fn resolve_ssh_key_path(&self, path: Option<&str>) -> Result<PathBuf> {
        match path {
            Some(path) => expand_path(path),
            None => Ok(self.state.default_ssh_key_path()),
        }
    }

    async fn wait_for_pid(&self, paths: &InstancePaths) -> Result<u32> {
        for _ in 0..50 {
            if let Some(pid) = paths.read_pid().await? {
                return Ok(pid);
            }
            sleep(Duration::from_millis(100)).await;
        }
        bail!("QEMU did not write a pid file")
    }

    fn print_created(&self, info: &VmInfo) {
        for line in created_lines(info) {
            println!("{line}");
        }
    }

    fn print_ready(&self, info: &VmInfo) {
        for line in ready_lines(info) {
            println!("{line}");
        }
    }

    async fn wait_for_ssh_with_serial(
        &self,
        config: &InstanceConfig,
        paths: &InstancePaths,
    ) -> Result<()> {
        println!("{}", booting_message(&config.name));
        let (stop_tx, stop_rx) = watch::channel(false);
        let serial_path = paths.serial.clone();
        let tail_task = tokio::spawn(async move { tail_serial_log(serial_path, stop_rx).await });
        let wait_result = wait_for_ssh(&config.ssh, config.timeout_secs).await;
        let _ = stop_tx.send(true);
        let tail_state = tail_task.await.unwrap_or_default();
        if tail_state.printed_any && !tail_state.ended_with_newline {
            println!();
        }
        wait_result
    }

    async fn collect_ssh_alias_entries(&self) -> Result<Vec<SshAliasEntry>> {
        let mut entries = Vec::new();
        for name in self.state.instance_names().await? {
            let paths = self.state.instance_paths(&name)?;
            if !paths.config.is_file() {
                continue;
            }
            let config = paths.read_config().await?;
            entries.push(SshAliasEntry {
                alias: config.name.clone(),
                host: config.ssh.host.clone(),
                port: config.ssh.port,
                user: config.ssh.user.clone(),
                identity_file: config.ssh.identity_file.clone(),
            });
        }
        Ok(entries)
    }

    pub(crate) async fn auto_configure_ssh_if_enabled(&self) {
        if !self.state.should_auto_sync_ssh_config() {
            return;
        }
        if let Err(err) = self.configure_ssh_if_enabled().await {
            eprintln!("warning: failed to update Hardpass SSH config: {err:#}");
        }
    }

    async fn configure_ssh_if_enabled(&self) -> Result<()> {
        let _lock = lock_file(self.state.ssh_config_lock_path()).await?;
        let manager = SshConfigManager::from_home_dir()?;
        let entries = self.collect_ssh_alias_entries().await?;
        manager.install().await?;
        manager.sync(&entries).await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VmInfo {
    pub name: String,
    pub status: InstanceStatus,
    pub release: String,
    pub arch: GuestArch,
    pub accel: AccelMode,
    pub cpus: u8,
    pub memory_mib: u32,
    pub disk_gib: u32,
    pub instance_dir: PathBuf,
    pub serial_log: PathBuf,
    pub ssh: VmSshInfo,
    pub forwards: Vec<PortForward>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VmSshInfo {
    pub alias: String,
    pub user: String,
    pub host: String,
    pub port: u16,
    pub identity_file: PathBuf,
}

impl VmInfo {
    fn from_config(config: &InstanceConfig, paths: &InstancePaths, status: InstanceStatus) -> Self {
        Self {
            name: config.name.clone(),
            status,
            release: config.release.clone(),
            arch: config.arch,
            accel: config.accel,
            cpus: config.cpus,
            memory_mib: config.memory_mib,
            disk_gib: config.disk_gib,
            instance_dir: paths.dir.clone(),
            serial_log: paths.serial.clone(),
            ssh: VmSshInfo {
                alias: config.name.clone(),
                user: config.ssh.user.clone(),
                host: config.ssh.host.clone(),
                port: config.ssh.port,
                identity_file: config.ssh.identity_file.clone(),
            },
            forwards: config.forwards.clone(),
        }
    }
}

#[derive(Debug, Default)]
struct SerialTailState {
    printed_any: bool,
    ended_with_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListRow {
    name: String,
    status: String,
    arch: String,
    release: String,
    ssh: String,
}

fn booting_message(name: &str) -> String {
    format!("Booting {name}; waiting for SSH...")
}

fn created_lines(info: &VmInfo) -> [String; 3] {
    [
        format!("Created {}", info.name),
        format!("start: hp start {}", info.name),
        format!("serial log: {}", info.serial_log.display()),
    ]
}

fn ready_lines(info: &VmInfo) -> [String; 3] {
    [
        format!("{} is ready", info.name),
        format!("ssh: hp ssh {}", info.name),
        format!("serial log: {}", info.serial_log.display()),
    ]
}

fn render_list_table(rows: &[ListRow]) -> String {
    let name_width = "NAME"
        .len()
        .max(rows.iter().map(|row| row.name.len()).max().unwrap_or(0));
    let status_width = "STATUS"
        .len()
        .max(rows.iter().map(|row| row.status.len()).max().unwrap_or(0));
    let arch_width = "ARCH"
        .len()
        .max(rows.iter().map(|row| row.arch.len()).max().unwrap_or(0));
    let release_width = "RELEASE"
        .len()
        .max(rows.iter().map(|row| row.release.len()).max().unwrap_or(0));

    let mut output = String::new();
    output.push_str(&format!(
        "{:<name_width$}  {:<status_width$}  {:<arch_width$}  {:<release_width$}  SSH\n",
        "NAME", "STATUS", "ARCH", "RELEASE",
    ));
    for row in rows {
        output.push_str(&format!(
            "{:<name_width$}  {:<status_width$}  {:<arch_width$}  {:<release_width$}  {}\n",
            row.name, row.status, row.arch, row.release, row.ssh,
        ));
    }
    output
}

fn validate_arch_accel_combo(
    host_arch: GuestArch,
    guest_arch: GuestArch,
    accel: AccelMode,
) -> Result<()> {
    if guest_arch == host_arch || accel == AccelMode::Tcg {
        return Ok(());
    }

    if accel == AccelMode::Auto {
        bail!(
            "guest architecture {guest_arch} does not match host architecture {host_arch}; use `--accel tcg` for cross-architecture emulation"
        );
    }

    bail!(
        "acceleration mode {accel} only supports host-native guests ({host_arch}); use `--accel tcg` for {guest_arch} emulation"
    )
}

fn ensure_host_dependencies(guest_arch: GuestArch, missing: &[HostDependency]) -> Result<()> {
    if missing.is_empty() {
        return Ok(());
    }
    bail!("{}", missing_dependency_message(guest_arch, missing))
}

fn missing_dependency_message(guest_arch: GuestArch, missing: &[HostDependency]) -> String {
    missing_dependency_message_for_os(guest_arch, missing, std::env::consts::OS)
}

fn missing_dependency_message_for_os(
    guest_arch: GuestArch,
    missing: &[HostDependency],
    os: &str,
) -> String {
    let labels = missing
        .iter()
        .map(|dependency| dependency.label(guest_arch))
        .collect::<Vec<_>>()
        .join(", ");
    if missing
        .iter()
        .all(|dependency| dependency.is_qemu_related())
    {
        let install_hint = if os == "macos" {
            "install QEMU with `brew install qemu`"
        } else {
            "install QEMU"
        };
        format!(
            "QEMU is not installed or incomplete (missing {labels}); {install_hint} and run `hp doctor` for details"
        )
    } else {
        format!("missing required host dependencies: {labels}; run `hp doctor` for details")
    }
}

fn should_offer_brew_install(
    os: &str,
    missing: &[HostDependency],
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
    brew_available: bool,
) -> bool {
    os == "macos"
        && stdin_is_terminal
        && stdout_is_terminal
        && brew_available
        && missing
            .iter()
            .any(|dependency| dependency.is_qemu_related())
}

fn brew_install_prompt(guest_arch: GuestArch, missing: &[HostDependency]) -> String {
    let labels = missing
        .iter()
        .filter(|dependency| dependency.is_qemu_related())
        .map(|dependency| dependency.label(guest_arch))
        .collect::<Vec<_>>()
        .join(", ");
    format!("QEMU is missing ({labels}). Run `brew install qemu` now? [y/N]: ")
}

#[cfg(test)]
fn ensure_match<T>(field: &str, expected: &T, actual: &T) -> Result<()>
where
    T: std::fmt::Debug + PartialEq,
{
    if expected == actual {
        Ok(())
    } else {
        bail!(
            "instance configuration mismatch for {field}: existing={expected:?}, requested={actual:?}. Delete and recreate the VM to change it."
        )
    }
}

async fn resolve_command_path(name: &str) -> Result<Option<String>> {
    if !command_exists(name).await {
        return Ok(None);
    }
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .await
        .with_context(|| format!("resolve {name}"))?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
}

async fn prompt_yes_no(prompt: &str) -> Result<bool> {
    let prompt = prompt.to_string();
    tokio::task::spawn_blocking(move || -> Result<bool> {
        let mut stdout = std::io::stdout();
        stdout.write_all(prompt.as_bytes())?;
        stdout.flush()?;

        let mut response = String::new();
        std::io::stdin().read_line(&mut response)?;
        let trimmed = response.trim();
        Ok(trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes"))
    })
    .await?
}

fn expand_path(path: &str) -> Result<PathBuf> {
    let expanded = if path == "~" {
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("unable to determine home directory"))?
    } else if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("unable to determine home directory"))?
            .join(rest)
    } else {
        PathBuf::from(path)
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(std::env::current_dir()?.join(expanded))
    }
}

fn send_signal(pid: u32, signal: Signal) -> Result<()> {
    nix::sys::signal::kill(Pid::from_raw(pid as i32), Some(signal))
        .with_context(|| format!("send {signal:?} to pid {pid}"))?;
    Ok(())
}

async fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if !process_is_alive(pid) {
            return true;
        }
        sleep(Duration::from_millis(250)).await;
    }
    !process_is_alive(pid)
}

async fn tail_serial_log(path: PathBuf, mut stop_rx: watch::Receiver<bool>) -> SerialTailState {
    let mut offset = 0u64;
    let mut state = SerialTailState::default();
    let mut stdout = tokio::io::stdout();
    loop {
        read_serial_delta(&path, &mut offset, &mut stdout, &mut state).await;
        if *stop_rx.borrow() {
            break;
        }
        tokio::select! {
            _ = stop_rx.changed() => {}
            _ = sleep(Duration::from_millis(200)) => {}
        }
    }
    read_serial_delta(&path, &mut offset, &mut stdout, &mut state).await;
    state
}

async fn read_serial_delta(
    path: &Path,
    offset: &mut u64,
    stdout: &mut tokio::io::Stdout,
    state: &mut SerialTailState,
) {
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return;
    };
    if file.seek(std::io::SeekFrom::Start(*offset)).await.is_err() {
        return;
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).await.is_err() || buf.is_empty() {
        return;
    }
    let _ = stdout.write_all(&buf).await;
    let _ = stdout.flush().await;
    *offset += buf.len() as u64;
    state.printed_any = true;
    state.ended_with_newline = buf.last() == Some(&b'\n');
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{
        HostDependency, ListRow, VmInfo, booting_message, brew_install_prompt, created_lines,
        ensure_match, expand_path, missing_dependency_message, missing_dependency_message_for_os,
        ready_lines, render_list_table, should_offer_brew_install, validate_arch_accel_combo,
    };
    use crate::state::{
        AccelMode, CloudInitConfig, GuestArch, ImageConfig, InstanceConfig, InstancePaths,
        InstanceStatus, PortForward, SshConfig,
    };

    #[test]
    fn ensure_match_reports_differences() {
        let err = ensure_match("cpus", &2u8, &4u8).expect_err("should fail");
        assert!(err.to_string().contains("configuration mismatch"));
    }

    #[test]
    fn booting_message_is_concise() {
        assert_eq!(
            booting_message("neuromancer"),
            "Booting neuromancer; waiting for SSH..."
        );
    }

    #[test]
    fn expand_relative_paths() {
        let current = std::env::current_dir().expect("cwd");
        let path = expand_path("relative/file").expect("expand");
        assert!(path.starts_with(current));
    }

    #[test]
    fn info_output_captures_paths() {
        let dir = tempdir().expect("tempdir");
        let paths = InstancePaths::new(dir.path().join("vm"));
        let config = InstanceConfig {
            name: "vm".into(),
            release: "24.04".into(),
            arch: GuestArch::Arm64,
            accel: AccelMode::Auto,
            cpus: 4,
            memory_mib: 4096,
            disk_gib: 24,
            timeout_secs: 180,
            ssh: SshConfig {
                user: "ubuntu".into(),
                host: "127.0.0.1".into(),
                port: 2222,
                identity_file: dir.path().join("id_ed25519"),
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
                network_config_sha256: Some("def".into()),
            },
        };
        let output = VmInfo::from_config(&config, &paths, InstanceStatus::Stopped);
        assert_eq!(output.name, "vm");
        assert_eq!(output.status, InstanceStatus::Stopped);
        assert_eq!(output.ssh.port, 2222);
    }

    #[test]
    fn created_lines_use_start_hint() {
        let dir = tempdir().expect("tempdir");
        let paths = InstancePaths::new(dir.path().join("neuromancer"));
        let config = InstanceConfig {
            name: "neuromancer".into(),
            release: "24.04".into(),
            arch: GuestArch::Arm64,
            accel: AccelMode::Auto,
            cpus: 4,
            memory_mib: 4096,
            disk_gib: 24,
            timeout_secs: 180,
            ssh: SshConfig {
                user: "ubuntu".into(),
                host: "127.0.0.1".into(),
                port: 49702,
                identity_file: dir.path().join("id_ed25519"),
            },
            forwards: vec![],
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
        };
        let info = VmInfo::from_config(&config, &paths, InstanceStatus::Stopped);
        let lines = created_lines(&info);
        assert_eq!(lines[0], "Created neuromancer");
        assert_eq!(lines[1], "start: hp start neuromancer");
        assert!(lines[2].contains("serial log:"));
    }

    #[test]
    fn ready_lines_use_hardpass_ssh_hint() {
        let dir = tempdir().expect("tempdir");
        let paths = InstancePaths::new(dir.path().join("neuromancer"));
        let config = InstanceConfig {
            name: "neuromancer".into(),
            release: "24.04".into(),
            arch: GuestArch::Arm64,
            accel: AccelMode::Auto,
            cpus: 4,
            memory_mib: 4096,
            disk_gib: 24,
            timeout_secs: 180,
            ssh: SshConfig {
                user: "ubuntu".into(),
                host: "127.0.0.1".into(),
                port: 49702,
                identity_file: dir.path().join("id_ed25519"),
            },
            forwards: vec![],
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
        };
        let info = VmInfo::from_config(&config, &paths, InstanceStatus::Running);
        let lines = ready_lines(&info);
        assert_eq!(lines[0], "neuromancer is ready");
        assert_eq!(lines[1], "ssh: hp ssh neuromancer");
        assert!(lines[2].contains("serial log:"));
    }

    #[test]
    fn qemu_only_missing_dependencies_have_install_hint() {
        let message = missing_dependency_message_for_os(
            GuestArch::Arm64,
            &[HostDependency::QemuSystem, HostDependency::Aarch64Firmware],
            "macos",
        );
        assert!(message.contains("QEMU is not installed or incomplete"));
        assert!(message.contains("qemu-system-aarch64"));
        assert!(message.contains("aarch64-firmware"));
        assert!(message.contains("brew install qemu"));
        assert!(message.contains("hp doctor"));
    }

    #[test]
    fn mixed_missing_dependencies_use_generic_message() {
        let message = missing_dependency_message(
            GuestArch::Amd64,
            &[HostDependency::QemuImg, HostDependency::SshKeygen],
        );
        assert!(message.contains("missing required host dependencies"));
        assert!(message.contains("qemu-img"));
        assert!(message.contains("ssh-keygen"));
        assert!(message.contains("hp doctor"));
    }

    #[test]
    fn linux_qemu_hint_stays_generic() {
        let message = missing_dependency_message_for_os(
            GuestArch::Amd64,
            &[HostDependency::QemuImg, HostDependency::QemuSystem],
            "linux",
        );
        assert!(message.contains("install QEMU"));
        assert!(!message.contains("brew install qemu"));
    }

    #[test]
    fn cross_arch_requires_tcg_for_auto() {
        let err = validate_arch_accel_combo(GuestArch::Arm64, GuestArch::Amd64, AccelMode::Auto)
            .expect_err("should fail");
        assert!(err.to_string().contains("--accel tcg"));
        assert!(err.to_string().contains("cross-architecture emulation"));
    }

    #[test]
    fn cross_arch_requires_tcg_for_non_tcg_explicit_accel() {
        let err = validate_arch_accel_combo(GuestArch::Arm64, GuestArch::Amd64, AccelMode::Hvf)
            .expect_err("should fail");
        assert!(err.to_string().contains("host-native guests"));
        assert!(err.to_string().contains("--accel tcg"));
    }

    #[test]
    fn cross_arch_is_allowed_with_tcg() {
        validate_arch_accel_combo(GuestArch::Arm64, GuestArch::Amd64, AccelMode::Tcg)
            .expect("tcg should allow cross-arch guests");
    }

    #[test]
    fn brew_offer_only_happens_on_interactive_macos_with_brew() {
        assert!(should_offer_brew_install(
            "macos",
            &[HostDependency::QemuImg],
            true,
            true,
            true,
        ));
        assert!(!should_offer_brew_install(
            "linux",
            &[HostDependency::QemuImg],
            true,
            true,
            true,
        ));
        assert!(!should_offer_brew_install(
            "macos",
            &[HostDependency::QemuImg],
            false,
            true,
            true,
        ));
        assert!(!should_offer_brew_install(
            "macos",
            &[HostDependency::QemuImg],
            true,
            true,
            false,
        ));
        assert!(!should_offer_brew_install(
            "macos",
            &[HostDependency::Ssh],
            true,
            true,
            true,
        ));
    }

    #[test]
    fn brew_prompt_lists_qemu_missing_bits_only() {
        let prompt = brew_install_prompt(
            GuestArch::Arm64,
            &[
                HostDependency::QemuImg,
                HostDependency::Ssh,
                HostDependency::Aarch64Firmware,
            ],
        );
        assert!(prompt.contains("qemu-img"));
        assert!(prompt.contains("aarch64-firmware"));
        assert!(!prompt.contains("ssh"));
        assert!(prompt.contains("brew install qemu"));
    }

    #[test]
    fn list_table_aligns_columns_with_spaces() {
        let output = render_list_table(&[
            ListRow {
                name: "neuromancer".into(),
                status: "running".into(),
                arch: "arm64".into(),
                release: "24.04".into(),
                ssh: "127.0.0.1:63320".into(),
            },
            ListRow {
                name: "vm".into(),
                status: "stopped".into(),
                arch: "amd64".into(),
                release: "24.04".into(),
                ssh: "127.0.0.1:40222".into(),
            },
        ]);
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "NAME         STATUS   ARCH   RELEASE  SSH");
        assert_eq!(
            lines[1],
            "neuromancer  running  arm64  24.04    127.0.0.1:63320"
        );
        assert_eq!(
            lines[2],
            "vm           stopped  amd64  24.04    127.0.0.1:40222"
        );
    }
}
