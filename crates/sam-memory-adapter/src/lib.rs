//! sam-memory-adapter — ergonomic wrapper around memory-brain's
//! [`memory_actor::MemoryGuardian`].
//!
//! M1 exposes enough surface for the `sam status` probe and a round-trip
//! store/recall smoke test. The upstream memory system quietly falls back
//! to the in-process HashEmbedder when its configured embedder URL is
//! unreachable, so calling [`MemoryAdapter::new`] is always safe from a
//! process-isolation perspective.

pub mod adapter;

pub use adapter::{MemoryAdapter, RecallHit};
pub use memory_actor::{MemorySystemConfig, MemorySystemStats};
