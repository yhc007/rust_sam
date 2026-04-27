//! Persistent delivery queue — ensures outbound messages survive daemon restarts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state_dir;

/// A message waiting to be delivered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub id: String,
    pub handle: String,
    pub body: String,
    pub queued_at: i64,
}

/// Persistent outbound message queue.
///
/// Messages are written to disk before sending. On successful send,
/// they are removed. On daemon restart, pending messages are retried.
pub struct DeliveryQueue {
    pending: Vec<QueuedMessage>,
    path: PathBuf,
}

impl DeliveryQueue {
    /// Load from disk or create empty.
    pub fn load() -> Self {
        let path = state_dir().join("delivery_queue.json");
        let pending = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                    warn!("failed to parse delivery_queue.json: {e}");
                    Vec::new()
                }),
                Err(e) => {
                    warn!("failed to read delivery_queue.json: {e}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        if !pending.is_empty() {
            info!(count = pending.len(), "DeliveryQueue: pending messages from previous run");
        }
        Self { pending, path }
    }

    /// Enqueue a message (persist to disk). Returns the queue id.
    pub fn enqueue(&mut self, handle: &str, body: &str) -> String {
        let msg = QueuedMessage {
            id: Uuid::new_v4().to_string(),
            handle: handle.to_string(),
            body: body.to_string(),
            queued_at: chrono::Utc::now().timestamp(),
        };
        let id = msg.id.clone();
        self.pending.push(msg);
        self.persist();
        id
    }

    /// Mark a message as successfully delivered (remove from queue).
    pub fn ack(&mut self, id: &str) {
        let before = self.pending.len();
        self.pending.retain(|m| m.id != id);
        if self.pending.len() < before {
            self.persist();
        }
    }

    /// Get all pending messages (for flush on startup or retry).
    pub fn pending(&self) -> &[QueuedMessage] {
        &self.pending
    }

    /// Drain all pending messages (returns owned vec, clears queue).
    pub fn drain_pending(&mut self) -> Vec<QueuedMessage> {
        let msgs = std::mem::take(&mut self.pending);
        self.persist();
        msgs
    }

    /// Check if queue is empty.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    fn persist(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&self.pending) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    warn!("failed to write delivery_queue.json: {e}");
                }
            }
            Err(e) => warn!("failed to serialize delivery queue: {e}"),
        }
        debug!(count = self.pending.len(), "delivery queue persisted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_ack_roundtrip() {
        let mut q = DeliveryQueue {
            pending: Vec::new(),
            path: std::env::temp_dir().join("sam-test-delivery-q.json"),
        };

        let id1 = q.enqueue("+8210", "hello");
        let id2 = q.enqueue("+8210", "world");
        assert_eq!(q.pending().len(), 2);

        q.ack(&id1);
        assert_eq!(q.pending().len(), 1);
        assert_eq!(q.pending()[0].id, id2);

        q.ack(&id2);
        assert!(q.is_empty());

        let _ = std::fs::remove_file(&q.path);
    }

    #[test]
    fn drain_clears_queue() {
        let mut q = DeliveryQueue {
            pending: Vec::new(),
            path: std::env::temp_dir().join("sam-test-delivery-drain.json"),
        };
        q.enqueue("+8210", "msg1");
        q.enqueue("+8210", "msg2");

        let drained = q.drain_pending();
        assert_eq!(drained.len(), 2);
        assert!(q.is_empty());

        let _ = std::fs::remove_file(&q.path);
    }
}
