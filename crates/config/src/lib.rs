//! Strict runtime configuration and Markdown agent documents.

mod agent_document;
mod agents;
mod cache;
mod error;
mod loader;
mod mcp_store;
mod prompt_template;
mod runtime;
mod runtime_templates;
mod secure_fs;
mod skills;
mod toml_values;
mod wildcard;

pub use agent_document::{AgentDocument, AgentDocumentSource};
pub use agents::{
    AgentFrontmatter, AgentLimits, AgentMaterializationInput, AgentMode, AgentModelFallback,
    AgentModelRef, AgentRegistry, BUILT_IN_APPROVAL_AGENT_ID, BUILT_IN_COMPACTION_AGENT_ID,
    BUILT_IN_DEFAULT_AGENT_ID, BUILT_IN_TITLE_AGENT_ID, PARENT_MODEL_EXPRESSION, PermissionAction,
    PermissionEffect, PermissionKey, PermissionRule, PermissionValue,
};
pub use cache::{
    AnthropicCacheConfig, BedrockCacheConfig, CacheConfig, CacheTtl, OpenAiCacheConfig,
    OpenAiCacheMode, OpenAiPromptCacheRetention, OpenAiPromptCacheTtl, RollingCacheTtl,
};
pub use error::ConfigError;
pub use loader::{ConfigLayerPaths, LoadedConfiguration, load, load_from_user_root};
pub use loader::{LoadedMcpServer, McpServerSource};
pub use mcp_store::{write_mcp_server, write_server_token};
pub use prompt_template::{
    DEFAULT_PROMPT_PLACEHOLDER, FOCUS_PLACEHOLDER, InternalPromptValues, render_internal_prompt,
};
pub use runtime::{
    AgentMdConfig, ApprovalConfig, ContextCompactionConfig, ContextCompactionTrigger,
    DelegationConfig, EngineConfig, LoopWarningConfig, McpConfig, McpOAuthConfig, McpOAuthSettings,
    McpServerConfig, MessagingConfig, ModelPricing, ModelRetryConfig, PRODUCER_MAX_BODY_BYTES,
    PicoUsdPerMillion, PluginConfig, PluginsConfig, PricingConfig, ProducersConfig,
    SUBAGENT_OUTPUT_MAX_BYTES, ServerConfig, SessionTitleConfig, SubagentOutputConfig,
    ToolOutputConfig, valid_daemon_token, valid_daemon_url,
};
pub use runtime_templates::RuntimeTemplateConfig;
pub use skills::{
    SkillAllowedTool, SkillContext, SkillDiagnostic, SkillDiscovery, SkillDocument,
    SkillFrontmatter, SkillRegistry, load_project_skills, load_skill_roots, load_user_skills,
    parse_skill_document, render_available_skills,
};
pub use wildcard::simple_wildcard_match;
