//! Config and flow hot-reload support.
//!
//! Watches `~/.sam/config.toml` and `~/.sam/flows/` for changes using
//! polling (no platform-specific fsnotify dependency). When changes are
//! detected, the relevant Arc<Mutex<T>> stores are refreshed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::{load_config, SamConfig};
use crate::flow_store::FlowStore;
use crate::paths::sam_home;

/// Shared config handle that can be hot-reloaded.
pub type SharedConfig = Arc<Mutex<SamConfig>>;

/// Run the hot-reload watcher loop. Checks every `interval` for file changes.
///
/// When config.toml changes → reloads `SamConfig` into the shared handle.
/// When any file in flows/ changes → calls `FlowStore::reload()`.
pub async fn run_hot_reload(
    shared_config: SharedConfig,
    flow_store: Arc<Mutex<FlowStore>>,
    cancel: CancellationToken,
    interval: Duration,
) {
    let config_path = sam_home().join("config.toml");
    let flows_dir = sam_home().join("flows");

    let mut config_mtime = file_mtime(&config_path);
    let mut flows_mtime = dir_latest_mtime(&flows_dir);

    let mut tick = tokio::time::interval(interval);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("hot-reload watcher stopped");
                return;
            }
            _ = tick.tick() => {
                // Check config.toml
                let new_config_mtime = file_mtime(&config_path);
                if new_config_mtime != config_mtime {
                    config_mtime = new_config_mtime;
                    match load_config(&config_path) {
                        Ok(new_cfg) => {
                            let mut cfg = shared_config.lock().await;
                            *cfg = new_cfg;
                            info!("config.toml hot-reloaded");
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to hot-reload config.toml");
                        }
                    }
                }

                // Check flows directory
                let new_flows_mtime = dir_latest_mtime(&flows_dir);
                if new_flows_mtime != flows_mtime {
                    flows_mtime = new_flows_mtime;
                    let mut store = flow_store.lock().await;
                    store.reload();
                    info!("flows hot-reloaded");
                }
            }
        }
    }
}

/// Get the modification time of a file, or None if it doesn't exist.
fn file_mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
}

/// Get the latest modification time of any file in a directory.
fn dir_latest_mtime(dir: &PathBuf) -> Option<SystemTime> {
    if !dir.exists() {
        return None;
    }
    let mut latest: Option<SystemTime> = None;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    latest = Some(match latest {
                        Some(prev) if mtime > prev => mtime,
                        Some(prev) => prev,
                        None => mtime,
                    });
                }
            }
        }
    }
    latest
}
