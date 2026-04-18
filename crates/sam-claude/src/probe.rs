//! Claude Code CLI health probe.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::process::Command;
use tokio::time::timeout;

/// Timeout applied to `claude --version`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Invoke `<binary> --version` and return the trimmed stdout.
///
/// Errors if the binary is missing, exits non-zero, or exceeds
/// [`PROBE_TIMEOUT`].
pub async fn claude_version(binary: &Path) -> Result<String> {
    if !binary.exists() {
        return Err(anyhow!(
            "claude binary not found at {}",
            binary.display()
        ));
    }

    let mut cmd = Command::new(binary);
    cmd.arg("--version");
    cmd.kill_on_drop(true);

    let fut = cmd.output();
    let output = timeout(PROBE_TIMEOUT, fut)
        .await
        .map_err(|_| anyhow!("claude --version timed out after {:?}", PROBE_TIMEOUT))?
        .with_context(|| format!("spawning {}", binary.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "claude --version exited {} (stderr: {})",
            output.status,
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        Err(anyhow!("claude --version produced empty output"))
    } else {
        Ok(stdout)
    }
}
