//! The memory snapshot's text, which the protocol crate renders so the
//! clients show the same turn the model reads.

#[cfg(any(test, feature = "test-support"))]
pub use cookie_agent_protocol::model_text::memory_snapshot::agent_section;
pub use cookie_agent_protocol::model_text::memory_snapshot::{
    agent_content_bytes, machines_section, project_index, project_overview, snapshot_turn,
    user_section,
};
