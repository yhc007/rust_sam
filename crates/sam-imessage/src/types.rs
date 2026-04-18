//! Shared message types for the iMessage adapter.

use serde::{Deserialize, Serialize};

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
}

/// A message to be sent via osascript.
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub handle: String,
    pub body: String,
}
