use std::{fmt, io};

use cookie_agent_identity::{AgentId, ModelKey, ProviderId};
use thiserror::Error;

#[derive(Error)]
pub enum ConfigError {
    #[error("configuration I/O failed")]
    Io(#[source] io::Error),
    #[error("configuration path is unsafe")]
    UnsafePath,
    #[error("required file was not found")]
    NotFound,
    #[error("{0} exceeds its byte limit")]
    TooLarge(String),
    #[error("{0} is not UTF-8")]
    Utf8(&'static str),
    #[error("invalid TOML: {0}")]
    Toml(String),
    #[error("TOML resource limit or unsupported datetime exceeded")]
    TomlLimit,
    #[error("provider `{provider}` is invalid")]
    Provider {
        provider: ProviderId,
        #[source]
        source: cookie_agent_models::authoring::AuthoringError,
    },
    #[error("runtime settings are invalid")]
    InvalidRuntime,
    #[error("environment interpolation is invalid at `{0}`")]
    Interpolation(String),
    #[error("environment variable `{variable}` is missing at `{path}`")]
    MissingEnvironment { path: String, variable: String },
    #[error("environment variable `{variable}` is not UTF-8 at `{path}`")]
    NonUtf8Environment { path: String, variable: String },
    #[error("invalid agent filename `{0}`")]
    AgentFilename(String),
    #[error("duplicate agent `{0}`")]
    DuplicateAgent(AgentId),
    #[error("agent ID `{0}` is reserved for a built-in agent")]
    ReservedAgentId(AgentId),
    #[error("invalid frontmatter for agent `{0}`")]
    AgentFrontmatter(AgentId),
    #[error(
        "agent `{0}` permission resources support only `${{workspace_dir}}`, and only for read or write"
    )]
    AgentPermissionExpression(AgentId),
    #[error("agent YAML resource limit exceeded")]
    AgentYamlLimit,
    #[error("agent `{0}` has an empty system prompt")]
    EmptyPrompt(AgentId),
    #[error("agent `{agent}` has invalid `{field}`")]
    AgentField { agent: AgentId, field: &'static str },
    #[error("agent `{0}` exceeds a list limit")]
    AgentLimit(AgentId),
    #[error("primary agent `{0}` requires a nonempty fallback")]
    PrimaryFallback(AgentId),
    #[error("agent `{0}` lists a tool more than once")]
    DuplicateTool(AgentId),
    #[error("agent `{0}` has invalid delegation")]
    Delegation(AgentId),
    #[error("agent `{agent}` delegates to unknown agent `{target}`")]
    UnknownDelegationTarget { agent: AgentId, target: AgentId },
    #[error("agent `{agent}` delegates to ineligible agent `{target}`")]
    IneligibleDelegationTarget { agent: AgentId, target: AgentId },
    #[error("agent `{agent}` repeats fallback model `{model}`")]
    DuplicateFallbackModel { agent: AgentId, model: ModelKey },
}

impl fmt::Debug for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ConfigError")
            .field(&self.to_string())
            .finish()
    }
}
