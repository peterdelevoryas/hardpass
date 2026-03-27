use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hardpass::{AccelMode, Hardpass, InstanceStatus, Vm, VmSpec, VmSshInfo};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::task::JoinSet;
use tokio::time::sleep;

const REMOTE_WORK_DIR: &str = "/tmp/hardpass-e2e";
const REMOTE_EXERCISER_PATH: &str = "/tmp/hardpass-e2e/guest-exerciser";
const APT_RETRY_ATTEMPTS: usize = 3;

#[tokio::test]
#[ignore = "requires QEMU and HARDPASS_REAL_QEMU_TEST=1"]
async fn e2e_vm_stress() -> Result<()> {
    if std::env::var_os("HARDPASS_REAL_QEMU_TEST").is_none() {
        eprintln!("skipping e2e_vm_stress; set HARDPASS_REAL_QEMU_TEST=1 to enable");
        return Ok(());
    }
    if !cfg!(target_os = "linux") {
        eprintln!(
            "skipping e2e_vm_stress; host-built guest exerciser currently requires a Linux host"
        );
        return Ok(());
    }
    ensure_ci_kvm_available()?;

    let profile = Profile::from_env()?;
    let root = TestRoot::new()?;
    let hardpass = Hardpass::with_root(root.path()).await?;
    hardpass.doctor().await?;

    let guest_exerciser = build_guest_exerciser(root.path()).await?;
    let mut created_names = Vec::new();
    let mut created_vms = Vec::new();

    let result = async {
        for index in 0..profile.vm_count {
            let name = vm_name(profile.slug, index);
            let vm = hardpass
                .create(vm_spec(&name))
                .await
                .with_context(|| format!("create {name}"))?;
            created_names.push(name);
            created_vms.push(vm);
        }
        run_profile(created_vms, guest_exerciser, profile).await
    }
    .await;

    if result.is_err() {
        print_serial_log_tails(root.path(), &created_names).await;
    }

    let cleanup = cleanup_vms(&hardpass, &created_names).await;
    match (result, cleanup) {
        (Err(err), _) => Err(err),
        (Ok(()), Err(err)) => Err(err),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[derive(Clone, Copy)]
struct Profile {
    slug: &'static str,
    vm_count: usize,
    duration_secs: u64,
    io_mib: usize,
    tcp_round_trips: usize,
    packages: &'static [&'static str],
    run_stress_ng: bool,
}

impl Profile {
    fn from_env() -> Result<Self> {
        match std::env::var("HARDPASS_E2E_PROFILE")
            .unwrap_or_else(|_| "pr".to_string())
            .as_str()
        {
            "pr" => Ok(Self {
                slug: "pr",
                vm_count: 1,
                duration_secs: 20,
                io_mib: 64,
                tcp_round_trips: 256,
                packages: &["jq"],
                run_stress_ng: false,
            }),
            "stress" => Ok(Self {
                slug: "stress",
                vm_count: 2,
                duration_secs: 45,
                io_mib: 128,
                tcp_round_trips: 512,
                packages: &["jq", "stress-ng"],
                run_stress_ng: true,
            }),
            other => bail!("unsupported HARDPASS_E2E_PROFILE value: {other}"),
        }
    }
}

fn vm_spec(name: &str) -> VmSpec {
    let accel = if running_in_github_actions() {
        AccelMode::Kvm
    } else {
        AccelMode::Auto
    };
    VmSpec::new(name)
        .cpus(1)
        .memory_mib(1024)
        .disk_gib(8)
        .timeout_secs(300)
        .accel(accel)
}

async fn run_profile(vms: Vec<Vm>, guest_exerciser: PathBuf, profile: Profile) -> Result<()> {
    let mut tasks = JoinSet::new();
    for vm in vms {
        let guest_exerciser = guest_exerciser.clone();
        tasks.spawn(async move {
            let name = vm.name().to_string();
            exercise_vm(vm, &guest_exerciser, profile)
                .await
                .with_context(|| format!("exercise {name}"))
        });
    }

    let mut failures = Vec::new();
    while let Some(outcome) = tasks.join_next().await {
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(err)) => failures.push(format!("{err:#}")),
            Err(err) => failures.push(format!("join error: {err}")),
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "{} VM exercises failed:\n{}",
            failures.len(),
            failures.join("\n\n")
        )
    }
}

async fn exercise_vm(vm: Vm, guest_exerciser: &Path, profile: Profile) -> Result<()> {
    vm.start().await?;
    let info = vm.wait_for_ssh().await?;
    if info.status != InstanceStatus::Running {
        bail!("{} did not reach running state", info.name);
    }

    let machine = run_remote_command_checked(&vm, ["uname", "-m"])
        .await?
        .stdout;
    if machine.trim() != expected_guest_machine() {
        bail!(
            "unexpected guest machine for {}: {}",
            info.name,
            machine.trim()
        );
    }

    run_apt_step(&vm, "sudo apt-get update").await?;
    run_apt_step(
        &vm,
        &format!(
            "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends {}",
            profile.packages.join(" ")
        ),
    )
    .await?;

    run_remote_shell_checked(&vm, "dpkg -s jq >/dev/null && jq --version >/dev/null").await?;
    if profile.run_stress_ng {
        run_remote_shell_checked(
            &vm,
            "dpkg -s stress-ng >/dev/null && stress-ng --version >/dev/null",
        )
        .await?;
        run_remote_shell_checked(
            &vm,
            "stress-ng --cpu 1 --timeout 10 --metrics-brief >/tmp/hardpass-e2e/stress-ng.log",
        )
        .await?;
    }

    upload_guest_exerciser(&info.ssh, guest_exerciser).await?;
    let summary = run_guest_exerciser(&vm, profile).await?;
    if summary.cpu_iterations == 0 {
        bail!("{} reported zero cpu iterations", info.name);
    }
    if summary.io_bytes < (profile.io_mib as u64 * 1024 * 1024) {
        bail!(
            "{} reported too few io bytes: {}",
            info.name,
            summary.io_bytes
        );
    }
    if summary.tcp_round_trips != profile.tcp_round_trips as u64 {
        bail!(
            "{} reported {} tcp round trips, expected {}",
            info.name,
            summary.tcp_round_trips,
            profile.tcp_round_trips
        );
    }

    vm.stop().await?;
    if vm.status().await? != InstanceStatus::Stopped {
        bail!("{} did not stop cleanly", vm.name());
    }
    Ok(())
}

#[derive(Debug)]
struct GuestSummary {
    cpu_iterations: u64,
    io_bytes: u64,
    tcp_round_trips: u64,
}

async fn run_guest_exerciser(vm: &Vm, profile: Profile) -> Result<GuestSummary> {
    let command = vec![
        REMOTE_EXERCISER_PATH.to_string(),
        "--duration-secs".to_string(),
        profile.duration_secs.to_string(),
        "--io-mib".to_string(),
        profile.io_mib.to_string(),
        "--tcp-round-trips".to_string(),
        profile.tcp_round_trips.to_string(),
    ];
    let output = vm.exec(command).await?;
    if !output.status.success() {
        bail!(
            "guest exerciser failed with status {}:\nstdout:\n{}\nstderr:\n{}",
            output.status,
            output.stdout.trim(),
            output.stderr.trim()
        );
    }

    let payload: serde_json::Value = serde_json::from_str(output.stdout.trim())
        .with_context(|| format!("parse guest exerciser output: {}", output.stdout.trim()))?;
    if payload.get("status").and_then(serde_json::Value::as_str) != Some("ok") {
        bail!("guest exerciser did not report ok status: {payload}");
    }

    Ok(GuestSummary {
        cpu_iterations: payload
            .get("cpu_iterations")
            .and_then(serde_json::Value::as_u64)
            .context("missing cpu_iterations in guest summary")?,
        io_bytes: payload
            .get("io_bytes")
            .and_then(serde_json::Value::as_u64)
            .context("missing io_bytes in guest summary")?,
        tcp_round_trips: payload
            .get("tcp_round_trips")
            .and_then(serde_json::Value::as_u64)
            .context("missing tcp_round_trips in guest summary")?,
    })
}

async fn run_apt_step(vm: &Vm, script: &str) -> Result<()> {
    let mut last_error = None;
    for attempt in 1..=APT_RETRY_ATTEMPTS {
        let output = vm.exec(["sh", "-lc", script]).await?;
        if output.status.success() {
            return Ok(());
        }

        last_error = Some(format!(
            "attempt {attempt}/{APT_RETRY_ATTEMPTS} failed for `{script}`:\nstdout:\n{}\nstderr:\n{}",
            output.stdout.trim(),
            output.stderr.trim()
        ));
        if attempt < APT_RETRY_ATTEMPTS {
            sleep(Duration::from_secs(attempt as u64 * 5)).await;
        }
    }

    bail!(
        "{}",
        last_error.unwrap_or_else(|| format!("apt step failed: {script}"))
    );
}

async fn run_remote_command_checked<I, S>(vm: &Vm, command: I) -> Result<hardpass::ExecOutput>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let output = vm.exec(command).await?;
    if output.status.success() {
        Ok(output)
    } else {
        bail!(
            "remote command failed with status {}:\nstdout:\n{}\nstderr:\n{}",
            output.status,
            output.stdout.trim(),
            output.stderr.trim()
        )
    }
}

async fn run_remote_shell_checked(vm: &Vm, script: &str) -> Result<hardpass::ExecOutput> {
    run_remote_command_checked(vm, ["sh", "-lc", script]).await
}

async fn upload_guest_exerciser(ssh: &VmSshInfo, local_path: &Path) -> Result<()> {
    let payload = tokio::fs::read(local_path)
        .await
        .with_context(|| format!("read {}", local_path.display()))?;
    let remote_command = format!(
        "mkdir -p {REMOTE_WORK_DIR} && cat > {REMOTE_EXERCISER_PATH} && chmod +x {REMOTE_EXERCISER_PATH}"
    );

    let mut child = Command::new("ssh")
        .args(ssh_args(ssh))
        .arg(format!("{}@{}", ssh.user, ssh.host))
        .arg("sh")
        .arg("-lc")
        .arg(remote_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn ssh upload")?;

    let mut stdin = child.stdin.take().context("missing ssh stdin")?;
    stdin
        .write_all(&payload)
        .await
        .context("stream upload payload")?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .context("wait for ssh upload")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "guest upload failed with status {}:\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn ssh_args(ssh: &VmSshInfo) -> Vec<String> {
    vec![
        "-i".to_string(),
        ssh.identity_file.display().to_string(),
        "-p".to_string(),
        ssh.port.to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "IdentitiesOnly=yes".to_string(),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
    ]
}

async fn build_guest_exerciser(root: &Path) -> Result<PathBuf> {
    let output_dir = root.join("artifacts");
    tokio::fs::create_dir_all(&output_dir).await?;
    let output_path = output_dir.join("guest-exerciser");
    let source_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("guest_exerciser.rs");

    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let output = Command::new(rustc)
        .arg("--edition=2024")
        .arg("-O")
        .arg("-C")
        .arg("debuginfo=0")
        .arg("-o")
        .arg(&output_path)
        .arg(&source_path)
        .output()
        .await
        .with_context(|| format!("compile {}", source_path.display()))?;
    if output.status.success() {
        Ok(output_path)
    } else {
        bail!(
            "rustc failed for {}:\nstdout:\n{}\nstderr:\n{}",
            source_path.display(),
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

async fn cleanup_vms(hardpass: &Hardpass, names: &[String]) -> Result<()> {
    let mut errors = Vec::new();
    for name in names {
        match hardpass.vm(name)?.delete().await {
            Ok(()) => {}
            Err(err) => errors.push(format!("{name}: {err:#}")),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!("cleanup failed:\n{}", errors.join("\n"))
    }
}

async fn print_serial_log_tails(root: &Path, names: &[String]) {
    for name in names {
        let serial_path = root.join("instances").join(name).join("serial.log");
        eprintln!("--- serial tail for {name} ({}) ---", serial_path.display());
        match tokio::fs::read_to_string(&serial_path).await {
            Ok(content) => {
                let tail = content
                    .lines()
                    .rev()
                    .take(60)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                if tail.is_empty() {
                    eprintln!("<empty serial log>");
                } else {
                    eprintln!("{tail}");
                }
            }
            Err(err) => eprintln!("unable to read serial log: {err}"),
        }
    }
}

fn vm_name(profile: &str, index: usize) -> String {
    format!("e2e_{profile}_{}_{}", std::process::id(), index)
}

fn expected_guest_machine() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => panic!("unsupported host architecture for e2e test: {other}"),
    }
}

fn ensure_ci_kvm_available() -> Result<()> {
    if !running_in_github_actions() {
        return Ok(());
    }

    let path = Path::new("/dev/kvm");
    if !path.exists() {
        bail!(
            "GitHub Actions hardpass e2e requires /dev/kvm. TCG fallback is disabled; run this workflow on a KVM-enabled Linux runner."
        );
    }

    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| {
            "GitHub Actions hardpass e2e requires accessible /dev/kvm. TCG fallback is disabled; ensure the runner grants read/write access to /dev/kvm."
        })?;
    Ok(())
}

fn running_in_github_actions() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some()
}

enum TestRoot {
    Persistent(PathBuf),
    Temp(TempDir),
}

impl TestRoot {
    fn new() -> Result<Self> {
        match std::env::var_os("HARDPASS_E2E_ROOT") {
            Some(path) => Ok(Self::Persistent(PathBuf::from(path))),
            None => Ok(Self::Temp(tempfile::tempdir()?)),
        }
    }

    fn path(&self) -> &Path {
        match self {
            Self::Persistent(path) => path.as_path(),
            Self::Temp(dir) => dir.path(),
        }
    }
}
