use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

use crate::state::{AccelMode, GuestArch, InstanceConfig, InstancePaths, PortForward};

#[derive(Debug, Clone)]
pub struct FirmwarePaths {
    pub code: PathBuf,
    pub vars_template: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
}

fn launch_accel_attempts(configured: AccelMode) -> Result<Vec<AccelMode>> {
    Ok(vec![resolve_accel(configured)?])
}

pub fn resolve_accel(configured: AccelMode) -> Result<AccelMode> {
    resolve_accel_for_env(configured, std::env::consts::OS, linux_kvm_available())
}

fn resolve_accel_for_env(
    configured: AccelMode,
    os: &str,
    linux_kvm_available: bool,
) -> Result<AccelMode> {
    match configured {
        AccelMode::Auto => {
            if os == "macos" {
                Ok(AccelMode::Hvf)
            } else if os == "linux" && linux_kvm_available {
                Ok(AccelMode::Kvm)
            } else if os == "linux" {
                bail!("{}", missing_kvm_error_message())
            } else {
                bail!(
                    "automatic acceleration is only supported on macOS (HVF) and Linux with /dev/kvm (KVM)"
                )
            }
        }
        AccelMode::Hvf if os == "macos" => Ok(AccelMode::Hvf),
        AccelMode::Kvm if os == "linux" && linux_kvm_available => Ok(AccelMode::Kvm),
        AccelMode::Kvm if os == "linux" => bail!("{}", missing_kvm_error_message()),
        AccelMode::Tcg => bail!("{}", tcg_disabled_error_message()),
        other => bail!("acceleration mode {other} is not supported on this host"),
    }
}

fn linux_kvm_available() -> bool {
    Path::new("/dev/kvm").exists()
}

fn missing_kvm_error_message() -> &'static str {
    "Hardpass requires KVM acceleration on Linux, but /dev/kvm is unavailable on this host. Hardpass will not fall back to TCG; run on a KVM-enabled Linux host/runner or choose a different supported accel mode."
}

fn tcg_disabled_error_message() -> &'static str {
    "TCG acceleration is disabled in Hardpass. Use KVM on Linux or HVF on macOS instead."
}

pub fn discover_aarch64_firmware() -> Result<FirmwarePaths> {
    let candidates = [
        (
            "/opt/homebrew/share/qemu/edk2-aarch64-code.fd",
            "/opt/homebrew/share/qemu/edk2-arm-vars.fd",
        ),
        (
            "/usr/local/share/qemu/edk2-aarch64-code.fd",
            "/usr/local/share/qemu/edk2-arm-vars.fd",
        ),
        (
            "/usr/share/AAVMF/AAVMF_CODE.fd",
            "/usr/share/AAVMF/AAVMF_VARS.fd",
        ),
        (
            "/usr/share/qemu/edk2-aarch64-code.fd",
            "/usr/share/qemu/edk2-arm-vars.fd",
        ),
    ];
    for (code, vars) in candidates {
        if Path::new(code).is_file() && Path::new(vars).is_file() {
            return Ok(FirmwarePaths {
                code: PathBuf::from(code),
                vars_template: PathBuf::from(vars),
            });
        }
    }
    bail!("unable to find aarch64 UEFI firmware; install QEMU/EDK2 firmware")
}

pub async fn create_overlay_disk(base_image: &Path, disk_path: &Path, disk_gib: u32) -> Result<()> {
    if disk_path.is_file() {
        return Ok(());
    }
    if let Some(parent) = disk_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let status = Command::new("qemu-img")
        .arg("create")
        .arg("-f")
        .arg("qcow2")
        .arg("-F")
        .arg("qcow2")
        .arg("-b")
        .arg(base_image)
        .arg(disk_path)
        .arg(format!("{disk_gib}G"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .await
        .context("run qemu-img create")?;
    if status.success() {
        Ok(())
    } else {
        bail!("qemu-img create failed with status {status}")
    }
}

#[allow(dead_code)]
pub fn build_launch_spec(config: &InstanceConfig, paths: &InstancePaths) -> Result<LaunchSpec> {
    build_launch_spec_with_accel(config, paths, resolve_accel(config.accel)?)
}

fn build_launch_spec_with_accel(
    config: &InstanceConfig,
    paths: &InstancePaths,
    resolved_accel: AccelMode,
) -> Result<LaunchSpec> {
    let mut args = vec![
        "-name".to_string(),
        config.name.clone(),
        "-display".to_string(),
        "none".to_string(),
        "-daemonize".to_string(),
        "-pidfile".to_string(),
        paths.pid.display().to_string(),
        "-monitor".to_string(),
        "none".to_string(),
        "-serial".to_string(),
        format!("file:{}", paths.serial.display()),
        "-qmp".to_string(),
        format!("unix:{},server=on,wait=off", paths.qmp.display()),
        "-smp".to_string(),
        config.cpus.to_string(),
        "-m".to_string(),
        config.memory_mib.to_string(),
        "-netdev".to_string(),
        qemu_user_network_arg(config.ssh.port, &config.forwards),
        "-device".to_string(),
        "virtio-net-pci,netdev=net0".to_string(),
        "-device".to_string(),
        "virtio-rng-pci".to_string(),
    ];

    match config.arch {
        GuestArch::Amd64 => {
            args.extend([
                "-cpu".to_string(),
                cpu_arg(resolved_accel).to_string(),
                "-machine".to_string(),
                format!("q35,accel={}", accel_name(resolved_accel)),
                "-drive".to_string(),
                format!("if=virtio,format=qcow2,file={}", paths.disk.display()),
                "-drive".to_string(),
                format!(
                    "if=virtio,format=raw,readonly=on,file={}",
                    paths.seed.display()
                ),
            ]);
        }
        GuestArch::Arm64 => {
            args.extend([
                "-cpu".to_string(),
                cpu_arg(resolved_accel).to_string(),
                "-machine".to_string(),
                format!("virt,accel={}", accel_name(resolved_accel)),
            ]);
            let firmware = discover_aarch64_firmware()?;
            args.extend([
                "-drive".to_string(),
                format!(
                    "if=pflash,format=raw,unit=0,readonly=on,file={}",
                    firmware.code.display()
                ),
                "-drive".to_string(),
                format!(
                    "if=pflash,format=raw,unit=1,file={}",
                    paths.firmware_vars.display()
                ),
                "-drive".to_string(),
                format!("if=none,id=main,format=qcow2,file={}", paths.disk.display()),
                "-device".to_string(),
                "virtio-blk-pci,drive=main".to_string(),
                "-drive".to_string(),
                format!(
                    "if=none,id=seed,format=raw,readonly=on,file={}",
                    paths.seed.display()
                ),
                "-device".to_string(),
                "virtio-blk-pci,drive=seed".to_string(),
            ]);
        }
    }

    Ok(LaunchSpec {
        program: config.arch.qemu_binary().to_string(),
        args,
    })
}

pub async fn ensure_firmware_vars(paths: &InstancePaths) -> Result<()> {
    if paths.firmware_vars.is_file() {
        return Ok(());
    }
    let firmware = discover_aarch64_firmware()?;
    tokio::fs::copy(&firmware.vars_template, &paths.firmware_vars).await?;
    Ok(())
}

pub async fn launch_vm(config: &InstanceConfig, paths: &InstancePaths) -> Result<()> {
    if config.arch == GuestArch::Arm64 {
        ensure_firmware_vars(paths).await?;
    }
    let mut errors = Vec::new();
    let attempts = launch_accel_attempts(config.accel)?;
    for (index, accel) in attempts.iter().copied().enumerate() {
        let spec = build_launch_spec_with_accel(config, paths, accel)?;
        match run_launch_spec(&spec).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                errors.push(format!("{}: {err:#}", accel_name(accel)));
                if index + 1 < attempts.len() {
                    paths.clear_runtime_artifacts().await?;
                }
            }
        }
    }
    bail!(
        "{} launch attempts failed: {}",
        config.arch.qemu_binary(),
        errors.join("; ")
    )
}

async fn run_launch_spec(spec: &LaunchSpec) -> Result<()> {
    let output = Command::new(&spec.program)
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("launch {}", spec.program))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "{} failed: {}",
            spec.program,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

pub async fn system_powerdown(qmp_socket: &Path) -> Result<()> {
    qmp_command(qmp_socket, "system_powerdown")
        .await
        .map(|_| ())
}

async fn qmp_command(qmp_socket: &Path, command: &str) -> Result<Value> {
    let stream = UnixStream::connect(qmp_socket)
        .await
        .with_context(|| format!("connect {}", qmp_socket.display()))?;
    let (reader_half, mut writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);

    let _ = read_qmp_message(&mut reader).await?;
    writer_half
        .write_all(br#"{"execute":"qmp_capabilities"}"#)
        .await?;
    writer_half.write_all(b"\n").await?;
    wait_for_qmp_return(&mut reader).await?;

    writer_half
        .write_all(format!(r#"{{"execute":"{command}"}}"#).as_bytes())
        .await?;
    writer_half.write_all(b"\n").await?;
    wait_for_qmp_return(&mut reader).await
}

async fn read_qmp_message(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            bail!("unexpected EOF from QMP socket");
        }
        if line.trim().is_empty() {
            continue;
        }
        return Ok(serde_json::from_str(line.trim())?);
    }
}

async fn wait_for_qmp_return(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<Value> {
    loop {
        let value = read_qmp_message(reader).await?;
        if value.get("return").is_some() {
            return Ok(value);
        }
        if let Some(err) = value.get("error") {
            bail!("QMP command failed: {err}");
        }
    }
}

fn accel_name(accel: AccelMode) -> &'static str {
    match accel {
        AccelMode::Hvf => "hvf",
        AccelMode::Kvm => "kvm",
        AccelMode::Tcg | AccelMode::Auto => "tcg",
    }
}

fn cpu_arg(accel: AccelMode) -> &'static str {
    match accel {
        AccelMode::Tcg => "max",
        _ => "host",
    }
}

fn qemu_user_network_arg(ssh_port: u16, forwards: &[PortForward]) -> String {
    let mut hostfwds = vec![format!("hostfwd=tcp:127.0.0.1:{ssh_port}-:22")];
    hostfwds.extend(
        forwards
            .iter()
            .map(|forward| format!("hostfwd=tcp:127.0.0.1:{}-:{}", forward.host, forward.guest)),
    );
    format!("user,id=net0,{}", hostfwds.join(","))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        build_launch_spec_with_accel, discover_aarch64_firmware, launch_accel_attempts,
        missing_kvm_error_message, qemu_user_network_arg, resolve_accel, resolve_accel_for_env,
        tcg_disabled_error_message,
    };
    use crate::state::{
        AccelMode, CloudInitConfig, GuestArch, ImageConfig, InstanceConfig, InstancePaths,
        PortForward, SshConfig,
    };

    fn config(arch: GuestArch) -> InstanceConfig {
        InstanceConfig {
            name: "dev".into(),
            release: "24.04".into(),
            arch,
            accel: AccelMode::Tcg,
            cpus: 2,
            memory_mib: 2048,
            disk_gib: 16,
            timeout_secs: 60,
            ssh: SshConfig {
                user: "ubuntu".into(),
                host: "127.0.0.1".into(),
                port: 2222,
                identity_file: PathBuf::from("/tmp/id_ed25519"),
            },
            forwards: vec![PortForward {
                host: 8080,
                guest: 8080,
            }],
            image: ImageConfig {
                release: "24.04".into(),
                arch,
                url: "https://example.invalid/ubuntu.img".into(),
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

    #[test]
    fn user_network_forwards_include_ssh_and_extras() {
        let arg = qemu_user_network_arg(
            2222,
            &[PortForward {
                host: 8080,
                guest: 8080,
            }],
        );
        assert!(arg.contains("hostfwd=tcp:127.0.0.1:2222-:22"));
        assert!(arg.contains("hostfwd=tcp:127.0.0.1:8080-:8080"));
    }

    #[test]
    fn x86_launch_spec_contains_expected_args() {
        let paths = InstancePaths::new(PathBuf::from("/tmp/dev"));
        let spec = build_launch_spec_with_accel(&config(GuestArch::Amd64), &paths, AccelMode::Kvm)
            .expect("spec");
        let joined = spec.args.join(" ");
        assert!(joined.contains("q35,accel=kvm"));
        assert!(joined.contains("if=virtio,format=qcow2,file=/tmp/dev/disk.qcow2"));
    }

    #[test]
    fn arm_launch_spec_contains_expected_args() {
        if discover_aarch64_firmware().is_err() {
            return;
        }
        let paths = InstancePaths::new(PathBuf::from("/tmp/dev"));
        let spec = build_launch_spec_with_accel(&config(GuestArch::Arm64), &paths, AccelMode::Kvm)
            .expect("spec");
        let joined = spec.args.join(" ");
        assert!(joined.contains("virt,accel=kvm"));
        assert!(joined.contains("if=pflash,format=raw,unit=1,file=/tmp/dev/firmware.vars.fd"));
    }

    #[test]
    fn auto_accel_has_single_attempt() {
        let attempts = launch_accel_attempts(AccelMode::Auto).expect("attempts");
        assert_eq!(
            attempts,
            vec![resolve_accel(AccelMode::Auto).expect("preferred accel")]
        );
    }

    #[test]
    fn explicit_supported_accel_has_single_attempt() {
        let accel = if cfg!(target_os = "macos") {
            AccelMode::Hvf
        } else if cfg!(target_os = "linux") && std::path::Path::new("/dev/kvm").exists() {
            AccelMode::Kvm
        } else {
            return;
        };
        assert_eq!(launch_accel_attempts(accel).expect("attempts"), vec![accel]);
    }

    #[test]
    fn explicit_kvm_requires_dev_kvm() {
        let err = resolve_accel_for_env(AccelMode::Kvm, "linux", false).expect_err("should fail");
        assert_eq!(err.to_string(), missing_kvm_error_message());
        assert!(err.to_string().contains("/dev/kvm"));
        assert!(err.to_string().contains("fall back to TCG"));
    }

    #[test]
    fn auto_requires_kvm_on_linux_without_dev_kvm() {
        let err = resolve_accel_for_env(AccelMode::Auto, "linux", false).expect_err("should fail");
        assert_eq!(err.to_string(), missing_kvm_error_message());
    }

    #[test]
    fn explicit_tcg_is_disabled() {
        let err = resolve_accel_for_env(AccelMode::Tcg, "linux", true).expect_err("should fail");
        assert_eq!(err.to_string(), tcg_disabled_error_message());
        assert!(err.to_string().contains("TCG acceleration is disabled"));
    }
}
