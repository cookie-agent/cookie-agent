use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
};

use cookie_agent_identity::AgentId;
use cookie_agent_identity::ProviderId;
use cookie_agent_models::ProviderDefinition;
use serde::Deserialize as _;

use crate::{
    AgentDocument, AgentDocumentSource, AgentRegistry, ApprovalConfig, ConfigError,
    ConfigSchemaVersion, ContextCompactionConfig, RuntimeConfig, ServerConfig, SessionTitleConfig,
    ToolOutputConfig,
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
    pub provider_provenance: BTreeMap<ProviderId, ProviderProvenance>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigLayer {
    User,
    Workspace,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderProvenance {
    pub layer: ConfigLayer,
    pub source_path: PathBuf,
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
    load_from_roots(user.as_deref(), Some(&cwd.join(".cookie-agent")))
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
        schema_version: ConfigSchemaVersion,
        server: ServerConfig::default(),
        tool_output: ToolOutputConfig::default(),
        approval: ApprovalConfig::default(),
        context_compaction: ContextCompactionConfig::default(),
        session_title: SessionTitleConfig::default(),
        providers: BTreeMap::new(),
    };
    let mut agents = BTreeMap::new();
    let mut provider_provenance = BTreeMap::new();
    let mut provider_values = SensitiveProviderValues::new();
    for root in [user.as_ref(), workspace.as_ref()].into_iter().flatten() {
        if let Some(mut layer) = root.load_runtime()? {
            let config_layer = match root.source {
                AgentDocumentSource::User => ConfigLayer::User,
                AgentDocumentSource::Workspace => ConfigLayer::Workspace,
                AgentDocumentSource::BuiltIn => {
                    unreachable!("built-in agents are not config roots")
                }
            };
            apply_settings(&mut runtime, &layer);
            for (id, value) in std::mem::take(&mut layer.providers) {
                let provenance = ProviderProvenance {
                    layer: config_layer,
                    source_path: root.path.join("config.toml"),
                };
                provider_provenance.insert(id.clone(), provenance);
                provider_values.insert(id, value);
            }
        }
        for (id, document) in root.load_agents()? {
            agents.insert(id, document);
        }
    }
    for (id, mut value) in provider_values {
        interpolate_provider_values(
            value.value_mut(),
            &mut vec!["providers".to_owned(), id.as_str().to_owned()],
        )?;
        let json = SensitiveJsonValue::from_toml(value.take());
        let provider = ProviderDefinition::deserialize(json.value())
            .map_err(|_| ConfigError::Toml("configuration TOML is invalid".to_owned()))?;
        runtime.providers.insert(id, provider);
    }
    AgentRegistry::validate_ref(&agents)?;
    validate_runtime(&runtime)?;
    Ok(LoadedConfiguration {
        runtime,
        agents,
        provider_provenance,
    })
}

impl LayerRoot {
    fn load_runtime(&self) -> Result<Option<RawRuntimeLayer>, ConfigError> {
        let Some(bytes) = read_optional_file(
            &self.directory,
            "config.toml",
            MAX_CONFIG_BYTES,
            self.source,
        )?
        else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| ConfigError::Utf8("config.toml"))?;
        let value = text
            .parse::<toml::Value>()
            .map_err(|error| ConfigError::Toml(safe_toml_error(&error)))?;
        let mut value = SensitiveTomlValue::new(value);
        validate_toml_value(value.value())?;
        let layer = decode_runtime_layer(value.value_mut())?;
        Ok(Some(layer))
    }

    fn load_agents(&self) -> Result<BTreeMap<AgentId, AgentDocument>, ConfigError> {
        let Some(directory) = open_optional_directory(&self.directory, "agents", self.source)?
        else {
            return Ok(BTreeMap::new());
        };
        let mut names = Vec::new();
        let mut stream = rustix::fs::Dir::read_from(&directory)
            .map_err(|error| ConfigError::Io(error.into()))?;
        for entry in &mut stream {
            let entry = entry.map_err(|error| ConfigError::Io(error.into()))?;
            let bytes = entry.file_name().to_bytes();
            if matches!(bytes, b"." | b"..") {
                continue;
            }
            let name = std::str::from_utf8(bytes)
                .map_err(|_| ConfigError::UnsafePath)?
                .to_owned();
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
            let bytes = read_required_file(&directory, &name, MAX_AGENT_BYTES, self.source)?;
            let document = parse_agent(id.clone(), &bytes, self.source)?;
            if documents.insert(id.clone(), document).is_some() {
                return Err(ConfigError::DuplicateAgent(id));
            }
        }
        Ok(documents)
    }
}

fn decode_runtime_layer(value: &mut toml::Value) -> Result<RawRuntimeLayer, ConfigError> {
    const ALLOWED: &[&str] = &[
        "schema_version",
        "server",
        "tool_output",
        "approval",
        "context_compaction",
        "session_title",
        "providers",
    ];
    let table = value
        .as_table_mut()
        .ok_or_else(|| ConfigError::Toml("configuration TOML is invalid".to_owned()))?;
    if table.keys().any(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(ConfigError::Toml(
            "configuration TOML is invalid".to_owned(),
        ));
    }
    let providers = match table.remove("providers") {
        None => SensitiveProviderValues::new(),
        Some(toml::Value::Table(values)) => values
            .into_iter()
            .map(|(id, value)| {
                let value = SensitiveTomlValue::new(value);
                ProviderId::new(id)
                    .map(|id| (id, value))
                    .map_err(|_| ConfigError::Toml("configuration TOML is invalid".to_owned()))
            })
            .collect::<Result<_, _>>()?,
        Some(mut other) => {
            zeroize_toml_value(&mut other);
            return Err(ConfigError::Toml(
                "configuration TOML is invalid".to_owned(),
            ));
        }
    };
    let owned = std::mem::replace(value, toml::Value::Table(Default::default()));
    let json = SensitiveJsonValue::from_toml(owned);
    let mut layer = RawRuntimeLayer::deserialize(json.value())
        .map_err(|_| ConfigError::Toml("configuration TOML is invalid".to_owned()))?;
    layer.providers = providers;
    Ok(layer)
}

fn user_root() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config/cookie_agent"))
}
