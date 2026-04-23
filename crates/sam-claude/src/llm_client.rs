//! Provider-agnostic LLM client trait.
//!
//! Re-exports [`LlmBackend`](crate::backend::LlmBackend) as `LlmClient` so
//! both names are available throughout the codebase.

pub use crate::backend::LlmBackend as LlmClient;
