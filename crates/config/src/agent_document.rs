use cookie_agent_identity::AgentId;
use cookie_agent_models::Sha256Digest;
pub use cookie_agent_protocol::AgentDocumentSource;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::path::Path;

use crate::{AgentFrontmatter, ConfigError};

const MAX_FRONTMATTER_BYTES: usize = 128 * 1024;
const MAX_BODY_BYTES: usize = 128 * 1024;
const MAX_LIST: usize = 256;

#[derive(Clone, Debug)]
pub struct AgentDocument {
    pub id: AgentId,
    pub frontmatter: AgentFrontmatter,
    pub body: String,
    pub source: AgentDocumentSource,
    /// An official agent, kept by the daemon in `official_agents/`
    /// (`docs/site/specs/machines-on-demand.md` section 2.1). Its `source`
    /// is [`AgentDocumentSource::BuiltIn`]: cookie wrote it, not the user.
    pub official: bool,
    /// An official agent's version, from the daemon's record of the
    /// official agents it keeps.
    pub official_version: Option<String>,
    pub document_fingerprint: Sha256Digest,
    pub prompt_fingerprint: Sha256Digest,
}

/// What a document of the user's whose body is only `{{default_prompt}}`
/// says while the official agent it extends has not been fetched.
const AWAITING_OFFICIAL_PROMPT: &str =
    "This agent extends an official agent whose prompt the daemon has not fetched yet.\n";

impl AgentDocument {
    /// Puts `official`'s prompt in place of the body's `{{default_prompt}}`:
    /// the official agent's, or for a preset's document the top-level one it
    /// replaces. The fingerprints follow that document too, so a refreshed
    /// official prompt reads as a changed agent.
    pub(crate) fn inherit_prompt(&mut self, official: &AgentDocument) {
        let body = crate::prompt_template::replace_default_prompt(
            &self.body,
            official.body.trim_end_matches('\n'),
        );
        self.document_fingerprint = hash(
            "cookie-agent/agent-document/inherited/v1",
            &(
                self.document_fingerprint.as_str(),
                official.document_fingerprint.as_str(),
            ),
        );
        self.set_body(body);
    }

    /// Drops the body's `{{default_prompt}}`, which nothing stands for; a
    /// body left blank says the prompt it awaits.
    pub(crate) fn drop_default_prompt(&mut self) {
        let mut body = crate::prompt_template::replace_default_prompt(&self.body, "")
            .trim_start()
            .to_owned();
        if body.is_empty() {
            AWAITING_OFFICIAL_PROMPT.clone_into(&mut body);
        }
        self.document_fingerprint = hash(
            "cookie-agent/agent-document/without-default-prompt/v1",
            &self.document_fingerprint.as_str(),
        );
        self.set_body(body);
    }

    fn set_body(&mut self, body: String) {
        self.body = format!("{}\n", body.trim_end_matches('\n'));
        self.prompt_fingerprint = hash("cookie-agent/system-prompt/v1", &self.body);
    }
}

/// `bytes`, an agent document, split into its frontmatter's YAML and its
/// body, both checked against the document limits.
pub(crate) fn split_document(bytes: &[u8], path: &Path) -> Result<(String, String), ConfigError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid_document(path, "content is not UTF-8"))?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if text.contains("${env:") {
        return Err(invalid_document(
            path,
            "environment interpolation is not allowed",
        ));
    }
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| invalid_document(path, "frontmatter must start with `---`"))?;
    let closing = rest
        .find("\n---\n")
        .or_else(|| rest.ends_with("\n---").then(|| rest.len() - "\n---".len()))
        .ok_or_else(|| invalid_document(path, "frontmatter must end with `---`"))?;
    let yaml = &rest[..closing];
    let raw_body = rest.get(closing + "\n---\n".len()..).unwrap_or_default();
    if yaml.len() > MAX_FRONTMATTER_BYTES || raw_body.len() > MAX_BODY_BYTES || forbidden_yaml(yaml)
    {
        return Err(invalid_document(
            path,
            "frontmatter is too large or uses unsupported YAML features",
        ));
    }
    Ok((yaml.to_owned(), raw_body.to_owned()))
}

/// The frontmatter YAML of a document, parsed.
pub(crate) fn parse_yaml(yaml: &str, path: &Path) -> Result<serde_yaml::Value, ConfigError> {
    serde_yaml::from_str(yaml).map_err(|error| yaml_error(path, &error))
}

pub(crate) fn parse_agent(
    id: AgentId,
    bytes: &[u8],
    source: AgentDocumentSource,
    path: &Path,
) -> Result<AgentDocument, ConfigError> {
    let (yaml, raw_body) = split_document(bytes, path)?;
    let (yaml, raw_body) = (yaml.as_str(), raw_body.as_str());
    let yaml_value = parse_yaml(yaml, path)?;
    validate_yaml_limits(&yaml_value, 0)
        .map_err(|_| invalid_document(path, "YAML resource limit exceeded"))?;
    reject_schema_field(&yaml_value, yaml, path)?;
    reject_renamed_models_field(&yaml_value, &id)?;
    reject_removed_tools_field(&yaml_value, &id)?;
    validate_permission_expressions(&yaml_value, &id)?;
    let frontmatter = parse_frontmatter(yaml, yaml_value, path)?;
    let mut body = format!("{}\n", raw_body.trim_end_matches('\n'));
    if !body.chars().any(|character| !character.is_whitespace()) {
        // An ACP agent's body is sent with its session's first prompt, and
        // an empty one sends nothing (`docs/site/specs/acp-agents.md`
        // section 4).
        if frontmatter.acp.is_none() {
            return Err(ConfigError::EmptyPrompt(id));
        }
        body.clear();
    }
    let document_fingerprint = hash(
        "cookie-agent/agent-document/v1",
        &(id.as_str(), yaml, &body),
    );
    let prompt_fingerprint = hash("cookie-agent/system-prompt/v1", &body);
    Ok(AgentDocument {
        id,
        frontmatter,
        body,
        source,
        official: false,
        official_version: None,
        document_fingerprint,
        prompt_fingerprint,
    })
}

/// The frontmatter of `yaml`. An ACP agent's document (one with an `acp`
/// block) names the agent's own model and effort values in `models`, not
/// cookie's model keys: those are read into `acp.models`.
fn parse_frontmatter(
    yaml: &str,
    mut value: serde_yaml::Value,
    path: &Path,
) -> Result<AgentFrontmatter, ConfigError> {
    let acp = serde_yaml::Value::String("acp".to_owned());
    let models = serde_yaml::Value::String("models".to_owned());
    let Some(mapping) = value
        .as_mapping_mut()
        .filter(|mapping| mapping.get(&acp).is_some_and(|acp| !acp.is_null()))
    else {
        return serde_yaml::from_str(yaml).map_err(|error| yaml_error(path, &error));
    };
    let choices = match mapping.remove(&models) {
        Some(models) => serde_yaml::from_value::<Vec<crate::AcpModelChoice>>(models)
            .map_err(|error| yaml_error(path, &error))?,
        None => Vec::new(),
    };
    let mut frontmatter: AgentFrontmatter =
        serde_yaml::from_value(value).map_err(|error| yaml_error(path, &error))?;
    if let Some(settings) = &mut frontmatter.acp {
        settings.models = choices;
    }
    Ok(frontmatter)
}

fn reject_schema_field(
    value: &serde_yaml::Value,
    yaml: &str,
    path: &Path,
) -> Result<(), ConfigError> {
    let schema = serde_yaml::Value::String("schema".to_owned());
    if value
        .as_mapping()
        .is_some_and(|frontmatter| frontmatter.contains_key(&schema))
    {
        let line = yaml
            .lines()
            .position(|line| line.trim_start().starts_with("schema:"))
            .map_or(2, |index| index + 2);
        return Err(ConfigError::AgentSchemaRemoved {
            path: path.to_owned(),
            line,
        });
    }
    Ok(())
}

pub(crate) fn invalid_document(path: &Path, message: impl Into<String>) -> ConfigError {
    ConfigError::AgentDocument {
        path: path.to_owned(),
        message: message.into(),
    }
}

fn yaml_error(path: &Path, error: &serde_yaml::Error) -> ConfigError {
    let location = error
        .location()
        .map(|location| {
            format!(
                " at line {}, column {}",
                location.line() + 1,
                location.column()
            )
        })
        .unwrap_or_default();
    let rendered = error.to_string();
    let message = extract_serde_field(&rendered, "unknown field")
        .map(|field| format!("unknown field `{field}`{location}; remove it"))
        .or_else(|| {
            extract_serde_field(&rendered, "missing field")
                .map(|field| format!("missing required field `{field}`{location}"))
        })
        .unwrap_or_else(|| format!("malformed YAML frontmatter{location}"));
    invalid_document(path, message)
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

fn reject_removed_tools_field(
    value: &serde_yaml::Value,
    agent: &AgentId,
) -> Result<(), ConfigError> {
    let tools = serde_yaml::Value::String("tools".to_owned());
    if value
        .as_mapping()
        .is_some_and(|frontmatter| frontmatter.contains_key(&tools))
    {
        return Err(ConfigError::AgentToolsRemoved(agent.clone()));
    }
    Ok(())
}

fn reject_renamed_models_field(
    value: &serde_yaml::Value,
    agent: &AgentId,
) -> Result<(), ConfigError> {
    let legacy = serde_yaml::Value::String("model_fallback".to_owned());
    if value
        .as_mapping()
        .is_some_and(|frontmatter| frontmatter.contains_key(&legacy))
    {
        return Err(ConfigError::AgentModelsRenamed(agent.clone()));
    }
    Ok(())
}

fn validate_permission_expressions(
    value: &serde_yaml::Value,
    agent: &AgentId,
) -> Result<(), ConfigError> {
    let Some(frontmatter) = value.as_mapping() else {
        return Ok(());
    };
    let permissions_key = serde_yaml::Value::String("permissions".to_owned());
    let Some(permissions) = frontmatter
        .get(&permissions_key)
        .and_then(serde_yaml::Value::as_mapping)
    else {
        return Ok(());
    };
    for (action, permission) in permissions {
        let Some(action) = action.as_str() else {
            continue;
        };
        let action = action.split_once('@').map_or(action, |(action, _)| action);
        let Some(resources) = permission.as_mapping() else {
            continue;
        };
        for resource in resources.keys().filter_map(serde_yaml::Value::as_str) {
            if !permission_expressions_valid(action, resource) {
                return Err(ConfigError::AgentPermissionExpression(agent.clone()));
            }
        }
    }
    Ok(())
}

/// Whether the `${…}` expressions of a rule's `resource` suit its `action`:
/// `${workspace_dir}` for `read` and `write`, and the memory variables
/// (`docs/site/specs/memory.md` section 16.9.9) for `memory` and
/// `memory_write`, each a whole URL segment, after a `/` and before a `/`
/// or the end. Any other `${` is malformed.
pub(crate) fn permission_expressions_valid(action: &str, resource: &str) -> bool {
    let mut rest = resource;
    while let Some(start) = rest.find("${") {
        let expression = &rest[start..];
        let Some(end) = expression.find('}') else {
            return false;
        };
        let (variable, after) = expression.split_at(end + 1);
        let valid = if variable == cookie_agent_identity::WildcardPattern::WORKSPACE_DIR_EXPRESSION
        {
            matches!(action, "read" | "write")
        } else if cookie_agent_identity::WildcardPattern::MEMORY_VARIABLES.contains(&variable) {
            matches!(action, "memory" | "memory_write")
                && rest[..start].ends_with('/')
                && (after.is_empty() || after.starts_with('/'))
        } else {
            false
        };
        if !valid {
            return false;
        }
        rest = after;
    }
    true
}

fn forbidden_yaml(yaml: &str) -> bool {
    yaml.lines().any(|line| {
        if line.trim_start().starts_with("<<:") {
            return true;
        }
        let mut single = false;
        let mut double = false;
        let mut escaped = false;
        for (index, character) in line.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if double && character == '\\' {
                escaped = true;
                continue;
            }
            match character {
                '\'' if !double => single = !single,
                '"' if !single => double = !double,
                '#' if !single && !double => break,
                '&' | '!' | '*' if !single && !double => {
                    let boundary = index == 0
                        || line[..index]
                            .chars()
                            .next_back()
                            .is_some_and(char::is_whitespace);
                    if boundary {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    })
}

fn validate_yaml_limits(value: &serde_yaml::Value, depth: usize) -> Result<(), ConfigError> {
    if depth > 16 {
        return Err(ConfigError::AgentYamlLimit);
    }
    match value {
        serde_yaml::Value::Sequence(values) => {
            if values.len() > MAX_LIST {
                return Err(ConfigError::AgentYamlLimit);
            }
            for value in values {
                validate_yaml_limits(value, depth + 1)?;
            }
        }
        serde_yaml::Value::Mapping(values) => {
            if values.len() > MAX_LIST {
                return Err(ConfigError::AgentYamlLimit);
            }
            for (key, value) in values {
                validate_yaml_limits(key, depth + 1)?;
                validate_yaml_limits(value, depth + 1)?;
            }
        }
        serde_yaml::Value::Tagged(_) => return Err(ConfigError::AgentYamlLimit),
        serde_yaml::Value::String(value) if value.len() > 128 * 1024 => {
            return Err(ConfigError::AgentYamlLimit);
        }
        _ => {}
    }
    Ok(())
}

fn hash(domain: &str, value: &impl Serialize) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(value).expect("strings always serialize to JSON"));
    Sha256Digest::new(format!("{:x}", hasher.finalize()))
        .expect("SHA-256 output is always a valid digest")
}
