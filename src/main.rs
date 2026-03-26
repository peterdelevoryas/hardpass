use anyhow::Result;
use clap::Parser;

use hardpass::cli::Args;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    hardpass::run(args).await
}
