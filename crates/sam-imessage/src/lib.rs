//! sam-imessage — iMessage adapter.
//!
//! M1 provides health probes and AppleScript string building.
//! M2 adds live polling (poller) and outbound sending (outbound).

pub mod outbound;
pub mod poller;
pub mod probe;
pub mod reader;
pub mod sender;
pub mod state;
pub mod types;

pub use probe::{automation_status, can_read_chat_db, ProbeResult};
pub use reader::ChatDbReader;
pub use sender::{build_applescript, build_applescript_live, dry_send};
pub use types::{IncomingMessage, OutgoingMessage};
