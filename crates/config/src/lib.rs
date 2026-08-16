//! Strict runtime configuration and Markdown agent documents.

mod agent_document;
mod agents;
mod error;
mod loader;
mod mcp_store;
mod runtime;
mod secure_fs;
mod toml_values;
mod wildcard;

pub use agent_document::{AgentDocument, AgentDocumentSource};
pub use agents::{
    AgentFrontmatter, AgentLimits, AgentMaterializationInput, AgentMode, AgentModelFallback,
    AgentModelRef, AgentRegistry, BUILT_IN_APPROVAL_AGENT_ID, BUILT_IN_COMPACTION_AGENT_ID,
    BUILT_IN_DEFAULT_AGENT_ID, BUILT_IN_TITLE_AGENT_ID, PARENT_MODEL_EXPRESSION, PermissionAction,
    PermissionEffect, PermissionRule, PermissionValue,
};
pub use error::ConfigError;
pub use loader::{ConfigLayerPaths, LoadedConfiguration, load, load_from_roots};
pub use loader::{LoadedMcpServer, McpServerSource};
pub use mcp_store::write_mcp_server;
pub use runtime::{
    ApprovalConfig, ContextCompactionConfig, ContextCompactionTrigger, DelegationConfig, McpConfig,
    McpServerConfig, RuntimeConfig, ServerConfig, SessionTitleConfig, ToolOutputConfig,
};
pub use wildcard::simple_wildcard_match;
