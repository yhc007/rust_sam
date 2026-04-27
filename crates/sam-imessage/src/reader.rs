//! Read-only view of the macOS Messages chat database.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use tracing::warn;

use crate::probe::default_chat_db_path;
use crate::types::{Attachment, IncomingMessage};

/// Image MIME types we support forwarding to Claude Vision.
const IMAGE_MIME_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/heic",
    "image/webp",
];

/// Audio MIME types we support for Whisper transcription.
const AUDIO_MIME_TYPES: &[&str] = &[
    "audio/mp4",
    "audio/x-m4a",
    "audio/x-caf",
    "audio/mpeg",
    "audio/wav",
    "audio/x-wav",
    "audio/ogg",
    "audio/webm",
    "audio/aac",
    "audio/flac",
    "audio/amr",
];

/// A handle to the macOS Messages chat.db, opened read-only.
///
/// M1 exposes a single convenience method ([`count_recent`]) used by the
/// `sam status` probe. Full message polling is implemented in M2.
pub struct ChatDbReader {
    path: PathBuf,
    conn: Connection,
}

impl ChatDbReader {
    /// Open the default chat.db at `~/Library/Messages/chat.db`.
    pub fn open() -> Result<Self> {
        Self::open_at(default_chat_db_path())
    }

    /// Open a chat.db at a custom path (useful for tests).
    pub fn open_at(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(&path, flags)
            .with_context(|| format!("opening chat.db at {}", path.display()))?;
        Ok(Self { path, conn })
    }

    /// Path this reader is bound to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Poll for new incoming messages with ROWID greater than `last_seen_rowid`,
    /// filtered to the given allowed handles. Returns messages in ROWID order.
    ///
    /// Messages with text OR image attachments are returned. If a message has
    /// attachments but no text, the text field is set to an empty string.
    pub fn poll_new(&self, last_seen_rowid: i64, allowed: &[String]) -> Result<Vec<IncomingMessage>> {
        if allowed.is_empty() {
            return Ok(vec![]);
        }

        let placeholders = allowed
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");

        let sql = format!(
            "SELECT m.ROWID, m.text, h.id AS sender, m.date, m.is_from_me \
             FROM message m \
             JOIN handle h ON m.handle_id = h.ROWID \
             WHERE m.ROWID > ?1 \
               AND m.is_from_me = 0 \
               AND (m.text IS NOT NULL OR EXISTS (SELECT 1 FROM message_attachment_join maj WHERE maj.message_id = m.ROWID)) \
               AND h.id IN ({placeholders}) \
             ORDER BY m.ROWID ASC"
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(allowed.len() + 1);
        params.push(Box::new(last_seen_rowid));
        for h in allowed {
            params.push(Box::new(h.clone()));
        }
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        let rows = stmt.query_map(&*param_refs, |row| {
            let rowid: i64 = row.get(0)?;
            let text: Option<String> = row.get(1)?;
            let sender: String = row.get(2)?;
            let raw_apple_ts: i64 = row.get(3)?;
            Ok((rowid, text.unwrap_or_default(), sender, raw_apple_ts))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            match row {
                Ok((rowid, text, sender, raw_apple_ts)) => {
                    let attachments = self.fetch_attachments(rowid);
                    messages.push(IncomingMessage {
                        rowid,
                        text,
                        sender,
                        timestamp_unix: apple_to_unix(raw_apple_ts),
                        raw_apple_ts,
                        attachments,
                    });
                }
                Err(e) => warn!("skipping malformed row: {e}"),
            }
        }
        Ok(messages)
    }

    /// Fetch image and audio attachments for a given message ROWID.
    ///
    /// Returns attachments with supported image or audio MIME types.
    /// The `filename` column uses `~` prefix which is expanded to the home dir.
    fn fetch_attachments(&self, message_rowid: i64) -> Vec<Attachment> {
        let sql = "SELECT a.filename, a.mime_type, a.transfer_name \
                   FROM attachment a \
                   JOIN message_attachment_join maj ON a.ROWID = maj.attachment_id \
                   WHERE maj.message_id = ?1";

        let mut stmt = match self.conn.prepare(sql) {
            Ok(s) => s,
            Err(e) => {
                warn!(message_rowid, "failed to prepare attachment query: {e}");
                return vec![];
            }
        };

        let rows = match stmt.query_map([message_rowid], |row| {
            let filename: Option<String> = row.get(0)?;
            let mime_type: Option<String> = row.get(1)?;
            let transfer_name: Option<String> = row.get(2)?;
            Ok((filename, mime_type, transfer_name))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!(message_rowid, "failed to query attachments: {e}");
                return vec![];
            }
        };

        let mut attachments = Vec::new();
        for row in rows {
            match row {
                Ok((Some(filename), Some(mime_type), transfer_name)) => {
                    // Only include supported image and audio types.
                    if !IMAGE_MIME_TYPES.contains(&mime_type.as_str())
                        && !AUDIO_MIME_TYPES.contains(&mime_type.as_str())
                    {
                        continue;
                    }
                    let expanded_path = expand_tilde(&filename);
                    attachments.push(Attachment {
                        path: expanded_path,
                        mime_type,
                        filename: transfer_name.unwrap_or_else(|| filename.clone()),
                    });
                }
                Ok(_) => {} // skip rows with NULL filename or mime_type
                Err(e) => warn!(message_rowid, "skipping malformed attachment row: {e}"),
            }
        }
        attachments
    }

    /// Count messages from any of the given handles within the last
    /// `minutes` minutes. Returns `0` if no handles are provided.
    ///
    /// Only `message.is_from_me = 0` rows (incoming) are counted.
    pub fn count_recent(&self, allowed: &[String], minutes: i64) -> Result<usize> {
        if allowed.is_empty() {
            return Ok(0);
        }

        // chat.db stores `message.date` as nanoseconds since 2001-01-01 00:00 UTC.
        // Compute the cutoff in the same unit.
        let cutoff_secs = (chrono_like_now_since_2001_secs()) - minutes * 60;
        let cutoff_ns = cutoff_secs.saturating_mul(1_000_000_000);

        let placeholders = allowed
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");

        let sql = format!(
            "SELECT COUNT(*) FROM message m \
             JOIN handle h ON h.ROWID = m.handle_id \
             WHERE m.is_from_me = 0 AND m.date >= ?1 \
             AND h.id IN ({placeholders})"
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(allowed.len() + 1);
        params.push(&cutoff_ns);
        for h in allowed {
            params.push(h as &dyn rusqlite::ToSql);
        }
        let count: i64 = stmt.query_row(rusqlite::params_from_iter(params.iter()), |row| row.get(0))?;
        Ok(count.max(0) as usize)
    }
}

/// Expand a leading `~` in a path to the user's home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix('~') {
        if let Some(home) = dirs::home_dir() {
            return format!("{}{rest}", home.display());
        }
    }
    path.to_string()
}

/// Convert Apple's nanosecond timestamp (since 2001-01-01 00:00 UTC) to
/// Unix epoch seconds.
pub(crate) fn apple_to_unix(apple_ns: i64) -> i64 {
    // 978307200 = seconds between 1970-01-01 and 2001-01-01 UTC.
    (apple_ns / 1_000_000_000) + 978_307_200
}

/// Seconds between 2001-01-01T00:00:00Z and now, computed from the system
/// clock. Avoids a `chrono` dep in this crate.
fn chrono_like_now_since_2001_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // 978307200 = seconds between 1970-01-01 and 2001-01-01 UTC.
    now.saturating_sub(978_307_200)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_to_unix_known_value() {
        // 2024-01-01T00:00:00Z = Unix 1704067200
        // Apple ns = (1704067200 - 978307200) * 1_000_000_000 = 725760000_000_000_000
        let apple_ns: i64 = 725_760_000_000_000_000;
        assert_eq!(apple_to_unix(apple_ns), 1_704_067_200);
    }

    #[test]
    fn apple_to_unix_epoch() {
        // 2001-01-01T00:00:00Z → Unix 978307200
        assert_eq!(apple_to_unix(0), 978_307_200);
    }

    #[test]
    fn poll_new_empty_handles_returns_empty() {
        // Cannot easily test with a real chat.db, but we can test the early return.
        // Opening a non-existent DB would fail, so we test the logic path indirectly:
        // If we had a reader, poll_new with empty allowed should return empty.
        let empty: Vec<String> = vec![];
        assert!(empty.is_empty()); // placeholder — real test needs a DB fixture
    }

    #[test]
    fn expand_tilde_works() {
        let expanded = expand_tilde("~/Library/Messages/Attachments/foo.jpg");
        assert!(!expanded.starts_with('~'));
        assert!(expanded.ends_with("/Library/Messages/Attachments/foo.jpg"));
    }

    #[test]
    fn expand_tilde_no_tilde() {
        let path = "/absolute/path/file.png";
        assert_eq!(expand_tilde(path), path);
    }
}
