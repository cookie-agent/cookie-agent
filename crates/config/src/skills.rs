use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use cookie_agent_protocol::{ModelKey, PermissionAction, SkillSource, WildcardPattern, paths};
use serde::{Deserialize, Serialize};

use crate::ConfigError;

const MAX_SKILL_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillContext {
    Fork,
}

#[derive(Clone, Debug)]
pub struct SkillAllowedTool {
    pub action: PermissionAction,
    pub pattern: WildcardPattern,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    #[serde(default, rename = "when_to_use")]
    pub when_to_use: Option<String>,
    #[serde(default, deserialize_with = "deserialize_allowed_tools")]
    pub allowed_tools: Vec<SkillAllowedTool>,
    #[serde(default)]
    pub disable_model_invocation: bool,
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    #[serde(default, deserialize_with = "deserialize_model")]
    pub model: Option<ModelKey>,
    #[serde(default)]
    pub context: Option<SkillContext>,
    #[serde(default)]
    pub argument_hint: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub compatibility: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_yaml::Value>,
}

const fn default_true() -> bool {
    true
}

fn deserialize_allowed_tools<'de, D>(deserializer: D) -> Result<Vec<SkillAllowedTool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    parse_allowed_tools(&value).map_err(serde::de::Error::custom)
}

fn deserialize_model<'de, D>(deserializer: D) -> Result<Option<ModelKey>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| {
            value
                .parse::<ModelKey>()
                .map_err(|_| serde::de::Error::custom(format!("invalid skill model `{value}`")))
        })
        .transpose()
}

fn parse_allowed_tools(value: &str) -> Result<Vec<SkillAllowedTool>, String> {
    if value.is_empty() || value.trim().is_empty() {
        return Err("allowed-tools contains an empty token".into());
    }
    let mut parsed = Vec::new();
    let mut offset = 0;
    while offset < value.len() {
        while value[offset..].starts_with(char::is_whitespace) {
            offset += value[offset..]
                .chars()
                .next()
                .expect("offset is within string")
                .len_utf8();
            if offset == value.len() {
                break;
            }
        }
        if offset == value.len() {
            break;
        }
        let start = offset;
        let name_end = value[offset..]
            .char_indices()
            .find_map(|(index, character)| {
                (character == '(' || character.is_whitespace()).then_some(offset + index)
            })
            .unwrap_or(value.len());
        if name_end == start {
            return Err(format!(
                "malformed allowed-tools token near `{}`: tool name is empty",
                &value[start..]
            ));
        }
        let name = &value[start..name_end];
        offset = name_end;
        let pattern = if offset < value.len() && value[offset..].starts_with('(') {
            offset += 1;
            let pattern_start = offset;
            let mut depth = 1_u32;
            let mut closing = None;
            for (index, character) in value[offset..].char_indices() {
                match character {
                    '(' => depth = depth.saturating_add(1),
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            closing = Some(offset + index);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let closing = closing.ok_or_else(|| {
                format!("malformed allowed-tools token `{name}`: unbalanced parentheses")
            })?;
            if closing == pattern_start {
                return Err(format!(
                    "malformed allowed-tools token `{name}()`: pattern is empty"
                ));
            }
            offset = closing + 1;
            if offset < value.len()
                && value[offset..]
                    .chars()
                    .next()
                    .is_some_and(|character| !character.is_whitespace())
            {
                return Err(format!(
                    "malformed allowed-tools token `{}`: expected whitespace after final `)`",
                    &value[start..]
                ));
            }
            &value[pattern_start..closing]
        } else {
            "*"
        };
        let action = tool_action(name).ok_or_else(|| {
            format!(
                "unknown allowed-tools tool `{name}`; valid values are Read, Write, Edit, Bash, Delegate, Skill, Mcp, and Plugin"
            )
        })?;
        if let Some(prefix) = pattern.strip_suffix(":*") {
            if prefix.is_empty() {
                return Err(format!(
                    "malformed allowed-tools token `{name}({pattern})`: prefix is empty"
                ));
            }
            for expanded in [prefix.to_owned(), format!("{prefix} *")] {
                let pattern = WildcardPattern::new(expanded).map_err(|error| {
                    format!("invalid allowed-tools prefix for `{name}`: {error}")
                })?;
                parsed.push(SkillAllowedTool { action, pattern });
            }
        } else {
            let pattern = WildcardPattern::new(pattern)
                .map_err(|error| format!("invalid allowed-tools pattern for `{name}`: {error}"))?;
            parsed.push(SkillAllowedTool { action, pattern });
        }
    }
    if parsed.is_empty() {
        return Err("allowed-tools contains an empty token".into());
    }
    Ok(parsed)
}

fn tool_action(name: &str) -> Option<PermissionAction> {
    if name.eq_ignore_ascii_case("read") {
        Some(PermissionAction::Read)
    } else if name.eq_ignore_ascii_case("write") || name.eq_ignore_ascii_case("edit") {
        Some(PermissionAction::Write)
    } else if name.eq_ignore_ascii_case("bash") {
        Some(PermissionAction::Bash)
    } else if name.eq_ignore_ascii_case("delegate") {
        Some(PermissionAction::Delegate)
    } else if name.eq_ignore_ascii_case("skill") {
        Some(PermissionAction::Skill)
    } else if name.eq_ignore_ascii_case("mcp") {
        Some(PermissionAction::Mcp)
    } else if name.eq_ignore_ascii_case("plugin") {
        Some(PermissionAction::Plugin)
    } else {
        None
    }
}

#[derive(Clone, Debug)]
pub struct SkillDocument {
    pub frontmatter: SkillFrontmatter,
    pub body: String,
    pub source: SkillSource,
    pub path: PathBuf,
    pub base_dir: PathBuf,
    pub supporting_files: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct SkillDiscovery {
    pub name: String,
    pub source: SkillSource,
    pub path: PathBuf,
    pub precedence_winner: bool,
}

#[derive(Clone, Debug)]
pub struct SkillDiagnostic {
    pub name: String,
    pub message: String,
    pub shadowed_path: PathBuf,
    pub winner_path: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, SkillDocument>,
    discoveries: Vec<SkillDiscovery>,
    diagnostics: Vec<SkillDiagnostic>,
}

impl SkillRegistry {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&SkillDocument> {
        self.skills.get(name)
    }

    pub fn skills(&self) -> impl ExactSizeIterator<Item = (&String, &SkillDocument)> {
        self.skills.iter()
    }

    #[must_use]
    pub fn discoveries(&self) -> &[SkillDiscovery] {
        &self.discoveries
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// Loads skills once for engine construction. Skill files are intentionally not hot-reloaded.
pub fn load_skills(cwd: &Path) -> Result<SkillRegistry, ConfigError> {
    let cwd = cwd.canonicalize().map_err(ConfigError::Io)?;
    let shared_user = paths::home_dir()
        .ok()
        .map(|home| home.join(".agents/skills"));
    let user = user_skill_root();
    let project = project_skill_roots(&cwd);
    load_skill_roots(shared_user.as_deref(), user.as_deref(), &project)
}

/// Loads explicit skill roots in low-to-high project precedence order.
///
/// `shared_user_root` is the cross-client `~/.agents/skills` convention and ranks below the
/// cookie-agent-native user root. Each project directory contributes its `.agents/skills` root
/// before its `.cookie-agent/skills` root, so native roots win same-scope name collisions.
pub fn load_skill_roots(
    shared_user_root: Option<&Path>,
    user_root: Option<&Path>,
    project_roots: &[PathBuf],
) -> Result<SkillRegistry, ConfigError> {
    let mut candidates = Vec::new();
    if let Some(root) = shared_user_root {
        candidates.extend(load_root(root, SkillSource::User)?);
    }
    if let Some(root) = user_root {
        candidates.extend(load_root(root, SkillSource::User)?);
    }
    for directory in project_roots {
        candidates.extend(load_root(
            &directory.join(".agents/skills"),
            SkillSource::Project,
        )?);
        candidates.extend(load_root(
            &directory.join(".cookie-agent/skills"),
            SkillSource::Project,
        )?);
    }

    let mut winner_index = BTreeMap::<String, usize>::new();
    for (index, document) in candidates.iter().enumerate() {
        winner_index.insert(document.frontmatter.name.clone(), index);
    }
    let mut registry = SkillRegistry::default();
    for (index, document) in candidates.into_iter().enumerate() {
        let name = document.frontmatter.name.clone();
        let precedence_winner = winner_index.get(&name) == Some(&index);
        registry.discoveries.push(SkillDiscovery {
            name: name.clone(),
            source: document.source,
            path: document.path.clone(),
            precedence_winner,
        });
        if precedence_winner {
            registry.skills.insert(name, document);
        }
    }
    for discovery in registry
        .discoveries
        .iter()
        .filter(|entry| entry.source == SkillSource::User && !entry.precedence_winner)
    {
        let winner = registry
            .skills
            .get(&discovery.name)
            .expect("shadowed skill has a winner");
        let winner_scope = match winner.source {
            SkillSource::User => "user",
            SkillSource::Project => "project",
        };
        registry.diagnostics.push(SkillDiagnostic {
            name: discovery.name.clone(),
            message: format!(
                "user skill `{}` is shadowed by {winner_scope} skill {}",
                discovery.name,
                winner.path.display()
            ),
            shadowed_path: discovery.path.clone(),
            winner_path: winner.path.clone(),
        });
    }
    registry.discoveries.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(registry)
}

fn user_skill_root() -> Option<PathBuf> {
    paths::user_data_root().ok().map(|root| root.join("skills"))
}

fn project_skill_roots(cwd: &Path) -> Vec<PathBuf> {
    let ancestors = cwd.ancestors().collect::<Vec<_>>();
    let stop = ancestors
        .iter()
        .position(|path| path.join(".git").exists())
        .unwrap_or(0);
    ancestors[..=stop]
        .iter()
        .rev()
        .map(|path| path.to_path_buf())
        .collect()
}

fn load_root(root: &Path, source: SkillSource) -> Result<Vec<SkillDocument>, ConfigError> {
    let metadata = match fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ConfigError::Io(error)),
    };
    if !metadata.is_dir() {
        return Err(ConfigError::UnsafePath);
    }
    let mut directories = fs::read_dir(root)
        .map_err(ConfigError::Io)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(ConfigError::Io)?;
    directories.sort_by_key(std::fs::DirEntry::file_name);
    let mut documents = Vec::new();
    for entry in directories {
        let file_type = entry.file_type().map_err(ConfigError::Io)?;
        if !file_type.is_dir() || file_type.is_symlink() {
            return Err(ConfigError::UnsafePath);
        }
        let directory_name =
            entry
                .file_name()
                .into_string()
                .map_err(|_| ConfigError::SkillName {
                    path: entry.path(),
                    name: "<non-UTF-8>".into(),
                })?;
        validate_name(&directory_name, &entry.path())?;
        let path = entry.path().join("SKILL.md");
        let metadata = fs::metadata(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ConfigError::SkillDocument {
                    path: path.clone(),
                    message: "required SKILL.md was not found".into(),
                }
            } else {
                ConfigError::Io(error)
            }
        })?;
        if !metadata.is_file() || metadata.len() > MAX_SKILL_BYTES {
            return Err(ConfigError::SkillDocument {
                path,
                message: "SKILL.md is not a regular file or exceeds 256 KiB".into(),
            });
        }
        let bytes = fs::read(&path).map_err(ConfigError::Io)?;
        documents.push(parse_skill(
            &directory_name,
            &bytes,
            source,
            &path,
            &entry.path(),
        )?);
    }
    Ok(documents)
}

fn parse_skill(
    directory_name: &str,
    bytes: &[u8],
    source: SkillSource,
    path: &Path,
    base_dir: &Path,
) -> Result<SkillDocument, ConfigError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| skill_document(path, "content is not UTF-8"))?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| skill_document(path, "frontmatter must start with `---`"))?;
    let closing = rest
        .find("\n---\n")
        .ok_or_else(|| skill_document(path, "frontmatter must end with `---`"))?;
    let yaml = &rest[..closing];
    let frontmatter: SkillFrontmatter = serde_yaml::from_str(yaml).map_err(|error| {
        let rendered = error.to_string();
        let message = if rendered.contains("unknown field") {
            format!("unknown frontmatter field: {rendered}")
        } else if rendered.contains("missing field") {
            format!("missing required frontmatter field: {rendered}")
        } else {
            format!("malformed YAML frontmatter: {rendered}")
        };
        skill_document(path, message)
    })?;
    validate_name(&frontmatter.name, path)?;
    if frontmatter.name != directory_name {
        return Err(ConfigError::SkillNameMismatch {
            path: path.to_owned(),
            directory: directory_name.to_owned(),
            frontmatter: frontmatter.name,
        });
    }
    if frontmatter.description.is_empty()
        || frontmatter.description.len() > 1024
        || frontmatter.description.chars().any(char::is_control)
    {
        return Err(skill_document(
            path,
            "description must be 1-1024 control-free UTF-8 bytes",
        ));
    }
    let body = format!(
        "{}\n",
        rest[closing + "\n---\n".len()..].trim_end_matches('\n')
    );
    if body.trim().is_empty() {
        return Err(skill_document(path, "skill body is empty"));
    }
    let supporting_files = supporting_file_sample(base_dir, path)?;
    Ok(SkillDocument {
        frontmatter,
        body,
        source,
        path: path.to_owned(),
        base_dir: base_dir.to_owned(),
        supporting_files,
    })
}

fn validate_name(name: &str, path: &Path) -> Result<(), ConfigError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
    if valid {
        Ok(())
    } else {
        Err(ConfigError::SkillName {
            path: path.to_owned(),
            name: name.to_owned(),
        })
    }
}

fn supporting_file_sample(base_dir: &Path, skill_path: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let mut pending = vec![base_dir.to_owned()];
    let mut files = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
            .map_err(ConfigError::Io)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(ConfigError::Io)?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let file_type = entry.file_type().map_err(ConfigError::Io)?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() && entry.path() != skill_path {
                files.insert(entry.path());
                if files.len() == 10 {
                    return Ok(files.into_iter().collect());
                }
            }
        }
    }
    Ok(files.into_iter().collect())
}

fn skill_document(path: &Path, message: impl Into<String>) -> ConfigError {
    ConfigError::SkillDocument {
        path: path.to_owned(),
        message: message.into(),
    }
}

pub fn render_available_skills<'a>(
    skills: impl IntoIterator<Item = &'a SkillDocument>,
    context_tokens: u64,
) -> Result<String, ConfigError> {
    let skills = skills.into_iter().collect::<Vec<_>>();
    if skills.is_empty() {
        return Ok(String::new());
    }
    let budget = usize::try_from(context_tokens / 100)
        .unwrap_or(usize::MAX)
        .saturating_mul(4);
    let render = |include_when: bool, descriptions: &[String]| {
        let mut output = String::from("<available_skills>");
        for (skill, description) in skills.iter().zip(descriptions) {
            output.push_str("<skill><name>");
            output.push_str(&escape_xml(&skill.frontmatter.name));
            output.push_str("</name><description>");
            output.push_str(&escape_xml(description));
            if include_when && let Some(when) = &skill.frontmatter.when_to_use {
                output.push(' ');
                output.push_str(&escape_xml(when));
            }
            output.push_str("</description><location>");
            output.push_str(&escape_xml(&skill.path.to_string_lossy()));
            output.push_str("</location></skill>");
        }
        output.push_str("</available_skills>");
        output
    };
    let mut descriptions = skills
        .iter()
        .map(|skill| skill.frontmatter.description.clone())
        .collect::<Vec<_>>();
    let full = render(true, &descriptions);
    if full.len() <= budget {
        return Ok(full);
    }
    let without_when = render(false, &descriptions);
    if without_when.len() <= budget {
        return Ok(without_when);
    }
    loop {
        let current = render(false, &descriptions);
        if current.len() <= budget {
            return Ok(current);
        }
        let Some((index, _)) = descriptions
            .iter()
            .enumerate()
            .filter(|(_, description)| !description.is_empty())
            .max_by_key(|(_, description)| description.len())
        else {
            return Err(ConfigError::SkillListingBudget);
        };
        descriptions[index].pop();
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cookie_agent_protocol::PermissionAction;

    use super::{load_skill_roots, parse_allowed_tools, render_available_skills, validate_name};
    use crate::simple_wildcard_match;

    #[test]
    fn validates_locked_skill_name_grammar() {
        for valid in ["a", "review-code", "skill9"] {
            assert!(validate_name(valid, std::path::Path::new(valid)).is_ok());
        }
        for invalid in ["", "UPPER", "two--parts", "-leading", "trailing-"] {
            assert!(validate_name(invalid, std::path::Path::new(invalid)).is_err());
        }
    }

    #[test]
    fn parses_agentskills_allowed_tools_format() {
        let parsed = parse_allowed_tools("Bash(git:*) Bash(jq:*) Read").expect("spec example");
        assert_eq!(parsed.len(), 5);
        assert_eq!(parsed[0].action, PermissionAction::Bash);
        assert_eq!(parsed[0].pattern.as_str(), "git");
        assert_eq!(parsed[1].pattern.as_str(), "git *");
        assert_eq!(parsed[2].pattern.as_str(), "jq");
        assert_eq!(parsed[3].pattern.as_str(), "jq *");
        assert_eq!(parsed[4].action, PermissionAction::Read);
        assert_eq!(parsed[4].pattern.as_str(), "*");
        for command in ["git", "git status", "git commit -m x"] {
            assert!(
                parsed[..2]
                    .iter()
                    .any(|grant| simple_wildcard_match(grant.pattern.as_str(), command)),
                "git prefix must grant {command:?}"
            );
        }
        assert!(
            !parsed[..2]
                .iter()
                .any(|grant| { simple_wildcard_match(grant.pattern.as_str(), "cargo test") })
        );

        let parsed = parse_allowed_tools(
            "read READ ReAd edit EDIT Write BASH(git commit -m *) McP(api(call *))",
        )
        .expect("case variants and patterns with spaces/parens");
        assert_eq!(parsed[0].action, PermissionAction::Read);
        assert_eq!(parsed[1].action, PermissionAction::Read);
        assert_eq!(parsed[2].action, PermissionAction::Read);
        assert_eq!(parsed[3].action, PermissionAction::Write);
        assert_eq!(parsed[4].action, PermissionAction::Write);
        assert_eq!(parsed[5].action, PermissionAction::Write);
        assert_eq!(parsed[6].action, PermissionAction::Bash);
        assert_eq!(parsed[6].pattern.as_str(), "git commit -m *");
        assert_eq!(parsed[7].action, PermissionAction::Mcp);
        assert_eq!(parsed[7].pattern.as_str(), "api(call *)");

        let passthrough =
            parse_allowed_tools("Write(src/**) Bash(git commit -m *)").expect("pass-through globs");
        assert_eq!(passthrough[0].pattern.as_str(), "src/**");
        assert_eq!(passthrough[1].pattern.as_str(), "git commit -m *");
    }

    #[test]
    fn rejects_unknown_and_malformed_allowed_tools_tokens() {
        let unknown = parse_allowed_tools("Unknown").unwrap_err();
        assert!(unknown.contains("unknown allowed-tools tool `Unknown`"));
        assert!(unknown.contains("Read, Write, Edit, Bash, Delegate, Skill, Mcp, and Plugin"));

        for malformed in ["", "Bash(", "Bash()", "Bash(:*)", "(git:*)", "Bash(git:*))"] {
            assert!(
                parse_allowed_tools(malformed).is_err(),
                "expected malformed token error for {malformed:?}"
            );
        }
    }

    #[test]
    fn invalid_model_key_is_a_targeted_load_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_skill(
            &temp.path().join(".cookie-agent/skills"),
            "model-test",
            "model-test",
            "model: not-a-model-key\n",
            "body",
        );
        let error = load_skill_roots(None, None, &[temp.path().to_owned()]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("invalid skill model `not-a-model-key`"),
            "{error}"
        );
    }

    fn write_skill(root: &std::path::Path, directory: &str, name: &str, extra: &str, body: &str) {
        let directory = root.join(directory);
        fs::create_dir_all(&directory).expect("skill directory");
        fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Test skill\n{extra}---\n{body}\n"),
        )
        .expect("skill document");
    }

    #[test]
    fn project_skill_shadows_user_and_records_diagnostic() {
        let temp = tempfile::tempdir().expect("tempdir");
        let user = temp.path().join("user");
        let project = temp.path().join("project");
        write_skill(&user, "review", "review", "", "user body");
        write_skill(
            &project.join(".cookie-agent/skills"),
            "review",
            "review",
            "",
            "project body",
        );
        let registry = load_skill_roots(None, Some(&user), &[project]).expect("registry");
        assert_eq!(
            registry.get("review").expect("winner").body,
            "project body\n"
        );
        assert_eq!(registry.diagnostics().len(), 1);
        assert_eq!(registry.discoveries().len(), 2);
    }

    #[test]
    fn precedence_runs_shared_user_then_user_then_project_shared_then_native() {
        let temp = tempfile::tempdir().expect("tempdir");
        let shared_user = temp.path().join("shared-user");
        let user = temp.path().join("user");
        let project = temp.path().join("project");
        // Distinct names per root all load.
        write_skill(
            &shared_user,
            "shared-only",
            "shared-only",
            "",
            "shared user body",
        );
        write_skill(&user, "user-only", "user-only", "", "user body");
        write_skill(
            &project.join(".agents/skills"),
            "project-shared-only",
            "project-shared-only",
            "",
            "project shared body",
        );
        // Same name at every level: native project wins, then project .agents,
        // then native user, then shared user.
        write_skill(
            &shared_user,
            "contested",
            "contested",
            "",
            "shared user body",
        );
        write_skill(&user, "contested", "contested", "", "user body");
        write_skill(
            &project.join(".agents/skills"),
            "contested",
            "contested",
            "",
            "project shared body",
        );
        write_skill(
            &project.join(".cookie-agent/skills"),
            "contested",
            "contested",
            "",
            "project native body",
        );
        let registry =
            load_skill_roots(Some(&shared_user), Some(&user), &[project]).expect("registry");
        assert_eq!(
            registry.get("shared-only").expect("shared").body,
            "shared user body\n"
        );
        assert_eq!(registry.get("user-only").expect("user").body, "user body\n");
        assert_eq!(
            registry
                .get("project-shared-only")
                .expect("project shared")
                .body,
            "project shared body\n"
        );
        assert_eq!(
            registry.get("contested").expect("winner").body,
            "project native body\n"
        );
        // Every shadowed user-scope discovery records a diagnostic.
        assert_eq!(registry.diagnostics().len(), 2);
        assert_eq!(registry.discoveries().len(), 7);
        // Native user beats shared user.
        let without_project = load_skill_roots(Some(&shared_user), Some(&user), &[])
            .expect("registry without project");
        assert_eq!(
            without_project.get("contested").expect("winner").body,
            "user body\n"
        );
        assert_eq!(without_project.diagnostics().len(), 1);
        assert!(
            without_project.diagnostics()[0]
                .message
                .contains("shadowed by user skill"),
            "{}",
            without_project.diagnostics()[0].message
        );
        // Project `.agents/skills` beats native user.
        let shared_only_project = temp.path().join("shared-project-only");
        write_skill(
            &shared_only_project.join(".agents/skills"),
            "contested",
            "contested",
            "",
            "project shared body",
        );
        let over_user = load_skill_roots(
            Some(&shared_user),
            Some(&user),
            &[shared_only_project.clone()],
        )
        .expect("registry with shared project root");
        assert_eq!(
            over_user.get("contested").expect("winner").body,
            "project shared body\n"
        );
        // Native project beats project `.agents/skills` within the same directory.
        write_skill(
            &shared_only_project.join(".cookie-agent/skills"),
            "contested",
            "contested",
            "",
            "project native body",
        );
        let within_project =
            load_skill_roots(None, None, &[shared_only_project]).expect("registry within project");
        assert_eq!(
            within_project.get("contested").expect("winner").body,
            "project native body\n"
        );
    }

    #[test]
    fn mismatched_name_and_unknown_field_are_targeted_errors() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_skill(
            &temp.path().join(".cookie-agent/skills"),
            "right",
            "wrong",
            "",
            "body",
        );
        assert!(
            load_skill_roots(None, None, &[temp.path().to_owned()])
                .unwrap_err()
                .to_string()
                .contains("name mismatch")
        );

        let temp = tempfile::tempdir().expect("tempdir");
        write_skill(
            &temp.path().join(".cookie-agent/skills"),
            "right",
            "right",
            "surprise: true\n",
            "body",
        );
        assert!(
            load_skill_roots(None, None, &[temp.path().to_owned()])
                .unwrap_err()
                .to_string()
                .contains("unknown frontmatter field")
        );
    }

    #[test]
    fn listing_drops_when_to_use_then_truncates_without_dropping_skill() {
        let temp = tempfile::tempdir().expect("tempdir");
        let description = "d".repeat(500);
        let directory = temp.path().join(".cookie-agent/skills/budget");
        fs::create_dir_all(&directory).expect("skill directory");
        fs::write(
            directory.join("SKILL.md"),
            format!(
                "---\nname: budget\ndescription: {description}\nwhen_to_use: {}\n---\nbody\n",
                "w".repeat(500)
            ),
        )
        .expect("skill document");
        let registry = load_skill_roots(None, None, &[temp.path().to_owned()]).expect("registry");
        let rendered = render_available_skills(registry.skills().map(|(_, skill)| skill), 10_000)
            .expect("bounded listing");
        assert!(rendered.len() <= 400);
        assert!(rendered.contains("<name>budget</name>"));
        assert!(!rendered.contains(&"w".repeat(100)));
    }
}
