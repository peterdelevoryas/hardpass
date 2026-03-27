use clap::{Args as ClapArgs, Parser, Subcommand};

use crate::state::{AccelMode, GuestArch};

#[derive(Debug, Parser)]
#[command(name = "hp")]
#[command(about = "Manage local Ubuntu cloud-image VMs with QEMU")]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check the local environment for required tools and firmware.
    Doctor,
    /// Manage cached Ubuntu cloud images.
    Image(ImageArgs),
    /// Create a named VM.
    Create(CreateArgs),
    /// Start a named VM.
    Start(NameArgs),
    /// Gracefully stop a named VM.
    Stop(NameArgs),
    /// Stop and remove a named VM.
    Delete(NameArgs),
    /// List known VMs.
    List,
    /// Show details for a named VM.
    Info(InfoArgs),
    /// Open an interactive SSH session to a running VM.
    Ssh(SshArgs),
    /// Execute a remote command over SSH.
    Exec(ExecArgs),
}

#[derive(Debug, Clone, ClapArgs)]
pub struct NameArgs {
    pub name: String,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ImageArgs {
    #[command(subcommand)]
    pub command: ImageCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ImageCommand {
    /// Download and verify a cloud image into the local cache.
    Prefetch(PrefetchImageArgs),
}

#[derive(Debug, Clone, ClapArgs)]
pub struct PrefetchImageArgs {
    #[arg(long)]
    pub release: Option<String>,
    #[arg(long, value_enum)]
    pub arch: Option<GuestArch>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct InfoArgs {
    pub name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct SshArgs {
    pub name: String,
    #[arg(last = true)]
    pub ssh_args: Vec<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ExecArgs {
    pub name: String,
    #[arg(required = true, last = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct CreateArgs {
    pub name: String,
    #[arg(long)]
    pub release: Option<String>,
    #[arg(long, value_enum)]
    pub arch: Option<GuestArch>,
    #[arg(long, value_enum)]
    pub accel: Option<AccelMode>,
    #[arg(long)]
    pub cpus: Option<u8>,
    #[arg(long)]
    pub memory_mib: Option<u32>,
    #[arg(long)]
    pub disk_gib: Option<u32>,
    #[arg(long)]
    pub ssh_key: Option<String>,
    #[arg(long = "forward", value_parser = parse_forward)]
    pub forwards: Vec<(u16, u16)>,
    #[arg(long)]
    pub timeout_secs: Option<u64>,
    #[arg(long)]
    pub cloud_init_user_data: Option<String>,
    #[arg(long)]
    pub cloud_init_network_config: Option<String>,
}

fn parse_forward(value: &str) -> Result<(u16, u16), String> {
    let (host, guest) = value
        .split_once(':')
        .ok_or_else(|| "forward must be HOST:GUEST".to_string())?;
    let host = host
        .parse::<u16>()
        .map_err(|_| format!("invalid host port: {host}"))?;
    let guest = guest
        .parse::<u16>()
        .map_err(|_| format!("invalid guest port: {guest}"))?;
    Ok((host, guest))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Args, Command, ImageCommand};
    use crate::state::{AccelMode, GuestArch};

    #[test]
    fn parses_create_command() {
        let args = Args::parse_from([
            "hp",
            "create",
            "dev",
            "--release",
            "24.04",
            "--arch",
            "arm64",
            "--accel",
            "tcg",
            "--cpus",
            "2",
            "--memory-mib",
            "2048",
            "--disk-gib",
            "12",
            "--forward",
            "8080:8080",
        ]);
        match args.command {
            Command::Create(create) => {
                assert_eq!(create.name, "dev");
                assert_eq!(create.release.as_deref(), Some("24.04"));
                assert_eq!(create.arch, Some(GuestArch::Arm64));
                assert_eq!(create.accel, Some(AccelMode::Tcg));
                assert_eq!(create.cpus, Some(2));
                assert_eq!(create.memory_mib, Some(2048));
                assert_eq!(create.disk_gib, Some(12));
                assert_eq!(create.forwards, vec![(8080, 8080)]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_start_command() {
        let args = Args::parse_from(["hp", "start", "dev"]);
        match args.command {
            Command::Start(start) => assert_eq!(start.name, "dev"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_image_prefetch_command() {
        let args = Args::parse_from([
            "hp",
            "image",
            "prefetch",
            "--release",
            "24.04",
            "--arch",
            "arm64",
        ]);
        match args.command {
            Command::Image(image) => match image.command {
                ImageCommand::Prefetch(prefetch) => {
                    assert_eq!(prefetch.release.as_deref(), Some("24.04"));
                    assert_eq!(prefetch.arch, Some(GuestArch::Arm64));
                }
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_exec_command() {
        let args = Args::parse_from(["hp", "exec", "dev", "--", "uname", "-m"]);
        match args.command {
            Command::Exec(exec) => {
                assert_eq!(exec.name, "dev");
                assert_eq!(exec.command, vec!["uname", "-m"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
