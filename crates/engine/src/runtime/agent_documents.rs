//! The agents panel's calls (`docs/site/specs/acp-onboarding.md` section 7):
//! every agent document with where it comes from, and the four changes the
//! panel makes to the user's own documents: customize (start a document
//! that inherits the official one or the `_acp` template), turn on or off,
//! reset (delete the user's document), and replace (copy the official
//! document whole). Each is a `config.apply` of one document, validated
//! whole as any other.

use cookie_agent_config::{ConfigDocument, ConfigPath, ConfigTree};

use crate::runtime::configuration::{ConfigChange, config_error};
use crate::{Engine, EngineError};

/// What kind of document an entry is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentDocumentKind {
    /// An agent of the daemon's own.
    Agent,
    /// An ACP agent, declared by a runtime.
    Acp,
    /// A template, `_<name>.md`.
    Template,
}

/// Where an agent's document comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentDocumentOrigin {
    /// The official document, with no document of the user's.
    Official,
    /// A document of the user's that inherits the official one (or, for
    /// an ACP agent, the `_acp` template).
    Customized,
    /// A document of the user's that replaces the official one whole.
    Replaced,
    /// A document of the user's with no official counterpart.
    Own,
    /// An ACP agent with no document, which follows the `_acp` template.
    FollowsTemplate,
    /// An ACP agent with no document and no template.
    Bare,
}

/// One row of the agents panel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDocumentEntry {
    /// The agent's ID, or a template's `_<name>`.
    pub id: String,
    pub preset: Option<String>,
    pub kind: AgentDocumentKind,
    pub origin: AgentDocumentOrigin,
    /// The tree path the user's document inherits.
    pub inherits: Option<String>,
    /// The tree path of the user's document, when there is one.
    pub path: Option<String>,
    pub official_version: Option<String>,
    /// Whether the user's document turns it off.
    pub disabled: bool,
}

/// The `inherit` a document's text names, read leniently.
fn inherit_of(text: &str) -> Option<String> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    rest[..end].lines().find_map(|line| {
        line.strip_prefix("inherit:")
            .map(|value| value.trim().trim_matches(['"', '\'']).to_owned())
    })
}

/// Whether a document's text says `enabled: false`.
fn disabled_in(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("---") else {
        return false;
    };
    let Some(end) = rest.find("\n---") else {
        return false;
    };
    rest[..end]
        .lines()
        .any(|line| line.trim_end() == "enabled: false")
}

/// `inherit` resolved against the folder of `path`.
fn resolve_inherit(path: &str, inherit: &str) -> String {
    let mut parts = path.split('/').collect::<Vec<_>>();
    parts.pop();
    for component in inherit.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    parts.join("/")
}

impl Engine {
    /// Every agent document the panel shows: the official agents and
    /// templates, the user's documents, and the ACP agents the runtimes
    /// declare that have none.
    pub async fn agent_documents(&self) -> Result<Vec<AgentDocumentEntry>, EngineError> {
        let root = self.configuration_root()?;
        let tree = tokio::task::spawn_blocking(move || ConfigTree::read(Some(&root)))
            .await
            .map_err(|error| EngineError::Runtime(error.to_string()))?
            .map_err(config_error)?;
        let config = self.inner.config.load();
        let officials = tree
            .official_agents()
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>();
        let official_templates = tree
            .official_templates()
            .map(|(name, _)| format!("_{name}"))
            .collect::<Vec<_>>();
        let mut entries = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for (path, bytes) in tree.documents() {
            let (preset, id, kind) = match path.document() {
                ConfigDocument::Agent { preset, id } => (preset, id.to_owned(), None),
                ConfigDocument::Template { preset, name } => (
                    preset,
                    format!("_{name}"),
                    Some(AgentDocumentKind::Template),
                ),
                ConfigDocument::Config => continue,
            };
            let text = String::from_utf8_lossy(bytes);
            let inherits =
                inherit_of(&text).map(|inherit| resolve_inherit(path.as_str(), &inherit));
            let official_path =
                format!("{}/{id}.md", cookie_agent_config::OFFICIAL_AGENTS_DIRECTORY);
            let has_official = officials.contains(&id) || official_templates.contains(&id);
            let acp = text.contains("\nacp:")
                || inherits
                    .as_deref()
                    .is_some_and(|target| target.ends_with("/_acp.md"));
            let kind = kind.unwrap_or(if acp {
                AgentDocumentKind::Acp
            } else {
                AgentDocumentKind::Agent
            });
            let origin = match (&inherits, has_official) {
                (Some(target), true) if *target == official_path => AgentDocumentOrigin::Customized,
                (Some(target), _)
                    if kind == AgentDocumentKind::Acp && target.ends_with("_acp.md") =>
                {
                    AgentDocumentOrigin::Customized
                }
                (_, true) => AgentDocumentOrigin::Replaced,
                (_, false) => AgentDocumentOrigin::Own,
            };
            if preset.is_none() {
                seen.insert(id.clone());
            }
            entries.push(AgentDocumentEntry {
                official_version: has_official
                    .then(|| {
                        id.strip_prefix('_').map_or_else(
                            || {
                                cookie_agent_identity::AgentId::new(&id)
                                    .ok()
                                    .and_then(|id| tree.official_version(&id).map(str::to_owned))
                            },
                            |name| tree.official_template_version(name).map(str::to_owned),
                        )
                    })
                    .flatten(),
                id,
                preset: preset.map(str::to_owned),
                kind,
                origin,
                inherits,
                path: Some(path.to_string()),
                disabled: disabled_in(&text),
            });
        }
        for id in officials.iter().chain(&official_templates) {
            if seen.contains(id) {
                continue;
            }
            let template = id.starts_with('_');
            entries.push(AgentDocumentEntry {
                official_version: match id.strip_prefix('_') {
                    Some(name) => tree.official_template_version(name).map(str::to_owned),
                    None => cookie_agent_identity::AgentId::new(id)
                        .ok()
                        .and_then(|id| tree.official_version(&id).map(str::to_owned)),
                },
                id: id.clone(),
                preset: None,
                kind: if template {
                    AgentDocumentKind::Template
                } else {
                    AgentDocumentKind::Agent
                },
                origin: AgentDocumentOrigin::Official,
                inherits: None,
                path: None,
                disabled: false,
            });
            seen.insert(id.clone());
        }
        // ACP declarations with no document of their ID.
        let mut declared = config
            .runtime
            .acp
            .agents
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for template in config.runtimes.values() {
            declared.extend(template.acp_agents.keys().map(ToString::to_string));
        }
        declared.sort();
        declared.dedup();
        let has_template = config.templates.keys().any(|name| name.as_str() == "acp");
        for id in declared {
            if seen.contains(&id) {
                continue;
            }
            entries.push(AgentDocumentEntry {
                id,
                preset: None,
                kind: AgentDocumentKind::Acp,
                origin: if has_template {
                    AgentDocumentOrigin::FollowsTemplate
                } else {
                    AgentDocumentOrigin::Bare
                },
                inherits: None,
                path: None,
                official_version: None,
                disabled: false,
            });
        }
        entries.sort_by(|left, right| (&left.preset, &left.id).cmp(&(&right.preset, &right.id)));
        Ok(entries)
    }

    /// The tree path of `id`'s document of the user's, in `preset`.
    fn user_document_path(id: &str, preset: Option<&str>) -> Result<ConfigPath, EngineError> {
        let path = match preset {
            Some(preset) => format!("agents/{preset}/{id}.md"),
            None => format!("agents/{id}.md"),
        };
        ConfigPath::new(&path).map_err(|error| EngineError::ConfigChangeRefused(error.to_string()))
    }

    /// What a new document of `id` inherits, relative to `path`: the
    /// official agent or template of its ID, else (an ACP agent) the `_acp`
    /// template, the user's when there is one.
    fn inherit_target(tree: &ConfigTree, id: &str, path: &ConfigPath) -> Option<String> {
        let official = match id.strip_prefix('_') {
            Some(name) => tree.official_templates().any(|(each, _)| each == name),
            None => tree.official_agents().any(|(each, _)| each.as_str() == id),
        };
        let target = if official {
            format!("{}/{id}.md", cookie_agent_config::OFFICIAL_AGENTS_DIRECTORY)
        } else if id.starts_with('_') {
            return None;
        } else if ConfigPath::new("agents/_acp.md")
            .ok()
            .is_some_and(|template| tree.get(&template).is_some())
        {
            "agents/_acp.md".to_owned()
        } else if tree.official_templates().any(|(name, _)| name == "acp") {
            format!("{}/_acp.md", cookie_agent_config::OFFICIAL_AGENTS_DIRECTORY)
        } else {
            return None;
        };
        Some(relative_path(path.as_str(), &target))
    }

    /// Starts a document of the user's for `id` that inherits the official
    /// one, or the `_acp` template for an ACP agent, listing what it
    /// inherits as comments; one that exists is left as it is. Returns its
    /// tree path.
    pub async fn customize_agent(
        &self,
        id: &str,
        preset: Option<&str>,
    ) -> Result<String, EngineError> {
        let path = Self::user_document_path(id, preset)?;
        let documents = self.configuration_documents().await?;
        if documents
            .documents
            .iter()
            .any(|document| document.path == path)
        {
            return Ok(path.to_string());
        }
        let tree = self.read_tree().await?;
        let target = Self::inherit_target(&tree, id, &path).ok_or_else(|| {
            EngineError::ConfigChangeRefused(format!(
                "`{id}` has no official document or template to inherit"
            ))
        })?;
        let base = self.inherited_text(&tree, &resolve_inherit(path.as_str(), &target));
        let text = customized_text(&target, base.as_deref(), None);
        self.apply_config_changes(
            documents.revision,
            vec![ConfigChange::Put {
                path: path.clone(),
                text,
                expected_fingerprint: None,
            }],
        )
        .await?;
        Ok(path.to_string())
    }

    /// Turns `id` on or off in its document of the user's: off writes
    /// `enabled: false` (starting a document that inherits the official
    /// one when there is none); on removes it, and the document with it
    /// when nothing else is left.
    pub async fn set_agent_enabled(
        &self,
        id: &str,
        preset: Option<&str>,
        enabled: bool,
    ) -> Result<String, EngineError> {
        let path = Self::user_document_path(id, preset)?;
        let shown = path.to_string();
        let documents = self.configuration_documents().await?;
        let current = documents
            .documents
            .iter()
            .find(|document| document.path == path)
            .cloned();
        let change = match (current, enabled) {
            (None, true) => return Ok(shown),
            (None, false) => {
                let tree = self.read_tree().await?;
                let target = Self::inherit_target(&tree, id, &path).ok_or_else(|| {
                    EngineError::ConfigChangeRefused(format!("`{id}` has no document to turn off"))
                })?;
                let base = self.inherited_text(&tree, &resolve_inherit(path.as_str(), &target));
                ConfigChange::Put {
                    path,
                    text: customized_text(&target, base.as_deref(), Some(false)),
                    expected_fingerprint: None,
                }
            }
            (Some(document), enabled) => {
                let text = with_enabled(&document.text, enabled).ok_or_else(|| {
                    EngineError::ConfigChangeRefused(format!(
                        "{} has no frontmatter to edit",
                        document.path
                    ))
                })?;
                if enabled && only_inherits(&text) {
                    ConfigChange::Delete {
                        path,
                        expected_fingerprint: document.fingerprint,
                    }
                } else {
                    ConfigChange::Put {
                        path,
                        text,
                        expected_fingerprint: Some(document.fingerprint),
                    }
                }
            }
        };
        self.apply_config_changes(documents.revision, vec![change])
            .await?;
        Ok(shown)
    }

    /// Deletes `id`'s document of the user's, so the official one (or the
    /// template) applies again.
    pub async fn reset_agent(&self, id: &str, preset: Option<&str>) -> Result<String, EngineError> {
        let path = Self::user_document_path(id, preset)?;
        let documents = self.configuration_documents().await?;
        let document = documents
            .documents
            .iter()
            .find(|document| document.path == path)
            .ok_or_else(|| EngineError::ConfigChangeRefused(format!("{path} does not exist")))?;
        self.apply_config_changes(
            documents.revision.clone(),
            vec![ConfigChange::Delete {
                path: path.clone(),
                expected_fingerprint: document.fingerprint.clone(),
            }],
        )
        .await?;
        Ok(path.to_string())
    }

    /// Copies the official document of `id` whole into the user's
    /// documents, where refreshes no longer reach it; one of the user's is
    /// replaced. Returns its tree path.
    pub async fn replace_agent(
        &self,
        id: &str,
        preset: Option<&str>,
    ) -> Result<String, EngineError> {
        let path = Self::user_document_path(id, preset)?;
        let tree = self.read_tree().await?;
        let official = format!("{}/{id}.md", cookie_agent_config::OFFICIAL_AGENTS_DIRECTORY);
        let text = self.inherited_text(&tree, &official).ok_or_else(|| {
            EngineError::ConfigChangeRefused(format!("`{id}` has no official document"))
        })?;
        let documents = self.configuration_documents().await?;
        let expected_fingerprint = documents
            .documents
            .iter()
            .find(|document| document.path == path)
            .map(|document| document.fingerprint.clone());
        self.apply_config_changes(
            documents.revision,
            vec![ConfigChange::Put {
                path: path.clone(),
                text,
                expected_fingerprint,
            }],
        )
        .await?;
        Ok(path.to_string())
    }

    async fn read_tree(&self) -> Result<ConfigTree, EngineError> {
        let root = self.configuration_root()?;
        tokio::task::spawn_blocking(move || ConfigTree::read(Some(&root)))
            .await
            .map_err(|error| EngineError::Runtime(error.to_string()))?
            .map_err(config_error)
    }

    /// The text of the document at tree path `target`: an official agent
    /// or template, or one of the user's.
    fn inherited_text(&self, tree: &ConfigTree, target: &str) -> Option<String> {
        let official = format!("{}/", cookie_agent_config::OFFICIAL_AGENTS_DIRECTORY);
        let bytes = match target.strip_prefix(&official) {
            Some(file) => {
                let stem = file.strip_suffix(".md")?;
                match stem.strip_prefix('_') {
                    Some(name) => tree
                        .official_templates()
                        .find(|(each, _)| *each == name)
                        .map(|(_, bytes)| bytes.to_vec()),
                    None => tree
                        .official_agents()
                        .find(|(each, _)| each.as_str() == stem)
                        .map(|(_, bytes)| bytes.to_vec()),
                }
            }
            None => tree
                .documents()
                .find(|(path, _)| path.as_str() == target)
                .map(|(_, bytes)| bytes.to_vec()),
        }?;
        String::from_utf8(bytes).ok()
    }
}

/// `target` (a tree path) relative to the folder of the document at `path`.
fn relative_path(path: &str, target: &str) -> String {
    let folder = path.rsplit_once('/').map_or("", |(folder, _)| folder);
    let mut up = folder
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let mut target_parts = target.split('/').collect::<Vec<_>>();
    // Drop what the two share.
    while !up.is_empty() && target_parts.len() > 1 && up[0] == target_parts[0] {
        up.remove(0);
        target_parts.remove(0);
    }
    format!("{}{}", "../".repeat(up.len()), target_parts.join("/"))
}

/// A new document of the user's: `inherit: target`, `enabled` when given,
/// and what `base` sets, as comments to uncomment and change.
fn customized_text(target: &str, base: Option<&str>, enabled: Option<bool>) -> String {
    let mut text = format!("---\ninherit: {target}\n");
    if let Some(enabled) = enabled {
        text.push_str(&format!("enabled: {enabled}\n"));
    }
    let frontmatter = base.and_then(|base| {
        let rest = base.strip_prefix("---")?;
        let end = rest.find("\n---")?;
        Some(rest[..end].trim_start_matches('\n').to_owned())
    });
    if let Some(frontmatter) = frontmatter.filter(|text| !text.trim().is_empty()) {
        text.push_str(
            "# Every key below is inherited. Write a key to change it; the rest follows the\n\
             # document this one inherits. As it reads now:\n",
        );
        for line in frontmatter.lines() {
            text.push_str("#   ");
            text.push_str(line);
            text.push('\n');
        }
    }
    text.push_str("---\n");
    text
}

/// `text` with `enabled` written (`false`) or removed (`true`) in its
/// frontmatter; `None` when it has none.
fn with_enabled(text: &str, enabled: bool) -> Option<String> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let (frontmatter, after) = rest.split_at(end);
    let mut lines = frontmatter
        .lines()
        .filter(|line| !line.trim_start().starts_with("enabled:") || line.starts_with(' '))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !enabled {
        let at = lines
            .iter()
            .position(|line| line.starts_with("inherit:"))
            .map_or(0, |index| index + 1);
        lines.insert(at, "enabled: false".to_owned());
    }
    Some(format!("---\n{}{after}", lines.join("\n") + "\n").replace("\n\n---", "\n---"))
}

/// Whether `text`'s frontmatter holds nothing but `inherit` and comments,
/// and its body is empty.
fn only_inherits(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("---\n") else {
        return false;
    };
    let Some(end) = rest.find("\n---") else {
        return false;
    };
    let body = rest[end + 4..].trim();
    body.is_empty()
        && rest[..end].lines().all(|line| {
            let line = line.trim();
            line.is_empty() || line.starts_with('#') || line.starts_with("inherit:")
        })
}

#[cfg(test)]
mod tests {
    use super::{customized_text, only_inherits, relative_path, resolve_inherit, with_enabled};

    #[test]
    fn paths_between_documents_climb_out_of_their_folders() {
        assert_eq!(
            relative_path("agents/primary.md", "official_agents/primary.md"),
            "../official_agents/primary.md"
        );
        assert_eq!(
            relative_path("agents/review/primary.md", "official_agents/primary.md"),
            "../../official_agents/primary.md"
        );
        assert_eq!(
            relative_path("agents/claude.md", "agents/_acp.md"),
            "_acp.md"
        );
        assert_eq!(
            resolve_inherit("agents/review/x.md", "../../official_agents/x.md"),
            "official_agents/x.md"
        );
    }

    #[test]
    fn a_customized_document_inherits_and_lists_what_it_inherits_as_comments() {
        let text = customized_text(
            "../official_agents/primary.md",
            Some("---\ndescription: The assistant\nmode: primary\n---\nHelp.\n"),
            Some(false),
        );
        assert!(text.starts_with("---\ninherit: ../official_agents/primary.md\nenabled: false\n"));
        assert!(text.contains("#   description: The assistant\n"));
        assert!(text.ends_with("---\n"));
    }

    #[test]
    fn turning_on_and_off_edits_only_enabled() {
        let off = with_enabled("---\ninherit: x.md\n# note\n---\nBody\n", false).unwrap();
        assert_eq!(
            off,
            "---\ninherit: x.md\nenabled: false\n# note\n---\nBody\n"
        );
        let on = with_enabled(&off, true).unwrap();
        assert_eq!(on, "---\ninherit: x.md\n# note\n---\nBody\n");
        assert!(!only_inherits(&on), "it has a body");
        assert!(only_inherits("---\ninherit: x.md\n# note\n---\n"));
        // A nested `enabled` under another key is not the agent's.
        let nested = "---\ninherit: x.md\nacp:\n  enabled: true\n---\n";
        assert_eq!(with_enabled(nested, true).unwrap(), nested);
    }
}
