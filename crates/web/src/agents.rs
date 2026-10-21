//! The Agents view's list: the agent documents of `config.get`, the
//! shared ones first, then each preset with its agents, and what a row
//! shows of a document: its description, its mode, and whether it is
//! enabled, read from the document's frontmatter.

use cookie_agent_protocol::ConfigGetResult;

/// One agent document as its row shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentEntry {
    /// The agent's ID: the document's file name without `.md`.
    pub id: String,
    pub path: String,
    pub summary: AgentSummary,
}

/// The agents of one preset, or the shared agents (`preset` is `None`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentGroup {
    pub preset: Option<String>,
    pub agents: Vec<AgentEntry>,
}

/// What a row shows of an agent document, from its frontmatter. A
/// document whose frontmatter does not say has no description and the
/// mode `None`; the daemon checks the document itself.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentSummary {
    pub description: Option<String>,
    /// `primary`, `subagent`, `all`, or whatever the document says.
    pub mode: Option<String>,
    pub enabled: bool,
}

/// The groups of `config`: the shared agents (when there are any), then
/// each preset in name order with its agents, an empty preset included.
pub fn agent_groups(config: &ConfigGetResult) -> Vec<AgentGroup> {
    let mut shared = Vec::new();
    let mut presets = std::collections::BTreeMap::<String, Vec<AgentEntry>>::new();
    for preset in &config.empty_presets {
        presets.entry(preset.clone()).or_default();
    }
    for document in &config.documents {
        let Some(rest) = document
            .path
            .strip_prefix("agents/")
            .and_then(|rest| rest.strip_suffix(".md"))
        else {
            continue;
        };
        let (preset, id) = match rest.split_once('/') {
            Some((preset, id)) => (Some(preset), id),
            None => (None, rest),
        };
        let entry = AgentEntry {
            id: id.to_owned(),
            path: document.path.clone(),
            summary: agent_summary(&document.text),
        };
        match preset {
            Some(preset) => presets.entry(preset.to_owned()).or_default().push(entry),
            None => shared.push(entry),
        }
    }
    let mut groups = Vec::new();
    if !shared.is_empty() {
        groups.push(AgentGroup {
            preset: None,
            agents: shared,
        });
    }
    groups.extend(presets.into_iter().map(|(preset, agents)| AgentGroup {
        preset: Some(preset),
        agents,
    }));
    groups
}

/// The description, mode, and `enabled` of the frontmatter of `text`: its
/// top-level `key: value` lines between the opening `---` and the next.
/// Only plain scalars are read; anything else is the daemon's to check.
pub fn agent_summary(text: &str) -> AgentSummary {
    let mut summary = AgentSummary {
        enabled: true,
        ..AgentSummary::default()
    };
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return summary;
    }
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        // Nested lines belong to another key.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = scalar(value);
        match key.trim() {
            "description" => summary.description = (!value.is_empty()).then(|| value.to_owned()),
            "mode" => summary.mode = (!value.is_empty()).then(|| value.to_owned()),
            "enabled" => summary.enabled = value != "false",
            _ => {}
        }
    }
    summary
}

/// A YAML scalar's text: trimmed, without its quotes.
fn scalar(value: &str) -> &str {
    let value = value.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|value| value.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// The agent ID of the document at `path`, for the editor's heading.
pub fn agent_id(path: &str) -> &str {
    let rest = path.strip_suffix(".md").unwrap_or(path);
    rest.rsplit_once('/').map_or(rest, |(_, id)| id)
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::ConfigGetResult;

    use super::{AgentSummary, agent_groups, agent_id, agent_summary};

    fn config() -> ConfigGetResult {
        serde_json::from_value(serde_json::json!({
            "revision": format!("sha256:{}", "a".repeat(64)),
            "status": {"applied_revision": format!("sha256:{}", "a".repeat(64))},
            "documents": [
                {"path": "config.toml", "text": "", "fingerprint": "c"},
                {"path": "agents/primary.md", "text": "---\ndescription: Codes\nmode: primary\n---\nHelp.\n", "fingerprint": "a"},
                {"path": "agents/fast/helper.md", "text": "---\nmode: subagent\n---\n", "fingerprint": "b"}
            ],
            "empty_presets": ["empty"]
        }))
        .expect("config")
    }

    #[test]
    fn shared_agents_come_first_then_each_preset_by_name() {
        let groups = agent_groups(&config());
        let shape = groups
            .iter()
            .map(|group| {
                (
                    group.preset.clone(),
                    group
                        .agents
                        .iter()
                        .map(|agent| agent.id.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shape,
            [
                (None, vec!["primary".to_owned()]),
                (Some("empty".to_owned()), vec![]),
                (Some("fast".to_owned()), vec!["helper".to_owned()]),
            ]
        );
        assert_eq!(groups[2].agents[0].path, "agents/fast/helper.md");
    }

    #[test]
    fn a_preset_only_configuration_has_no_shared_group() {
        let mut config = config();
        config
            .documents
            .retain(|document| document.path != "agents/primary.md");
        let groups = agent_groups(&config);
        assert!(groups.iter().all(|group| group.preset.is_some()));
    }

    #[test]
    fn a_row_reads_the_frontmatter_scalars() {
        assert_eq!(
            agent_summary(
                "---\ndescription: \"Reviews: diffs\"\nmode: 'subagent'\nenabled: false\nmodels:\n  - { model: x }\n---\nmode: not this\n"
            ),
            AgentSummary {
                description: Some("Reviews: diffs".into()),
                mode: Some("subagent".into()),
                enabled: false,
            }
        );
        let bare = agent_summary("Help.\n");
        assert_eq!(bare.description, None);
        assert!(bare.enabled, "a document that does not say is enabled");
        assert_eq!(agent_summary("---\ndescription: \n---\n").description, None);
    }

    #[test]
    fn the_editor_names_the_agent_by_its_file() {
        assert_eq!(agent_id("agents/fast/helper.md"), "helper");
        assert_eq!(agent_id("agents/primary.md"), "primary");
    }
}
