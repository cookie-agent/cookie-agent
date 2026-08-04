use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
};

use cookie_agent_identity::AgentId;
use cookie_agent_models::ModelSet;

use crate::{
    AgentDocument, AgentDocumentSource, AgentRegistry, ApprovalConfig, ConfigError,
    ConfigSchemaVersion, ContextCompactionConfig, RuntimeConfig, ServerConfig, SessionTitleConfig,
    ToolOutputConfig,
    agent_document::parse_agent,
    runtime::{RuntimeLayer, apply_layer, validate_runtime},
    secure_fs::{
        LayerRoot, open_layer_root, open_optional_directory, read_optional_file,
        read_required_file, regular_file_at,
    },
    toml_values::{interpolate_provider_values, reject_toml_datetime, safe_toml_error},
};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_AGENT_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug)]
pub struct LoadedConfiguration {
    pub runtime: RuntimeConfig,
    pub agents: BTreeMap<AgentId, AgentDocument>,
}

impl LoadedConfiguration {
    pub fn resolve_agents(&self, models: &ModelSet) -> Result<AgentRegistry, ConfigError> {
        AgentRegistry::resolve(self.agents.clone(), models)
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
    for root in [user.as_ref(), workspace.as_ref()].into_iter().flatten() {
        if let Some(layer) = root.load_runtime()? {
            apply_layer(&mut runtime, layer);
        }
        for (id, document) in root.load_agents()? {
            agents.insert(id, document);
        }
    }
    validate_runtime(&runtime)?;
    Ok(LoadedConfiguration { runtime, agents })
}

impl LayerRoot {
    fn load_runtime(&self) -> Result<Option<RuntimeLayer>, ConfigError> {
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
        reject_toml_datetime(text)?;
        let mut value = text
            .parse::<toml::Value>()
            .map_err(|error| ConfigError::Toml(safe_toml_error(&error)))?;
        interpolate_provider_values(&mut value, &mut Vec::new())?;
        let layer: RuntimeLayer = value
            .try_into()
            .map_err(|error| ConfigError::Toml(safe_toml_error(&error)))?;
        if layer.providers.is_empty() {
            return Err(ConfigError::EmptyProviders);
        }
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

fn user_root() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config/cookie_agent"))
}
