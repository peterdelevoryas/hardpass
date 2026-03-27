mod api;
pub mod cli;
mod cloud_init;
mod images;
mod instance;
mod lock;
mod ports;
mod qemu;
mod ssh;
mod ssh_config;
mod state;

use anyhow::Result;

pub use api::{Hardpass, Vm, VmSpec};
use cli::{Args, Command, SshConfigCommand};
use instance::InstanceManager;
pub use instance::{VmInfo, VmSshInfo};
pub use ssh::ExecOutput;
use state::HardpassState;
pub use state::{AccelMode, GuestArch, InstanceStatus, PortForward};

pub async fn run(args: Args) -> Result<()> {
    let state = HardpassState::load().await?;
    let manager = InstanceManager::new(state);
    match args.command {
        Command::Doctor => manager.doctor().await,
        Command::Create(args) => manager.create(args).await,
        Command::Start(args) => manager.start(&args.name).await,
        Command::Stop(args) => manager.stop(&args.name).await,
        Command::Delete(args) => manager.delete(&args.name).await,
        Command::List => manager.list().await,
        Command::Info(args) => manager.info(&args.name, args.json).await,
        Command::SshConfig(args) => match args.command {
            SshConfigCommand::Install => manager.install_ssh_config().await,
            SshConfigCommand::Sync => manager.sync_ssh_config().await,
        },
        Command::Ssh(args) => manager.ssh(&args.name, &args.ssh_args).await,
        Command::Exec(args) => manager.exec(&args.name, &args.command).await,
    }
}
