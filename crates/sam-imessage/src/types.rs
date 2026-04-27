//! Shared message types for the iMessage adapter.

use serde::{Deserialize, Serialize};

/// An image/file attachment received from iMessage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// Expanded filesystem path to the attachment file.
    pub path: String,
    /// MIME type, e.g. "image/jpeg".
    pub mime_type: String,
    /// Original filename (transfer_name from chat.db).
    pub filename: String,
}

/// A message received from chat.db.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingMessage {
    pub rowid: i64,
    pub text: String,
    /// Handle identifier, e.g. "+821038600983".
    pub sender: String,
    /// Seconds since Unix epoch.
    pub timestamp_unix: i64,
    /// Raw Apple Absolute Time (nanoseconds since 2001-01-01).
    pub raw_apple_ts: i64,
    /// Image attachments associated with this message.
    pub attachments: Vec<Attachment>,
}

/// A message to be sent via osascript.
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub handle: String,
    pub body: String,
    /// Optional file path to attach (image, PDF, etc.).
    pub attachment: Option<String>,
}
