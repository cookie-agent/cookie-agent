//! Strict schema-8 runtime configuration and schema-2 Markdown agents.

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
    AgentFrontmatter, AgentMaterializationInput, AgentMode, AgentModelFallback, AgentRegistry,
    AgentSchemaVersion, BUILT_IN_DEFAULT_AGENT_ID, PermissionAction, PermissionEffect,
    PermissionRule, PermissionValue, ToolName,
};
pub use error::ConfigError;
pub use loader::{LoadedConfiguration, load, load_from_roots};
pub use runtime::{
    ApprovalConfig, ConfigSchemaVersion, ContextCompactionConfig, DelegationConfig, RuntimeConfig,
    ServerConfig, SessionTitleConfig, ToolOutputConfig,
};
pub use wildcard::simple_wildcard_match;
