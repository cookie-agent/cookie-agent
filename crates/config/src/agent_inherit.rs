//! `inherit:` in a user's agent document: the path of another agent document
//! (an official one, `../official_agents/primary.md`, or one of the user's)
//! that this one starts from. Every frontmatter key the document writes
//! replaces the inherited one; `permissions` are replaced per action, so a
//! `bash` key replaces the inherited `bash` keys (any runtime selector) and
//! leaves the other actions as inherited. A body left empty is the inherited
//! document's prompt, and `{{default_prompt}}` in a body stands for it.
//!
//! The path is relative to the document's own file and must name an agent
//! document of the configuration tree: `official_agents/<id>.md`,
//! `agents/<id>.md`, or `agents/<preset>/<id>.md`. Inheritance may chain; a
//! cycle is an error. The merge happens before the strict parser reads the
//! document, so the merged document is validated as one written whole would
//! be, and its fingerprints change when what it inherits does.

use std::path::Path;

use crate::{ConfigError, ConfigTree, agent_document};

/// The frontmatter key.
pub(crate) const INHERIT_KEY: &str = "inherit";
/// The longest chain of documents inheriting one another.
const MAX_CHAIN: usize = 8;

/// What a document of the user's means once its `inherit` is resolved.
pub(crate) enum Resolved {
    /// It inherits nothing: read it as written.
    AsWritten,
    /// The merged document, to read in its place.
    Merged(Vec<u8>),
    /// It inherits a document the tree does not hold (yet), such as an
    /// official agent before the first refresh: this path.
    Missing(String),
}

/// A document's frontmatter and body.
struct Parts {
    yaml: serde_yaml::Mapping,
    body: String,
}

enum Outcome {
    Parts(Parts),
    Missing(String),
}

/// Resolves the `inherit` of the document at `path` (a tree path such as
/// `agents/primary.md`), whose content is `bytes`. Official agents are read
/// only when `officials` is set.
pub(crate) fn resolve(
    tree: &ConfigTree,
    path: &str,
    bytes: &[u8],
    officials: bool,
) -> Result<Resolved, ConfigError> {
    let shown = tree_path(tree, path);
    let (yaml, _) = agent_document::split_document(bytes, &shown)?;
    let mapping = mapping_of(&yaml, &shown)?;
    if !mapping.contains_key(INHERIT_KEY) {
        return Ok(Resolved::AsWritten);
    }
    let mut chain = vec![path.to_owned()];
    Ok(match parts_of(tree, path, bytes, officials, &mut chain)? {
        Outcome::Missing(target) => Resolved::Missing(target),
        Outcome::Parts(parts) => {
            // As JSON, which YAML reads: every string stays quoted, so
            // the parser's check for YAML aliases and tags never mistakes a
            // pattern such as `ls *` for one.
            let yaml = serde_json::to_string_pretty(&parts.yaml).map_err(|error| {
                agent_document::invalid_document(&shown, format!("inherited frontmatter: {error}"))
            })? + "\n";
            Resolved::Merged(format!("---\n{yaml}---\n{}", parts.body).into_bytes())
        }
    })
}

/// The frontmatter and body of the document at `path`, `inherit` resolved
/// (and its own `{{default_prompt}}`, when it inherits nothing, from the
/// document it replaces).
fn parts_of(
    tree: &ConfigTree,
    path: &str,
    bytes: &[u8],
    officials: bool,
    chain: &mut Vec<String>,
) -> Result<Outcome, ConfigError> {
    let shown = tree_path(tree, path);
    let (yaml, body) = agent_document::split_document(bytes, &shown)?;
    let mut mapping = mapping_of(&yaml, &shown)?;
    let official = path.starts_with(&format!("{}/", crate::OFFICIAL_AGENTS_DIRECTORY));
    let Some(inherit) = mapping.remove(INHERIT_KEY) else {
        let body = if official {
            body
        } else {
            match replaced_prompt(tree, path, officials, chain)? {
                Some(prompt) => crate::prompt_template::replace_default_prompt(&body, &prompt),
                None => body,
            }
        };
        return Ok(Outcome::Parts(Parts {
            yaml: mapping,
            body,
        }));
    };
    if official {
        // An official agent never inherits; the loader leaves it out.
        return Ok(Outcome::Missing(path.to_owned()));
    }
    let inherit = inherit.as_str().ok_or_else(|| {
        agent_document::invalid_document(&shown, "`inherit` must be the path of an agent document")
    })?;
    let target = target_path(path, inherit).ok_or_else(|| {
        agent_document::invalid_document(
            &shown,
            format!(
                "`inherit: {inherit}` must name an agent document of the configuration: \
                     official_agents/<id>.md, agents/<id>.md, or agents/<preset>/<id>.md, \
                     relative to this file"
            ),
        )
    })?;
    if chain.contains(&target) || chain.len() >= MAX_CHAIN {
        return Err(agent_document::invalid_document(
            &shown,
            format!(
                "`inherit` makes a cycle or a chain longer than {MAX_CHAIN}: {} → {target}",
                chain.join(" → ")
            ),
        ));
    }
    let Some(target_bytes) = lookup(tree, &target, officials) else {
        return Ok(Outcome::Missing(target));
    };
    chain.push(target.clone());
    let base = parts_of(tree, &target, &target_bytes, officials, chain)?;
    chain.pop();
    let Outcome::Parts(base) = base else {
        return Ok(base);
    };
    Ok(Outcome::Parts(merge(base, mapping, body)))
}

/// `base` with `mapping`'s keys over it (`permissions` per action) and
/// `body`, or `base`'s body when `body` is empty.
fn merge(base: Parts, mapping: serde_yaml::Mapping, body: String) -> Parts {
    let mut yaml = base.yaml;
    for (key, value) in mapping {
        if key.as_str() == Some("permissions")
            && let (Some(serde_yaml::Value::Mapping(inherited)), serde_yaml::Value::Mapping(own)) =
                (yaml.get_mut(&key), &value)
        {
            for (own_key, own_value) in own {
                let action = |key: &serde_yaml::Value| {
                    key.as_str()
                        .map(|key| key.split('@').next().unwrap_or(key).to_owned())
                };
                let replaced = action(own_key);
                inherited.retain(|inherited_key, _| action(inherited_key) != replaced);
                inherited.insert(own_key.clone(), own_value.clone());
            }
            continue;
        }
        yaml.insert(key, value);
    }
    let body = if body.trim().is_empty() {
        base.body
    } else {
        crate::prompt_template::replace_default_prompt(&body, base.body.trim_end_matches('\n'))
    };
    Parts { yaml, body }
}

/// The prompt a document at `path` that inherits nothing takes its
/// `{{default_prompt}}` from: the document it replaces, a preset's document
/// the user's own top-level one, else the official agent of its ID. `None`
/// when neither exists, which the loader handles (`compaction`'s built-in
/// prompt, or a diagnostic).
fn replaced_prompt(
    tree: &ConfigTree,
    path: &str,
    officials: bool,
    chain: &mut Vec<String>,
) -> Result<Option<String>, ConfigError> {
    let Some(rest) = path.strip_prefix("agents/") else {
        return Ok(None);
    };
    let file = rest.rsplit('/').next().unwrap_or(rest);
    let mut candidates = Vec::new();
    if rest.contains('/') {
        candidates.push(format!("agents/{file}"));
    }
    candidates.push(format!("{}/{file}", crate::OFFICIAL_AGENTS_DIRECTORY));
    for candidate in candidates {
        if chain.contains(&candidate) {
            continue;
        }
        let Some(bytes) = lookup(tree, &candidate, officials) else {
            continue;
        };
        chain.push(candidate.clone());
        let outcome = parts_of(tree, &candidate, &bytes, officials, chain)?;
        chain.pop();
        return Ok(match outcome {
            Outcome::Parts(parts) => Some(parts.body.trim_end_matches('\n').to_owned()),
            Outcome::Missing(_) => None,
        });
    }
    Ok(None)
}

fn mapping_of(yaml: &str, shown: &Path) -> Result<serde_yaml::Mapping, ConfigError> {
    match agent_document::parse_yaml(yaml, shown)? {
        serde_yaml::Value::Mapping(mapping) => Ok(mapping),
        serde_yaml::Value::Null => Ok(serde_yaml::Mapping::new()),
        _ => Err(agent_document::invalid_document(
            shown,
            "frontmatter must be a mapping",
        )),
    }
}

/// `inherit` read relative to the folder of the document at `path`, as a
/// tree path of an agent document; `None` for anything else.
fn target_path(path: &str, inherit: &str) -> Option<String> {
    if inherit.starts_with('/') || inherit.contains('\\') {
        return None;
    }
    let mut parts = path.split('/').collect::<Vec<_>>();
    parts.pop();
    for component in inherit.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            name => parts.push(name),
        }
    }
    let joined = parts.join("/");
    let valid_id = |file: &str| {
        file.strip_suffix(".md")
            .is_some_and(|id| cookie_agent_identity::AgentId::new(id).is_ok())
    };
    let agent_document = match parts.as_slice() {
        [directory, file] => {
            (*directory == crate::OFFICIAL_AGENTS_DIRECTORY || *directory == "agents")
                && valid_id(file)
        }
        ["agents", preset, file] => {
            !preset.is_empty() && !preset.starts_with('.') && valid_id(file)
        }
        _ => false,
    };
    agent_document.then_some(joined)
}

/// The content of the agent document at tree path `path`.
fn lookup(tree: &ConfigTree, path: &str, officials: bool) -> Option<Vec<u8>> {
    if let Some(file) = path.strip_prefix(&format!("{}/", crate::OFFICIAL_AGENTS_DIRECTORY)) {
        if !officials {
            return None;
        }
        let id = file.strip_suffix(".md")?;
        return tree
            .official_agents()
            .find(|(official, _)| official.as_str() == id)
            .map(|(_, bytes)| bytes.to_vec());
    }
    tree.documents()
        .find(|(document, _)| document.as_str() == path)
        .map(|(_, bytes)| bytes.to_vec())
}

/// Where `path` is on disk, for messages.
fn tree_path(tree: &ConfigTree, path: &str) -> std::path::PathBuf {
    tree.root()
        .map_or_else(|| std::path::PathBuf::from(path), |root| root.join(path))
}

#[cfg(test)]
mod tests {
    use super::target_path;

    #[test]
    fn inherit_paths_name_agent_documents_of_the_tree() {
        assert_eq!(
            target_path("agents/primary.md", "../official_agents/primary.md").as_deref(),
            Some("official_agents/primary.md")
        );
        assert_eq!(
            target_path("agents/fast.md", "primary.md").as_deref(),
            Some("agents/primary.md")
        );
        assert_eq!(
            target_path("agents/review/primary.md", "../primary.md").as_deref(),
            Some("agents/primary.md")
        );
        assert_eq!(
            target_path(
                "agents/review/primary.md",
                "../../official_agents/primary.md"
            )
            .as_deref(),
            Some("official_agents/primary.md")
        );
        assert_eq!(
            target_path("agents/x.md", "./other/y.md").as_deref(),
            Some("agents/other/y.md")
        );
        for refused in [
            "/etc/passwd",
            "../../secret.md",
            "../config.toml",
            "../official_agents/Not An ID.md",
            "../official_agents/x/y.md",
            "..\\official_agents\\x.md",
        ] {
            assert_eq!(target_path("agents/x.md", refused), None, "{refused}");
        }
    }
}
