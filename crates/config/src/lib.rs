//! Strict schema-10 runtime configuration and schema-5 Markdown agents.

mod agent_document;
mod agents;
mod error;
mod loader;
mod runtime;
mod secure_fs;
mod toml_values;
mod wildcard;

pub use agent_document::{AgentDocument, AgentDocumentSource};
pub use agents::{
    AgentFrontmatter, AgentLimits, AgentMaterializationInput, AgentMode, AgentModelFallback,
    AgentModelRef, AgentRegistry, AgentSchemaVersion, BUILT_IN_APPROVAL_AGENT_ID,
    BUILT_IN_COMPACTION_AGENT_ID, BUILT_IN_DEFAULT_AGENT_ID, BUILT_IN_TITLE_AGENT_ID,
    PARENT_MODEL_EXPRESSION, PermissionAction, PermissionEffect, PermissionRule, PermissionValue,
};
pub use error::ConfigError;
pub use loader::{LoadedConfiguration, load, load_from_roots};
pub use loader::{LoadedMcpServer, McpServerSource};
pub use runtime::{
    ApprovalConfig, ConfigSchemaVersion, ContextCompactionConfig, ContextCompactionTrigger,
    DelegationConfig, McpConfig, McpServerConfig, RuntimeConfig, ServerConfig, SessionTitleConfig,
    ToolOutputConfig,
};
pub use wildcard::simple_wildcard_match;
