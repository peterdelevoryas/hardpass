use std::path::Path;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};

use hardpass::cli::Args;

#[tokio::main]
async fn main() -> Result<()> {
    let mut command = Args::command();
    if let Some(argv0) = std::env::args_os().next()
        && let Some(file_name) = Path::new(&argv0).file_name()
    {
        let name: &'static str =
            Box::leak(file_name.to_string_lossy().into_owned().into_boxed_str());
        command = command.name(name);
    }
    let matches = command.get_matches();
    let args = Args::from_arg_matches(&matches).expect("clap validated matches");
    hardpass::run(args).await
}
