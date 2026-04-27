//! Clap-derived command-line interface for `sam-agent`.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "sam-agent",
    version,
    about = "Sam — personal-assistant agent",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print a static + live health report.
    Status {
        /// Emit the report as a single JSON object instead of colored text.
        #[arg(long)]
        json: bool,
        /// Print extra detail per probe.
        #[arg(short, long)]
        verbose: bool,
    },

    /// Print the binary version (short form).
    Version,

    /// Run Sam as a long-lived daemon.
    Daemon,

    /// Interactive CLI chat with Sam.
    Chat,

    /// Run Sam as a Telegram bot.
    Telegram,

    /// Run Sam as a web server.
    Web {
        /// Port to listen on (default: 3000).
        #[arg(short, long, default_value = "3000")]
        port: u16,
    },

    /// Run the health monitoring dashboard.
    Dashboard {
        /// Port to listen on (default: 3100).
        #[arg(short, long, default_value = "3100")]
        port: u16,
    },

    /// Signal a running Sam to reload config/prompts. Stub in M1.
    Reload,

    /// Send a one-shot iMessage (for debugging).
    Send {
        /// The recipient handle (e.g. "+821038600983").
        handle: String,
        /// The message text to send.
        text: String,
    },

    /// Import memories from a JSON file into the memory system.
    ImportMemories {
        /// Path to a JSON file: [{"text": "...", "tags": ["..."]}]
        file: String,
    },
}
