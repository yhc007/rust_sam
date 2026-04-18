//! sam-tools — discovery of external-command tool definitions under
//! `~/.sam/tools/`.
//!
//! M1 is scan-and-validate only. The registry deliberately keeps the tool
//! input schema as a raw JSON string so that downstream validation can
//! evolve without churn in this crate.

pub mod registry;
pub mod schema;

pub use registry::ToolRegistry;
pub use schema::{RawCommand, ToolDef};
