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

/// `text` with its front matter's top-level `key` set to `value`, for the
/// agent editor's form (`docs/site/specs/web-polish.md`, P5): the key's one
/// line replaced, or added before the closing `---`, or a front matter made
/// when the document has none. Every other line stays as written. `None`
/// when the key holds a nested block (an indented line follows it), which
/// a one-line value would cut short: the file view edits those.
pub fn set_front_matter(text: &str, key: &str, value: &str) -> Option<String> {
    let line = format!("{key}: {}", yaml_scalar(value));
    let lines: Vec<&str> = text.lines().collect();
    if lines.first().map(|first| first.trim_end()) != Some("---") {
        return Some(format!("---\n{line}\n---\n{text}"));
    }
    let close = lines
        .iter()
        .skip(1)
        .position(|line| line.trim_end() == "---")
        .map(|at| at + 1)?;
    let at = (1..close).find(|&index| {
        let line = lines[index];
        !line.starts_with([' ', '\t'])
            && line
                .split_once(':')
                .is_some_and(|(name, _)| name.trim() == key)
    });
    let mut out: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    match at {
        Some(index) => {
            if lines
                .get(index + 1)
                .is_some_and(|next| index + 1 < close && next.starts_with([' ', '\t']))
            {
                return None;
            }
            out[index] = line;
        }
        None => out.insert(close, line),
    }
    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    Some(joined)
}

/// `value` as a YAML scalar on one line: as it is when that reads back the
/// same, else double-quoted.
fn yaml_scalar(value: &str) -> String {
    let plain = !value.is_empty()
        && value.trim() == value
        && !value.contains([
            ':', '#', '"', '\'', '\\', '\n', '[', ']', '{', '}', ',', '&', '*', '!', '|', '>', '%',
            '@', '`',
        ])
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~"
        )
        && value.parse::<f64>().is_err()
        && !value.starts_with(['-', '?']);
    if plain {
        value.to_owned()
    } else {
        format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
        )
    }
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

    use super::{AgentSummary, agent_groups, agent_id, agent_summary, set_front_matter};

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

    #[test]
    fn the_form_sets_one_top_level_key_and_leaves_the_rest() {
        let text = "---\ndescription: Old\nmode: primary\nmodels:\n  - { model: x }\n---\nBody.\n";
        let set = set_front_matter(text, "description", "Reviews: diffs").expect("set");
        assert_eq!(
            set,
            "---\ndescription: \"Reviews: diffs\"\nmode: primary\nmodels:\n  - { model: x }\n---\nBody.\n"
        );
        assert_eq!(
            agent_summary(&set).description.as_deref(),
            Some("Reviews: diffs")
        );
        let moded = set_front_matter(&set, "mode", "subagent").expect("set");
        assert_eq!(agent_summary(&moded).mode.as_deref(), Some("subagent"));
        assert!(moded.ends_with("---\nBody.\n"));
    }

    #[test]
    fn the_form_adds_a_missing_key_and_refuses_a_block() {
        let added =
            set_front_matter("---\nmode: all\n---\nBody", "description", "Plain").expect("set");
        assert_eq!(added, "---\nmode: all\ndescription: Plain\n---\nBody");
        let made = set_front_matter("Body only", "mode", "primary").expect("set");
        assert_eq!(made, "---\nmode: primary\n---\nBody only");
        assert_eq!(
            set_front_matter("---\nmodels:\n  - a\n---\n", "models", "x"),
            None
        );
        assert_eq!(
            set_front_matter("---\nmode: all\n", "mode", "x"),
            None,
            "an unclosed front matter"
        );
        // Values YAML would read as something else are quoted.
        let quoted = set_front_matter("---\n---\n", "description", "true").expect("set");
        assert!(quoted.contains("description: \"true\""), "{quoted}");
    }
}
