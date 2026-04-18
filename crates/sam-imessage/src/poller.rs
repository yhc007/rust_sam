//! Inbound polling task — reads chat.db on a timer and emits
//! [`IncomingMessage`]s to the router.

use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use sam_core::IMessageConfig;

use crate::reader::ChatDbReader;
use crate::state::{load_state, save_state};
use crate::types::IncomingMessage;

/// Run the polling loop until `cancel` is triggered.
///
/// Each tick opens chat.db (via `spawn_blocking` — rusqlite is `!Send`),
/// queries for new rows past `last_seen_rowid`, and forwards matching
/// messages through `tx`.
pub async fn run_poller(
    config: IMessageConfig,
    tx: mpsc::Sender<IncomingMessage>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut state = load_state()?;
    let mut tick = interval(Duration::from_millis(config.poll_interval_ms));

    info!(
        last_seen_rowid = state.last_seen_rowid,
        poll_ms = config.poll_interval_ms,
        handles = ?config.allowed_handles,
        "Poller started"
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                save_state(&state)?;
                info!(last_seen_rowid = state.last_seen_rowid, "Poller stopped");
                return Ok(());
            }
            _ = tick.tick() => {
                let allowed = config.allowed_handles.clone();
                let rowid = state.last_seen_rowid;

                let messages = match tokio::task::spawn_blocking(move || {
                    let reader = ChatDbReader::open()?;
                    reader.poll_new(rowid, &allowed)
                })
                .await
                {
                    Ok(Ok(msgs)) => msgs,
                    Ok(Err(e)) => {
                        error!("poll_new error: {e}");
                        continue;
                    }
                    Err(e) => {
                        error!("spawn_blocking panic: {e}");
                        continue;
                    }
                };

                let prev_rowid = state.last_seen_rowid;
                for msg in messages {
                    state.last_seen_rowid = msg.rowid;
                    if tx.send(msg).await.is_err() {
                        info!("poller: receiver dropped, exiting");
                        save_state(&state)?;
                        return Ok(());
                    }
                }

                // Persist state when we advanced.
                if state.last_seen_rowid > prev_rowid {
                    if let Err(e) = save_state(&state) {
                        error!("failed to save poller state: {e}");
                    }
                }
            }
        }
    }
}
