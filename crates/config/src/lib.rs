//! Strict runtime configuration and Markdown agent documents.

mod agent_document;
mod agents;
mod cache;
mod error;
mod loader;
mod prompt_template;
mod runtime;
mod runtime_templates;
mod secure_fs;
mod skills;
mod toml_values;
mod tree;
mod wildcard;
mod writer;

pub use agent_document::{AgentDocument, AgentDocumentSource};
pub use agents::{
    AcpDocumentSettings, AcpModelChoice, AgentFrontmatter, AgentLimits, AgentMaterializationInput,
    AgentMode, AgentModelFallback, AgentModelRef, AgentRegistry, BUILT_IN_APPROVAL_AGENT_ID,
    BUILT_IN_COMPACTION_AGENT_ID, BUILT_IN_DEFAULT_AGENT_ID, BUILT_IN_TITLE_AGENT_ID,
    DEFAULT_MODEL_EXPRESSION, PARENT_MODEL_EXPRESSION, PermissionAction, PermissionEffect,
    PermissionKey, PermissionRule, PermissionValue, SMALL_MODEL_EXPRESSION,
};
pub use cache::{
    AnthropicCacheConfig, BedrockCacheConfig, CacheConfig, CacheLayer, CacheTtl, CacheTtls,
    OpenAiCacheConfig, OpenAiCacheMode, OpenAiPromptCacheRetention, OpenAiPromptCacheTtl,
    StructuralCacheConfig,
};
pub use error::ConfigError;
pub use loader::{
    ConfigLayerPaths, LoadedConfiguration, load, load_from_tree, load_from_user_root,
};
pub use loader::{LoadedMcpServer, McpServerSource};
pub use prompt_template::{
    DEFAULT_PROMPT_PLACEHOLDER, FOCUS_PLACEHOLDER, InternalPromptValues, render_internal_prompt,
};
pub use runtime::{
    AcpConfig, AgentMdConfig, ApprovalCacheTtl, ApprovalConfig, ConfiguredModel,
    ContextCompactionConfig, ContextCompactionTrigger, CronConfig, DelegationConfig, EngineConfig,
    LoopWarningConfig, McpConfig, McpOAuthConfig, McpOAuthSettings, McpServerConfig,
    McpServerScope, MemoryConfig, MessagingConfig, ModelPricing, ModelRetryConfig,
    PRODUCER_MAX_BODY_BYTES, PicoUsdPerMillion, PluginConfig, PluginsConfig, PricingConfig,
    ProducersConfig, SUBAGENT_OUTPUT_MAX_BYTES, ServerConfig, SessionTitleConfig, SkillsConfig,
    SubagentOutputConfig, ToolOutputConfig, valid_daemon_token, valid_daemon_url,
};
pub use runtime_templates::{AcpAgentConfig, RuntimeTemplateConfig};
pub use skills::{
    OFFICIAL_SKILLS_DIRECTORY, SkillAllowedTool, SkillContext, SkillDiagnostic, SkillDiscovery,
    SkillDocument, SkillFrontmatter, SkillRegistry, load_project_skills, load_skill_roots,
    load_skill_roots_with_official, load_user_skills, load_user_skills_with_official,
    official_skills_root, parse_skill_document, render_available_skills,
};
pub use tree::{CONFIG_FILE, ConfigDocument, ConfigPath, ConfigTree};
pub use wildcard::simple_wildcard_match;
pub use writer::{write_beside, write_server_token, write_tree};
