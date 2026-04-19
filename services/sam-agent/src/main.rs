//! `sam-agent` — personal-assistant agent binary (M1 scaffold).

mod cli;
mod cmd;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};

fn init_tracing() {
    let default_level = std::env::var("SAM_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "warn".to_string());

    let _ = tracing_subscriber::fmt()
        .with_env_filter(default_level)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let exit_code = match cli.command {
        Command::Status { json, verbose } => cmd::status::run(json, verbose).await,
        Command::Version => {
            cmd::version::run();
            0
        }
        Command::Daemon => cmd::daemon::run().await,
        Command::Chat => cmd::chat::run().await,
        Command::Telegram => cmd::telegram::run().await,
        Command::Web { port } => cmd::web::run(port).await,
        Command::Reload => cmd::reload::run(),
        Command::Send { handle, text } => cmd::send::run(handle, text).await,
        Command::ImportMemories { file } => cmd::import_memories::run(file).await,
    };
    std::process::exit(exit_code);
}
