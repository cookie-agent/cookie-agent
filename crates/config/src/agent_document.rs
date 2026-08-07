use cookie_agent_identity::AgentId;
use cookie_agent_models::Sha256Digest;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::{AgentFrontmatter, ConfigError};

const MAX_FRONTMATTER_BYTES: usize = 128 * 1024;
const MAX_BODY_BYTES: usize = 128 * 1024;
const MAX_LIST: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDocumentSource {
    BuiltIn,
    User,
    Workspace,
}

#[derive(Clone, Debug)]
pub struct AgentDocument {
    pub id: AgentId,
    pub frontmatter: AgentFrontmatter,
    pub body: String,
    pub source: AgentDocumentSource,
    pub document_fingerprint: Sha256Digest,
    pub prompt_fingerprint: Sha256Digest,
}

pub(crate) fn parse_agent(
    id: AgentId,
    bytes: &[u8],
    source: AgentDocumentSource,
) -> Result<AgentDocument, ConfigError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ConfigError::Utf8("agent document"))?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if text.contains("${env:") {
        return Err(ConfigError::AgentFrontmatter(id));
    }
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| ConfigError::AgentFrontmatter(id.clone()))?;
    let closing = rest
        .find("\n---\n")
        .ok_or_else(|| ConfigError::AgentFrontmatter(id.clone()))?;
    let yaml = &rest[..closing];
    let raw_body = &rest[closing + "\n---\n".len()..];
    if yaml.len() > MAX_FRONTMATTER_BYTES || raw_body.len() > MAX_BODY_BYTES || forbidden_yaml(yaml)
    {
        return Err(ConfigError::AgentFrontmatter(id));
    }
    let yaml_value: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|_| ConfigError::AgentFrontmatter(id.clone()))?;
    validate_yaml_limits(&yaml_value, 0)?;
    validate_permission_expressions(&yaml_value, &id)?;
    let frontmatter: AgentFrontmatter = serde_yaml::from_value(yaml_value)
        .map_err(|_| ConfigError::AgentFrontmatter(id.clone()))?;
    let body = format!("{}\n", raw_body.trim_end_matches('\n'));
    if !body.chars().any(|character| !character.is_whitespace()) {
        return Err(ConfigError::EmptyPrompt(id));
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
        document_fingerprint,
        prompt_fingerprint,
    })
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
        let Some(resources) = permission.as_mapping() else {
            continue;
        };
        for resource in resources.keys().filter_map(serde_yaml::Value::as_str) {
            let remainder = resource.replace("${workspace_dir}", "");
            let malformed_expression = remainder.contains("${");
            let unsupported_action = resource.contains("${workspace_dir}")
                && !matches!(action, "read" | "write" | "external_directory");
            if malformed_expression || unsupported_action {
                return Err(ConfigError::AgentPermissionExpression(agent.clone()));
            }
        }
    }
    Ok(())
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
