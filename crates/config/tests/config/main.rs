//! The configuration crate's integration tests, one module per surface, in
//! one test binary.

mod acp_agents;
mod agent_inherit;
mod agent_md;
mod approval;
mod compaction_recent_tokens;
mod compaction_trigger;
mod config_tree;
mod cron;
mod default_models;
mod deferred_tools;
mod internal_prompt;
mod mcp_config;
mod memory;
mod messaging;
mod model_retry;
mod notifications;
mod official_agents;
mod official_skills;
mod output_previews;
mod permission_schema_parity;
mod plugin_config;
mod producers;
mod prompt_caching;
mod runtime_templates;
mod server_token;
mod shell_environment;
mod wire_type_serde_equivalence;
