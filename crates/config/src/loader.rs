use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use cookie_agent_identity::AgentId;
use cookie_agent_identity::ProviderId;
use cookie_agent_models::ProviderDefinition;
use cookie_agent_protocol::paths;
use indexmap::IndexMap;
use serde::Deserialize as _;

use crate::{
    AgentDocument, AgentDocumentSource, AgentRegistry, ApprovalConfig, ConfigError,
    ContextCompactionConfig, DelegationConfig, PromptCachingConfig, RuntimeConfig, ServerConfig,
    SessionTitleConfig, ToolOutputConfig,
    agent_document::parse_agent,
    runtime::{RawRuntimeLayer, apply_settings, validate_runtime},
    secure_fs::{
        LayerRoot, open_layer_root, open_optional_directory, read_optional_file,
        read_required_file, regular_file_at,
    },
    toml_values::{
        SensitiveJsonValue, SensitiveProviderValues, SensitiveTomlValue,
        interpolate_provider_values, safe_toml_error, validate_toml_value, zeroize_toml_value,
    },
};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_AGENT_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug)]
pub struct LoadedConfiguration {
    pub runtime: RuntimeConfig,
    pub agents: BTreeMap<AgentId, AgentDocument>,
    pub mcp_servers: BTreeMap<String, LoadedMcpServer>,
    pub user_mcp_servers: BTreeMap<String, crate::McpServerConfig>,
    pub workspace_mcp_servers: BTreeMap<String, crate::McpServerConfig>,
    pub plugins: IndexMap<String, crate::PluginConfig>,
    pub config_paths: ConfigLayerPaths,
    pub skills: crate::SkillRegistry,
}

#[derive(Clone, Debug, Default)]
pub struct ConfigLayerPaths {
    pub user: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpServerSource {
    UserFile,
    WorkspaceFile,
    Runtime,
}

#[derive(Clone, Debug)]
pub struct LoadedMcpServer {
    pub source: McpServerSource,
    pub config: crate::McpServerConfig,
}

impl LoadedConfiguration {
    #[must_use]
    pub fn agent_registry(&self) -> AgentRegistry {
        AgentRegistry::from_validated(self.agents.clone())
    }
}

/// Loads user config and exact-cwd `.cookie-agent` with no upward search.
pub fn load(cwd: &Path) -> Result<LoadedConfiguration, ConfigError> {
    let cwd = cwd.canonicalize().map_err(ConfigError::Io)?;
    let user = user_root();
    let mut loaded = load_from_roots(user.as_deref(), Some(&cwd.join(".cookie-agent")))?;
    loaded.skills = crate::load_skills(&cwd)?;
    Ok(loaded)
}

/// Loads explicit roots, primarily for composition and adversarial tests.
pub fn load_from_roots(
    user_root: Option<&Path>,
    workspace_root: Option<&Path>,
) -> Result<LoadedConfiguration, ConfigError> {
    let user = user_root
        .map(|path| open_layer_root(path, AgentDocumentSource::User))
        .transpose()?
        .flatten();
    let workspace = workspace_root
        .map(|path| open_layer_root(path, AgentDocumentSource::Workspace))
        .transpose()?
        .flatten();
    let mut runtime = RuntimeConfig {
        server: ServerConfig::default(),
        tool_output: ToolOutputConfig::default(),
        approval: ApprovalConfig::default(),
        context_compaction: ContextCompactionConfig::default(),
        prompt_caching: PromptCachingConfig::default(),
        session_title: SessionTitleConfig::default(),
        delegation: DelegationConfig::default(),
        pricing: Default::default(),
        providers: BTreeMap::new(),
    };
    let mut agents = BTreeMap::new();
    let mut mcp_servers = BTreeMap::new();
    let mut user_mcp_servers = BTreeMap::new();
    let mut workspace_mcp_servers = BTreeMap::new();
    let mut plugins = IndexMap::new();
    for root in [user.as_ref(), workspace.as_ref()].into_iter().flatten() {
        if let Some((mut layer, source_text)) = root.load_runtime()? {
            apply_settings(&mut runtime, &layer);
            if let Some(mcp) = layer.mcp.take() {
                let source = match root.source {
                    AgentDocumentSource::User => McpServerSource::UserFile,
                    AgentDocumentSource::Workspace => McpServerSource::WorkspaceFile,
                    AgentDocumentSource::BuiltIn => {
                        unreachable!("configuration roots are authored")
                    }
                };
                for (name, config) in mcp.servers {
                    match source {
                        McpServerSource::UserFile => {
                            user_mcp_servers.insert(name.clone(), config.clone());
                        }
                        McpServerSource::WorkspaceFile => {
                            workspace_mcp_servers.insert(name.clone(), config.clone());
                        }
                        McpServerSource::Runtime => unreachable!("file configuration layer"),
                    }
                    mcp_servers.insert(name, LoadedMcpServer { source, config });
                }
            }
            if let Some(layer_plugins) = layer.plugins.take() {
                plugins.extend(plugins_in_authored_order(layer_plugins, &source_text).map(
                    |(name, config)| {
                        (
                            name,
                            (
                                config,
                                root.path.join("config.toml"),
                                Arc::clone(&source_text),
                            ),
                        )
                    },
                ));
            }
            for (id, mut value) in std::mem::take(&mut layer.providers) {
                interpolate_provider_values(
                    value.value_mut(),
                    &mut vec!["providers".to_owned(), id.as_str().to_owned()],
                )?;
                let json = SensitiveJsonValue::from_toml(value.take());
                let provider = ProviderDefinition::deserialize(json.value()).map_err(|error| {
                    config_decode_error(
                        &root.path.join("config.toml"),
                        None,
                        &format!("provider `{id}`: {error}"),
                    )
                })?;
                runtime.providers.insert(id, provider);
            }
        }
        for (id, document) in root.load_agents()? {
            agents.insert(id, document);
        }
    }
    AgentRegistry::validate_ref(&agents)?;
    validate_runtime(&runtime)?;
    for (name, server) in &mcp_servers {
        server.config.validate(name)?;
    }
    for (name, (plugin, path, text)) in &plugins {
        if let Some(field) = plugin.invalid_field(name) {
            return Err(config_decode_error(
                path,
                Some(text),
                &format!("plugin `{name}`: invalid field `{field}`"),
            ));
        }
    }
    let plugins = plugins
        .into_iter()
        .map(|(name, (config, _, _))| (name, config))
        .collect();
    Ok(LoadedConfiguration {
        runtime,
        agents,
        mcp_servers,
        user_mcp_servers,
        workspace_mcp_servers,
        plugins,
        config_paths: ConfigLayerPaths {
            user: user_root.map(|root| root.join("config.toml")),
            workspace: workspace_root.map(|root| root.join("config.toml")),
        },
        skills: crate::SkillRegistry::default(),
    })
}

fn plugins_in_authored_order(
    mut plugins: crate::PluginsConfig,
    source_text: &str,
) -> impl Iterator<Item = (String, crate::PluginConfig)> {
    let document = source_text
        .parse::<toml_edit::DocumentMut>()
        .expect("validated TOML reparses for plugin ordering");
    let names: Vec<String> = document
        .get("plugins")
        .and_then(toml_edit::Item::as_table)
        .map(|table| table.iter().map(|(name, _)| name.to_owned()).collect())
        .unwrap_or_default();
    names.into_iter().filter_map(move |name| {
        plugins
            .plugins
            .shift_remove(&name)
            .map(|config| (name, config))
    })
}

impl LayerRoot {
    fn load_runtime(&self) -> Result<Option<(RawRuntimeLayer, Arc<str>)>, ConfigError> {
        let Some(bytes) = read_optional_file(&self.path, "config.toml", MAX_CONFIG_BYTES)? else {
            return Ok(None);
        };
        let path = self.path.join("config.toml");
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| ConfigError::Toml(format!("{}: content is not UTF-8", path.display())))?;
        let value = text.parse::<toml::Value>().map_err(|error| {
            ConfigError::Toml(format!(
                "{}: {}",
                path.display(),
                safe_toml_error(text, &error)
            ))
        })?;
        let mut value = SensitiveTomlValue::new(value);
        validate_toml_value(value.value()).map_err(|_| {
            ConfigError::Toml(format!(
                "{}: TOML resource limit or unsupported datetime exceeded",
                path.display()
            ))
        })?;
        let layer = decode_runtime_layer(value.value_mut(), text, &path)?;
        Ok(Some((layer, Arc::from(text))))
    }

    fn load_agents(&self) -> Result<BTreeMap<AgentId, AgentDocument>, ConfigError> {
        let Some(directory) = open_optional_directory(&self.path, "agents")? else {
            return Ok(BTreeMap::new());
        };
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&directory).map_err(ConfigError::Io)? {
            let entry = entry.map_err(ConfigError::Io)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ConfigError::UnsafePath)?;
            if name.ends_with(".md") {
                names.push(name);
            } else if !regular_file_at(&directory, &name)? {
                return Err(ConfigError::UnsafePath);
            }
        }
        names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        let mut documents = BTreeMap::new();
        for name in names {
            let stem = name.strip_suffix(".md").expect("suffix checked");
            let id = AgentId::new(stem).map_err(|_| ConfigError::AgentFilename(name.clone()))?;
            let bytes = read_required_file(&directory, &name, MAX_AGENT_BYTES)?;
            let document = parse_agent(id.clone(), &bytes, self.source, &directory.join(&name))?;
            if documents.insert(id.clone(), document).is_some() {
                return Err(ConfigError::DuplicateAgent(id));
            }
        }
        Ok(documents)
    }
}

fn decode_runtime_layer(
    value: &mut toml::Value,
    text: &str,
    path: &Path,
) -> Result<RawRuntimeLayer, ConfigError> {
    const ALLOWED: &[&str] = &[
        "server",
        "tool_output",
        "approval",
        "context_compaction",
        "prompt_caching",
        "session_title",
        "delegation",
        "pricing",
        "mcp",
        "plugins",
        "providers",
    ];
    let table = value.as_table_mut().ok_or_else(|| {
        ConfigError::Toml(format!("{}: top level must be a table", path.display()))
    })?;
    if table.contains_key("schema_version") {
        return Err(ConfigError::ConfigSchemaRemoved {
            path: path.to_owned(),
            line: key_line(text, "schema_version").unwrap_or(1),
        });
    }
    if let Some(key) = table.keys().find(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(ConfigError::Toml(format!(
            "{} line {}: unknown top-level field `{key}`; remove it",
            path.display(),
            key_line(text, key).unwrap_or(1)
        )));
    }
    if table
        .get("context_compaction")
        .and_then(toml::Value::as_table)
        .is_some_and(|context| {
            context.contains_key("trigger") && context.contains_key("buffer_tokens")
        })
    {
        return Err(ConfigError::Toml(format!(
            "{}: context_compaction.trigger and context_compaction.buffer_tokens cannot both be set",
            path.display()
        )));
    }
    let providers = match table.remove("providers") {
        None => SensitiveProviderValues::new(),
        Some(toml::Value::Table(values)) => values
            .into_iter()
            .map(|(id, value)| {
                let value = SensitiveTomlValue::new(value);
                ProviderId::new(id).map(|id| (id, value)).map_err(|_| {
                    ConfigError::Toml(format!("{}: invalid provider ID", path.display()))
                })
            })
            .collect::<Result<_, _>>()?,
        Some(mut other) => {
            zeroize_toml_value(&mut other);
            return Err(ConfigError::Toml(format!(
                "{}: `providers` must be a table",
                path.display()
            )));
        }
    };
    let owned = std::mem::replace(value, toml::Value::Table(Default::default()));
    let json = SensitiveJsonValue::from_toml(owned);
    let mut layer = RawRuntimeLayer::deserialize(json.value())
        .map_err(|error| config_decode_error(path, Some(text), &error.to_string()))?;
    layer.providers = providers;
    Ok(layer)
}

fn config_decode_error(path: &Path, text: Option<&str>, message: &str) -> ConfigError {
    let field = extract_serde_field(message, "unknown field")
        .map(|field| (field, "unknown field"))
        .or_else(|| {
            extract_serde_field(message, "missing field").map(|field| (field, "missing field"))
        })
        .or_else(|| {
            extract_serde_field(message, "invalid field").map(|field| (field, "invalid field"))
        });
    let detail = field.as_ref().map_or_else(
        || "malformed configuration content".to_owned(),
        |(field, kind)| {
            let line = text.and_then(|text| key_line(text, field));
            let advice = match *kind {
                "unknown field" => "remove it",
                "missing field" => "add it",
                "invalid field" => "fix it",
                _ => unreachable!("known configuration diagnostic kind"),
            };
            match line {
                Some(line) => format!("line {line}: {kind} `{field}`; {advice}"),
                None => format!("{kind} `{field}`; {advice}"),
            }
        },
    );
    ConfigError::Toml(format!("{}: {detail}", path.display()))
}

fn extract_serde_field(message: &str, marker: &str) -> Option<String> {
    let rest = message.split_once(marker)?.1.trim_start();
    let quote = rest.chars().next()?;
    if !matches!(quote, '`' | '\'' | '"') {
        return None;
    }
    rest[quote.len_utf8()..]
        .split_once(quote)
        .map(|(field, _)| field.to_owned())
}

fn key_line(text: &str, key: &str) -> Option<usize> {
    text.lines()
        .position(|line| {
            line.split_once('=')
                .is_some_and(|(candidate, _)| candidate.trim().trim_matches('"') == key)
        })
        .map(|index| index + 1)
}

fn user_root() -> Option<PathBuf> {
    paths::user_data_root().ok()
}
