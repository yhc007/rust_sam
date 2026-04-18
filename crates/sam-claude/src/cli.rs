//! Claude Code CLI session interface (stub).

use std::path::PathBuf;

use anyhow::Result;
use tracing::info;

/// A request to spawn a new Claude Code session.
///
/// Kept small and serializable-friendly so M2 can trivially queue it.
#[derive(Debug, Clone)]
pub struct ClaudeSpawnRequest {
    /// Natural-language prompt to hand off to Claude Code.
    pub prompt: String,
    /// Session tag used to namespace on-disk artifacts.
    pub tag: String,
    /// Optional override for `--permission-mode`.
    pub permission_mode_override: Option<String>,
}

/// Configured interface to the Claude Code CLI.
pub struct ClaudeCli {
    pub binary: PathBuf,
    pub default_mode: String,
    pub session_root: PathBuf,
}

impl ClaudeCli {
    pub fn new(binary: PathBuf, default_mode: String, session_root: PathBuf) -> Self {
        Self { binary, default_mode, session_root }
    }

    /// Spawn a Claude Code session.
    ///
    /// M1: dry-run only — this logs the invocation that *would* happen and
    /// returns an error so callers can fall back gracefully. Full
    /// subprocess orchestration lands in M2 alongside the actor wiring.
    pub async fn spawn(&self, req: ClaudeSpawnRequest) -> Result<()> {
        let mode = req
            .permission_mode_override
            .as_deref()
            .unwrap_or(&self.default_mode);
        info!(
            target = "sam_claude::cli",
            binary = %self.binary.display(),
            mode = mode,
            tag = %req.tag,
            session_root = %self.session_root.display(),
            "spawn requested (M1 stub — not executed)",
        );
        anyhow::bail!("sam_claude::ClaudeCli::spawn is not implemented in M1 (M2 wires this up)")
    }
}
