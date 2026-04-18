//! Wrapper over [`memory_actor::MemoryGuardian`].

use anyhow::Result;
use memory_actor::{
    HippocampusConfig, MemoryContext, MemoryGuardian, MemorySystemConfig,
    MemorySystemStats,
};
use sam_core::MemoryConfig;
use tracing::{debug, info};
use uuid::Uuid;

/// A single recall hit (text + similarity score).
#[derive(Debug, Clone)]
pub struct RecallHit {
    pub id: Uuid,
    pub text: String,
    pub similarity: f32,
}

/// Ergonomic, single-owner handle to the CLS memory system.
///
/// `MemoryAdapter` keeps ownership of the underlying `MemoryGuardian` and
/// brokers the two operations Sam cares about during M1: storing a new
/// memory and recalling top-k by similarity.
pub struct MemoryAdapter {
    guardian: MemoryGuardian,
}

impl MemoryAdapter {
    /// Initialize with the given config. Memory-brain internally handles
    /// embedder fallback (HashEmbedder when the HTTP embedder is
    /// unreachable), so this call should not block on external services.
    pub fn new(config: MemorySystemConfig) -> Result<Self> {
        let guardian = MemoryGuardian::new(config);
        debug!("MemoryAdapter constructed");
        Ok(Self { guardian })
    }

    /// Initialize from Sam's `[memory]` config section.
    ///
    /// Maps `embedder_url` to the hippocampus embedding backend.
    /// Falls back to in-memory HashEmbedder when the URL is unreachable.
    pub fn from_config(config: &MemoryConfig) -> Result<Self> {
        let hippo_config = HippocampusConfig {
            embedding_url: Some(config.embedder_url.clone()),
            ..HippocampusConfig::default()
        };
        let sys_config = MemorySystemConfig {
            hippocampus: hippo_config,
            ..MemorySystemConfig::default()
        };
        info!(
            embedder_url = %config.embedder_url,
            "MemoryAdapter initialising"
        );
        Self::new(sys_config)
    }

    /// Store a new memory. Returns the assigned id.
    pub fn store(&mut self, text: impl Into<String>, tags: Vec<String>) -> Result<Uuid> {
        let ctx = MemoryContext {
            source: "sam".to_string(),
            timestamp: chrono::Utc::now(),
            tags,
            metadata: None,
        };
        let id = self.guardian.store(text.into(), ctx);
        Ok(id)
    }

    /// Store a conversation turn (user message + Sam's reply).
    pub fn store_conversation(
        &mut self,
        handle: &str,
        user_text: &str,
        reply_text: &str,
    ) -> Result<Uuid> {
        let text = format!("[user] {user_text}\n[sam] {reply_text}");
        let tags = vec!["conversation".to_string(), handle.to_string()];
        self.store(text, tags)
    }

    /// Recall top-k memories by semantic similarity. Returns `(text, score)`
    /// pairs — the id is retained internally.
    pub fn recall(&mut self, query: &str, k: usize) -> Vec<RecallHit> {
        self.guardian
            .recall(query, k)
            .into_iter()
            .map(|r| RecallHit {
                id: r.memory.id,
                text: r.memory.content,
                similarity: r.similarity,
            })
            .collect()
    }

    /// Recall top-k memories and format them as a context block suitable
    /// for injection into the system prompt. Returns an empty string when
    /// nothing relevant is found.
    pub fn recall_context(&mut self, query: &str, k: usize) -> String {
        let hits = self.recall(query, k);
        if hits.is_empty() {
            return String::new();
        }
        let mut out = String::from("## 관련 기억\n");
        for hit in &hits {
            out.push_str(&format!("- {}\n", hit.text.replace('\n', " | ")));
        }
        out
    }

    /// Snapshot of global stats.
    pub fn stats(&self) -> MemorySystemStats {
        self.guardian.stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_recall_round_trip_with_hash_embedder_fallback() {
        let mut adapter = MemoryAdapter::new(MemorySystemConfig::default()).unwrap();
        let _id = adapter
            .store(
                "Rust actor models and the pekko runtime",
                vec!["rust".into(), "actor".into()],
            )
            .unwrap();
        let hits = adapter.recall("rust actor", 3);
        assert!(!hits.is_empty(), "expected at least one recall hit");
        let stats = adapter.stats();
        assert!(stats.total_memories >= 1);
    }
}
