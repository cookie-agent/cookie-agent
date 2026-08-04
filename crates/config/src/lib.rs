//! Strict schema-6 runtime configuration and schema-1 Markdown agents.

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
    AgentDelegationConfig, AgentDescriptor, AgentFrontmatter, AgentMode, AgentModelFallback,
    AgentRegistry, AgentSchemaVersion, DelegatedModelPlan, PermissionAction, PermissionEffect,
    PermissionRule, ResolvedAgent, RootModelPlan, ToolName,
};
pub use error::ConfigError;
pub use loader::{LoadedConfiguration, load, load_from_roots};
pub use runtime::{
    ApprovalConfig, ConfigSchemaVersion, ContextCompactionConfig, RuntimeConfig, ServerConfig,
    SessionTitleConfig, ToolOutputConfig,
};
pub use wildcard::simple_wildcard_match;
