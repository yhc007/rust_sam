//! Health probes for iMessage integration.

use std::path::PathBuf;

/// Outcome of a single probe step.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub ok: bool,
    pub detail: String,
}

impl ProbeResult {
    pub fn ok(detail: impl Into<String>) -> Self {
        Self { ok: true, detail: detail.into() }
    }

    pub fn warn(detail: impl Into<String>) -> Self {
        Self { ok: false, detail: detail.into() }
    }
}

/// Default path to the macOS Messages chat database.
pub fn default_chat_db_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    home.join("Library/Messages/chat.db")
}

/// Check whether we can open `chat.db` in read-only mode.
///
/// Returns `ok=true` if the database is reachable and openable; otherwise
/// `ok=false` with a human-readable reason (typically an FDA hint).
pub fn can_read_chat_db() -> ProbeResult {
    let path = default_chat_db_path();
    if !path.exists() {
        return ProbeResult::warn(format!(
            "chat.db not found at {} — is Messages.app set up for this account?",
            path.display()
        ));
    }

    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        | rusqlite::OpenFlags::SQLITE_OPEN_URI
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;

    match rusqlite::Connection::open_with_flags(&path, flags) {
        Ok(_) => ProbeResult::ok("readable"),
        Err(e) => ProbeResult::warn(format!(
            "not readable — grant Full Disk Access to the binary ({e})"
        )),
    }
}

/// Best-effort probe of Messages-app automation availability. M1 does not
/// actually invoke `osascript`; it merely checks that the binary exists on
/// `$PATH` and returns an informational placeholder.
pub fn automation_status() -> ProbeResult {
    match which_osascript() {
        Some(path) => ProbeResult::ok(format!(
            "osascript at {} (automation prompt will fire on first send)",
            path.display()
        )),
        None => ProbeResult::warn("osascript not found on PATH".to_string()),
    }
}

fn which_osascript() -> Option<PathBuf> {
    let candidates = ["/usr/bin/osascript", "/usr/local/bin/osascript"];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    // Fallback: scan $PATH.
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let cand = dir.join("osascript");
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}
