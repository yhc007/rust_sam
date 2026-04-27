//! Outbound iMessage sender — executes osascript with rate limiting
//! and exponential-backoff retry.

use std::time::Duration;

use anyhow::{bail, Result};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::sender::{build_applescript_live, build_applescript_attachment};
use crate::types::OutgoingMessage;

/// Run the sender loop, consuming messages from `rx` and delivering them via
/// osascript. Enforces `send_rate_limit_ms` between consecutive sends.
pub async fn run_sender(
    send_rate_limit_ms: u64,
    mut rx: mpsc::Receiver<OutgoingMessage>,
    cancel: CancellationToken,
) -> Result<()> {
    let rate_limit = Duration::from_millis(send_rate_limit_ms);

    info!(rate_limit_ms = send_rate_limit_ms, "Sender started");

    loop {
        let msg = tokio::select! {
            _ = cancel.cancelled() => {
                info!("Sender stopped");
                return Ok(());
            }
            msg = rx.recv() => match msg {
                Some(m) => m,
                None => {
                    info!("Sender: channel closed");
                    return Ok(());
                }
            }
        };

        let result = if let Some(ref attachment) = msg.attachment {
            send_attachment_with_retry(&msg.handle, attachment, &msg.body, 3).await
        } else {
            send_with_retry(&msg.handle, &msg.body, 3).await
        };
        if let Err(e) = result {
            error!(handle = %msg.handle, "message dropped after retries: {e}");
        }

        sleep(rate_limit).await;
    }
}

/// Send a single message via osascript (no retry, no rate limiting).
/// Useful for `sam send` one-shot command.
pub async fn send_once(handle: &str, text: &str) -> Result<()> {
    let script = build_applescript_live(handle, text);
    let output = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .await?;

    if output.status.success() {
        info!(handle, bytes = text.len(), "sent");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("osascript failed: {stderr}")
    }
}

/// Send with exponential backoff. Base delay = 1s, doubling each retry.
async fn send_with_retry(handle: &str, body: &str, max_retries: u32) -> Result<()> {
    let script = build_applescript_live(handle, body);
    let mut backoff = Duration::from_secs(1);

    for attempt in 0..=max_retries {
        let output = Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output()
            .await?;

        if output.status.success() {
            info!(handle, bytes = body.len(), "sent");
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if attempt < max_retries {
            warn!(attempt, %stderr, "osascript failed, retrying in {backoff:?}");
            sleep(backoff).await;
            backoff *= 2;
        } else {
            bail!("osascript failed after {max_retries} retries: {stderr}");
        }
    }

    unreachable!()
}

/// Send a file attachment with exponential backoff retry.
async fn send_attachment_with_retry(
    handle: &str,
    file_path: &str,
    caption: &str,
    max_retries: u32,
) -> Result<()> {
    let script = build_applescript_attachment(handle, file_path, caption);
    let mut backoff = Duration::from_secs(1);

    for attempt in 0..=max_retries {
        let output = Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output()
            .await?;

        if output.status.success() {
            info!(handle, file = file_path, "attachment sent");
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if attempt < max_retries {
            warn!(attempt, %stderr, "attachment send failed, retrying in {backoff:?}");
            sleep(backoff).await;
            backoff *= 2;
        } else {
            bail!("attachment send failed after {max_retries} retries: {stderr}");
        }
    }

    unreachable!()
}
