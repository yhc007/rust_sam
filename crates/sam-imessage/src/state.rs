//! Poller state persistence at `~/.sam/state/imessage.json`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Persisted poller checkpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PollerState {
    pub last_seen_rowid: i64,
    pub updated_at: String,
}

/// Path to the state file (`~/.sam/state/imessage.json`).
pub fn state_file_path() -> PathBuf {
    sam_core::state_dir().join("imessage.json")
}

/// Load the poller state from the default path. Returns a default state if
/// the file does not exist yet (first run).
pub fn load_state() -> Result<PollerState> {
    load_state_from(&state_file_path())
}

/// Load the poller state from an explicit path.
pub fn load_state_from(path: &std::path::Path) -> Result<PollerState> {
    if !path.exists() {
        return Ok(PollerState::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let state: PollerState =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(state)
}

/// Atomically save the poller state to the default path.
pub fn save_state(state: &PollerState) -> Result<()> {
    save_state_to(state, &state_file_path())
}

/// Atomically save the poller state to an explicit path (write tmp + rename).
pub fn save_state_to(state: &PollerState, path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut state = state.clone();
    state.updated_at = now_iso8601();

    let json = serde_json::to_string_pretty(&state)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Simple ISO 8601 timestamp from the system clock (no chrono dependency).
fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Produce a UTC timestamp in the format "2026-04-17T12:34:56Z".
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Convert days since 1970-01-01 to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Civil calendar algorithm (Howard Hinnant).
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_round_trip() {
        let dir = std::env::temp_dir().join("sam-state-rt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("imessage.json");
        let original = PollerState {
            last_seen_rowid: 42,
            updated_at: String::new(),
        };
        save_state_to(&original, &path).unwrap();
        let loaded = load_state_from(&path).unwrap();
        assert_eq!(loaded.last_seen_rowid, 42);
        assert!(!loaded.updated_at.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_returns_default() {
        let path = std::env::temp_dir().join("sam-state-no-exist").join("imessage.json");
        let state = load_state_from(&path).unwrap();
        assert_eq!(state.last_seen_rowid, 0);
    }
}
