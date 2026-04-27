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

/// Chunk of a document for RAG ingestion.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DocumentChunk {
    /// Source identifier (filename, URL, etc.).
    pub source: String,
    /// Chunk index within the document.
    pub chunk_index: usize,
    /// The text content of this chunk.
    pub text: String,
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
            vecdb_url: Some("http://localhost:3100".to_string()),
            collection: "memory_actor".to_string(),
            ..HippocampusConfig::default()
        };
        let sys_config = MemorySystemConfig {
            hippocampus: hippo_config,
            ..MemorySystemConfig::default()
        };
        info!(
            embedder_url = %config.embedder_url,
            vecdb_url = "http://localhost:3100",
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

    // ── RAG: Document Ingestion ─────────────────────────────────────────

    /// Ingest a document by chunking it and storing each chunk as a memory
    /// tagged with the source. Returns the number of chunks stored.
    pub fn ingest_document(
        &mut self,
        source: &str,
        text: &str,
        chunk_size: usize,
        chunk_overlap: usize,
    ) -> Result<usize> {
        let chunks = chunk_text(text, chunk_size, chunk_overlap);
        let count = chunks.len();

        for (i, chunk) in chunks.iter().enumerate() {
            if chunk.trim().is_empty() {
                continue;
            }
            let tagged_text = format!("[source: {source}][chunk {}/{}] {chunk}", i + 1, count);
            let tags = vec![
                "document".to_string(),
                format!("source:{source}"),
                format!("chunk:{i}"),
            ];
            self.store(tagged_text, tags)?;
        }

        info!(source = %source, chunks = count, "document ingested");
        Ok(count)
    }

    /// Hybrid recall: combines semantic similarity with keyword matching.
    /// Boosts results that contain exact keyword matches from the query.
    pub fn recall_hybrid(&mut self, query: &str, k: usize) -> Vec<RecallHit> {
        // Get more candidates than needed, then re-rank.
        let candidates = self.recall(query, k * 3);

        if candidates.is_empty() {
            return candidates;
        }

        // Extract keywords from query (3+ char tokens).
        let keywords: Vec<String> = query
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| s.len() >= 2)
            .map(|s| s.to_string())
            .collect();

        // Score each candidate: vector_similarity + keyword_boost.
        let mut scored: Vec<(RecallHit, f32)> = candidates
            .into_iter()
            .map(|hit| {
                let lower_text = hit.text.to_lowercase();
                let keyword_score: f32 = keywords
                    .iter()
                    .filter(|kw| lower_text.contains(kw.as_str()))
                    .count() as f32
                    / keywords.len().max(1) as f32;

                // Blend: 70% vector similarity + 30% keyword match.
                let hybrid_score = hit.similarity * 0.7 + keyword_score * 0.3;
                (hit, hybrid_score)
            })
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);

        scored
            .into_iter()
            .map(|(mut hit, score)| {
                hit.similarity = score;
                hit
            })
            .collect()
    }

    /// Recall documents specifically from an ingested source.
    pub fn recall_from_source(&mut self, query: &str, source: &str, k: usize) -> Vec<RecallHit> {
        let candidates = self.recall(query, k * 5);
        let source_tag = format!("source:{source}");
        candidates
            .into_iter()
            .filter(|hit| hit.text.contains(&source_tag) || hit.text.contains(source))
            .take(k)
            .collect()
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

// ── Chunking ────────────────────────────────────────────────────────

/// Split text into overlapping chunks of approximately `chunk_size` characters.
/// Prefers splitting at paragraph or sentence boundaries.
pub fn chunk_text(text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![];
    }
    if text.chars().count() <= chunk_size {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let mut start = 0;

    while start < total {
        let end = (start + chunk_size).min(total);
        let chunk_str: String = chars[start..end].iter().collect();

        // Try to find a good split point (paragraph > sentence > word).
        let actual_end = if end < total {
            if let Some(pos) = chunk_str.rfind("\n\n") {
                start + pos + 2
            } else if let Some(pos) = chunk_str.rfind(". ") {
                start + pos + 2
            } else if let Some(pos) = chunk_str.rfind('\n') {
                start + pos + 1
            } else if let Some(pos) = chunk_str.rfind(' ') {
                start + pos + 1
            } else {
                end
            }
        } else {
            end
        };

        let final_chunk: String = chars[start..actual_end].iter().collect();
        if !final_chunk.trim().is_empty() {
            chunks.push(final_chunk.trim().to_string());
        }

        // Advance with overlap.
        let advance = if actual_end > start + overlap {
            actual_end - start - overlap
        } else {
            1 // prevent infinite loop
        };
        start += advance;
    }

    chunks
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

    #[test]
    fn chunk_text_basic() {
        let text = "Hello world. This is a test document. It has multiple sentences.";
        let chunks = chunk_text(text, 30, 5);
        assert!(chunks.len() >= 2, "expected multiple chunks, got {}", chunks.len());
        // All chunks should be non-empty.
        for chunk in &chunks {
            assert!(!chunk.is_empty());
        }
    }

    #[test]
    fn chunk_text_short_no_split() {
        let text = "Short text";
        let chunks = chunk_text(text, 500, 50);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Short text");
    }

    #[test]
    fn chunk_text_empty() {
        let chunks = chunk_text("", 500, 50);
        assert!(chunks.is_empty());
    }

    #[test]
    fn ingest_document_stores_chunks() {
        let mut adapter = MemoryAdapter::new(MemorySystemConfig::default()).unwrap();
        let text = "First paragraph about Rust.\n\nSecond paragraph about Python.\n\nThird paragraph about TypeScript.";
        let count = adapter.ingest_document("test.md", text, 40, 5).unwrap();
        assert!(count >= 2, "expected multiple chunks, got {count}");

        let hits = adapter.recall("Rust", 5);
        assert!(!hits.is_empty(), "should find ingested content about Rust");
    }

    #[test]
    fn hybrid_recall_boosts_keyword_matches() {
        let mut adapter = MemoryAdapter::new(MemorySystemConfig::default()).unwrap();
        adapter.store("Rust programming language is great", vec![]).unwrap();
        adapter.store("Python is also popular", vec![]).unwrap();
        adapter.store("The weather is nice today", vec![]).unwrap();

        let hits = adapter.recall_hybrid("Rust programming", 3);
        assert!(!hits.is_empty());
        // First hit should be about Rust due to keyword boost.
        assert!(hits[0].text.contains("Rust"), "expected Rust hit first, got: {}", hits[0].text);
    }
}
