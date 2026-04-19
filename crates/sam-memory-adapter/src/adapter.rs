//! Wrapper over [`memory_actor::MemoryGuardian`] with file-based persistence.

use std::path::PathBuf;

use anyhow::Result;
use memory_actor::{
    HippocampusConfig, Memory, MemoryContext, MemoryGuardian, MemorySystemConfig,
    MemorySystemStats,
};
use sam_core::MemoryConfig;
use tracing::{debug, info, warn};
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
///
/// Memories are persisted to `~/.sam/data/memories.json` and restored on
/// startup so they survive process restarts.
pub struct MemoryAdapter {
    guardian: MemoryGuardian,
    /// Path to the persistence file.
    persist_path: PathBuf,
}

impl MemoryAdapter {
    /// Initialize with the given config. Memory-brain internally handles
    /// embedder fallback (HashEmbedder when the HTTP embedder is
    /// unreachable), so this call should not block on external services.
    pub fn new(config: MemorySystemConfig) -> Result<Self> {
        let guardian = MemoryGuardian::new(config);
        let persist_path = sam_core::expand_tilde("~/.sam/data/memories.json");
        debug!("MemoryAdapter constructed");
        Ok(Self {
            guardian,
            persist_path: PathBuf::from(persist_path),
        })
    }

    /// Initialize from Sam's `[memory]` config section.
    ///
    /// Maps `embedder_url` to the hippocampus embedding backend.
    /// Falls back to in-memory HashEmbedder when the URL is unreachable.
    /// Automatically loads persisted memories from disk.
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
        let mut adapter = Self::new(sys_config)?;
        adapter.load_from_disk();
        Ok(adapter)
    }

    /// Store a new memory. Returns the assigned id.
    /// Automatically persists to disk.
    pub fn store(&mut self, text: impl Into<String>, tags: Vec<String>) -> Result<Uuid> {
        let ctx = MemoryContext {
            source: "sam".to_string(),
            timestamp: chrono::Utc::now(),
            tags,
            metadata: None,
        };
        let id = self.guardian.store(text.into(), ctx);
        self.save_to_disk();
        Ok(id)
    }

    /// Store a conversation turn (user message + Sam's reply).
    /// Automatically persists to disk.
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

    /// Run dream consolidation — transfers episodic memories into semantic
    /// concepts, replays important memories, and prunes weak ones.
    /// Saves to disk after consolidation.
    pub fn dream(&mut self) -> String {
        self.guardian.start_dream();
        self.save_to_disk();
        let stats = self.guardian.stats();
        format!(
            "Dream consolidation complete. Memories: {}, Concepts: {}",
            stats.total_memories, stats.total_concepts
        )
    }

    /// Get recent memories from working memory.
    pub fn recent(&self, limit: usize) -> Vec<RecallHit> {
        self.guardian
            .recent(limit)
            .into_iter()
            .map(|m| RecallHit {
                id: m.id,
                text: m.content,
                similarity: m.strength,
            })
            .collect()
    }

    /// Query semantic knowledge (concepts extracted by dream consolidation).
    pub fn query_knowledge(&self, concept: &str) -> Option<String> {
        self.guardian.query_knowledge(concept)
    }

    /// Snapshot of global stats.
    pub fn stats(&self) -> MemorySystemStats {
        self.guardian.stats()
    }

    // ── Persistence ────────────────────────────────────────────────────

    /// Save all memories to disk as JSON.
    fn save_to_disk(&self) {
        let memories = self.guardian.all_memories();
        if memories.is_empty() {
            return;
        }

        // Ensure parent directory exists.
        if let Some(parent) = self.persist_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(error = %e, "failed to create memory data directory");
                return;
            }
        }

        match serde_json::to_string(&memories) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.persist_path, &json) {
                    warn!(error = %e, "failed to write memories to disk");
                } else {
                    debug!(
                        count = memories.len(),
                        path = %self.persist_path.display(),
                        "memories saved to disk"
                    );
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to serialize memories");
            }
        }
    }

    /// Load memories from disk, restoring them into the guardian.
    fn load_from_disk(&mut self) {
        if !self.persist_path.exists() {
            debug!(path = %self.persist_path.display(), "no persisted memories found");
            return;
        }

        let data = match std::fs::read_to_string(&self.persist_path) {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, "failed to read memories from disk");
                return;
            }
        };

        let memories: Vec<Memory> = match serde_json::from_str(&data) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "failed to parse memories from disk");
                return;
            }
        };

        let count = memories.len();
        for memory in memories {
            self.guardian.restore_memory(memory);
        }

        info!(
            count = count,
            path = %self.persist_path.display(),
            "restored memories from disk"
        );
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

    #[test]
    fn persistence_round_trip() {
        let tmp = std::env::temp_dir().join("sam-test-memories.json");
        let _ = std::fs::remove_file(&tmp);

        // Store
        {
            let mut adapter = MemoryAdapter::new(MemorySystemConfig::default()).unwrap();
            adapter.persist_path = tmp.clone();
            adapter.store("Remember this fact", vec!["test".into()]).unwrap();
            adapter.store("Another important thing", vec!["test".into()]).unwrap();
        }

        // Verify file exists
        assert!(tmp.exists(), "persistence file should exist");

        // Load into a new adapter
        {
            let mut adapter = MemoryAdapter::new(MemorySystemConfig::default()).unwrap();
            adapter.persist_path = tmp.clone();
            adapter.load_from_disk();

            let stats = adapter.stats();
            assert_eq!(stats.total_memories, 2, "should have restored 2 memories");

            let hits = adapter.recall("Remember", 5);
            assert!(!hits.is_empty(), "should recall restored memory");
        }

        let _ = std::fs::remove_file(&tmp);
    }
}
