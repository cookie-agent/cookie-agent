//! The `/memory` panel's rules every client shares: the memories as rows
//! by layer and group, what a new memory starts from and where, what
//! `memory.write` answered, how a delete's refusal reads, and the parts of
//! an `agent://` file the daemon owns.

use cookie_agent_protocol::{
    ClientError, MemoryIgnoredEntry, MemoryInfo, MemoryLayer, MemoryListResult, MemoryWriteResult,
    errors,
};

/// The text a new memory starts from.
pub const NEW_MEMORY: &str = "---\ndescription: \nkeywords: []\n---\n";

/// What `memory.write` answered.
#[derive(Debug, Eq, PartialEq)]
pub enum MemoryWriteOutcome {
    /// Written; `notice` says what the daemon kept in place of what the
    /// text gave (an `agent://` file's description and first keywords).
    Saved {
        notice: Option<String>,
    },
    /// The text is not a valid memory: why.
    Invalid(String),
    /// The memory changed, appeared, or went away since it was read.
    Changed,
    Failed(String),
}

impl MemoryWriteOutcome {
    pub fn from_result(result: Result<MemoryWriteResult, ClientError>) -> Self {
        match result {
            Ok(written) => Self::Saved {
                notice: written.notice,
            },
            Err(error) if error.is(&errors::MEMORY_INVALID) => Self::Invalid(message_of(&error)),
            Err(error)
                if error.is(&errors::MEMORY_REVISION_MISMATCH)
                    || error.is(&errors::MEMORY_EXISTS)
                    || error.is(&errors::MEMORY_NOT_FOUND) =>
            {
                Self::Changed
            }
            Err(error) => Self::Failed(message_of(&error)),
        }
    }
}

/// What the status line says once `url` is saved: `Created` for a new
/// memory, else `Saved`, then the daemon's notice when it gave one.
pub fn saved_status(url: &str, created: bool, notice: Option<&str>) -> String {
    let verb = if created { "Created" } else { "Saved" };
    match notice {
        Some(notice) => format!("{verb} {url}. {notice}"),
        None => format!("{verb} {url}."),
    }
}

/// The message of a daemon error, without its code.
pub fn message_of(error: &ClientError) -> String {
    match error {
        ClientError::Rpc(rpc) => rpc.message.clone(),
        error => error.to_string(),
    }
}

/// What a refused `memory.delete` of `url` says.
pub fn delete_refusal(url: &str, error: &ClientError) -> String {
    if error.is(&errors::MEMORY_REVISION_MISMATCH) {
        format!("{url} changed since it was listed; it was not deleted")
    } else {
        format!("{url} was not deleted: {}", message_of(error))
    }
}

/// One row of the panel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryRow {
    /// A layer's heading.
    Layer(MemoryLayer),
    /// A machine or project of the layer.
    Group {
        layer: MemoryLayer,
        name: String,
    },
    Memory(MemoryInfo),
    /// The heading of what is not a memory.
    IgnoredHeading,
    Ignored(MemoryIgnoredEntry),
}

impl MemoryRow {
    /// The URL prefix a new memory started from this row gets.
    pub fn new_prefix(&self) -> String {
        match self {
            Self::Layer(layer) => format!("{}://", layer.as_str()),
            Self::Group { layer, name } => format!("{}://{name}/", layer.as_str()),
            Self::Memory(memory) => match &memory.group {
                Some(group) => format!("{}://{group}/", memory.layer.as_str()),
                None => format!("{}://", memory.layer.as_str()),
            },
            Self::IgnoredHeading | Self::Ignored(_) => "project://".to_owned(),
        }
    }
}

/// The rows: each layer's heading, its memories grouped by machine or
/// project, then what is not a memory.
pub fn memory_rows(listing: &MemoryListResult) -> Vec<MemoryRow> {
    let mut rows = Vec::new();
    for layer in MemoryLayer::ALL {
        rows.push(MemoryRow::Layer(layer));
        let mut group: Option<&str> = None;
        for memory in listing
            .memories
            .iter()
            .filter(|memory| memory.layer == layer)
        {
            if let Some(name) = memory.group.as_deref()
                && group != Some(name)
            {
                rows.push(MemoryRow::Group {
                    layer,
                    name: name.to_owned(),
                });
                group = Some(name);
            }
            rows.push(MemoryRow::Memory(memory.clone()));
        }
    }
    if !listing.ignored.is_empty() {
        rows.push(MemoryRow::IgnoredHeading);
        rows.extend(listing.ignored.iter().cloned().map(MemoryRow::Ignored));
    }
    rows
}

/// Why `url` cannot be a new memory of `listing`, if it cannot. The daemon
/// checks the URL itself when the memory is saved.
pub fn new_memory_refusal(listing: &MemoryListResult, url: &str) -> Option<String> {
    if listing.memories.iter().any(|memory| memory.url == url) {
        return Some(format!("{url} exists; edit it instead"));
    }
    let complete = url
        .split_once("://")
        .is_some_and(|(_, rest)| !rest.is_empty() && !rest.ends_with('/'));
    (!complete).then(|| {
        "a memory URL is user://<category>, agent://<agent>, machine://<machine>/<name>, \
         or project://<project>/<name>"
            .to_owned()
    })
}

/// The memory's own name: what its URL says after its layer and group.
pub fn memory_name(memory: &MemoryInfo) -> &str {
    let rest = memory
        .url
        .split_once("://")
        .map_or(memory.url.as_str(), |(_, rest)| rest);
    rest.rsplit_once('/').map_or(rest, |(_, name)| name)
}

/// The keywords an agent's mode gives its memory, after its ID.
const MODE_KEYWORDS: [&str; 3] = ["primary", "subagent", "all"];

/// An `agent://` file as an editor shows it: the daemon owns its
/// description and its first keywords (the agent's ID, then its mode) and
/// keeps them whatever a write gives; the other keywords and the body are
/// the user's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentMemoryParts {
    pub description: String,
    /// The daemon's keywords: the agent's ID, then its mode.
    pub fixed: Vec<String>,
    pub extra: Vec<String>,
    pub body: String,
}

/// The parts of `memory`, an `agent://` file whose whole text is `text`,
/// as `memory.list` describes it; `None` for any other memory, an invalid
/// one, or a text without frontmatter, which an editor shows whole.
pub fn agent_memory_parts(memory: &MemoryInfo, text: &str) -> Option<AgentMemoryParts> {
    if memory.layer != MemoryLayer::Agent || memory.invalid.is_some() {
        return None;
    }
    let agent = memory.url.strip_prefix("agent://")?;
    let body = frontmatter_body(text)?;
    let mut keywords = memory.keywords.iter();
    let mut fixed = Vec::new();
    let mut extra = Vec::new();
    for keyword in keywords.by_ref() {
        let owned = match fixed.len() {
            0 => keyword == agent,
            1 => agent != keyword && MODE_KEYWORDS.contains(&keyword.as_str()),
            _ => false,
        };
        if owned {
            fixed.push(keyword.clone());
        } else {
            extra.push(keyword.clone());
            break;
        }
    }
    extra.extend(keywords.cloned());
    Some(AgentMemoryParts {
        description: memory.description.clone().unwrap_or_default(),
        fixed,
        extra,
        body: body.to_owned(),
    })
}

/// What follows the frontmatter of `text`: the lines after the `---` that
/// closes the one that opens it.
fn frontmatter_body(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end() == "---" {
            return Some(&rest[offset..]);
        }
    }
    None
}

impl AgentMemoryParts {
    /// The extra keywords of a comma-separated field.
    pub fn typed_keywords(typed: &str) -> Vec<String> {
        typed
            .split(',')
            .map(str::trim)
            .filter(|keyword| !keyword.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// The whole file to write: the frontmatter with the daemon's
    /// description and keywords (it keeps them anyway), the extra
    /// keywords, then the body. Scalars are double-quoted JSON strings,
    /// which YAML reads as the same text.
    pub fn text(&self) -> String {
        let quote = |value: &str| serde_json::to_string(value).unwrap_or_default();
        let mut text = format!("---\ndescription: {}\n", quote(&self.description));
        let keywords = self
            .fixed
            .iter()
            .chain(&self.extra)
            .map(|keyword| quote(keyword))
            .collect::<Vec<_>>();
        if !keywords.is_empty() {
            text.push_str(&format!("keywords: [{}]\n", keywords.join(", ")));
        }
        text.push_str("---\n");
        text.push_str(&self.body);
        text
    }
}

/// The heading of a layer.
pub const fn layer_title(layer: MemoryLayer) -> &'static str {
    match layer {
        MemoryLayer::User => "User",
        MemoryLayer::Agent => "Agents",
        MemoryLayer::Machine => "Machines",
        MemoryLayer::Project => "Projects",
        MemoryLayer::Unknown => "Other",
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::MemoryListResult;

    use super::{
        AgentMemoryParts, MemoryRow, agent_memory_parts, memory_rows, new_memory_refusal,
        saved_status,
    };

    fn listing() -> MemoryListResult {
        serde_json::from_value(serde_json::json!({
            "memories": [
                {"url": "project://shop/build", "layer": "project", "group": "shop",
                 "revision": "r1", "size": 10, "description": "how to build"}
            ],
            "ignored": []
        }))
        .expect("listing")
    }

    #[test]
    fn memories_group_under_their_layer_and_project() {
        let rows = memory_rows(&listing());
        let group = rows
            .iter()
            .position(|row| matches!(row, MemoryRow::Group { name, .. } if name == "shop"))
            .expect("the project's group");
        assert!(
            matches!(&rows[group + 1], MemoryRow::Memory(memory) if memory.url == "project://shop/build")
        );
        assert_eq!(rows[group + 1].new_prefix(), "project://shop/");
    }

    #[test]
    fn a_new_memory_needs_a_complete_free_url() {
        assert!(new_memory_refusal(&listing(), "project://shop/test").is_none());
        assert!(new_memory_refusal(&listing(), "project://shop/build").is_some());
        assert!(new_memory_refusal(&listing(), "project://shop/").is_some());
    }

    fn agent_memory(keywords: &[&str]) -> cookie_agent_protocol::MemoryInfo {
        serde_json::from_value(serde_json::json!({
            "url": "agent://reviewer", "layer": "agent", "revision": "r1", "size": 10,
            "description": "Memory of the `reviewer` agent: reviews diffs",
            "keywords": keywords
        }))
        .expect("memory")
    }

    #[test]
    fn an_agent_file_splits_the_daemons_keywords_from_the_users() {
        let text = "---\ndescription: x\nkeywords: [reviewer, subagent, rust]\n---\nPrefers small diffs.\n";
        let parts = agent_memory_parts(&agent_memory(&["reviewer", "subagent", "rust"]), text)
            .expect("an agent file");
        assert_eq!(parts.fixed, ["reviewer", "subagent"]);
        assert_eq!(parts.extra, ["rust"]);
        assert_eq!(parts.body, "Prefers small diffs.\n");
        assert_eq!(
            parts.description,
            "Memory of the `reviewer` agent: reviews diffs"
        );
        // A file whose keywords lost the mode keeps the rest as the user's.
        let parts = agent_memory_parts(&agent_memory(&["reviewer", "rust", "subagent"]), text)
            .expect("an agent file");
        assert_eq!(parts.fixed, ["reviewer"]);
        assert_eq!(parts.extra, ["rust", "subagent"]);
        assert!(agent_memory_parts(&agent_memory(&["reviewer"]), "no frontmatter").is_none());
        let mut project = agent_memory(&[]);
        project.layer = cookie_agent_protocol::MemoryLayer::Project;
        assert!(agent_memory_parts(&project, text).is_none());
    }

    #[test]
    fn an_agent_file_is_written_with_its_parts_quoted() {
        let parts = AgentMemoryParts {
            description: "Memory of the `reviewer` agent: \"diffs\"".into(),
            fixed: vec!["reviewer".into(), "subagent".into()],
            extra: AgentMemoryParts::typed_keywords(" rust , ,a: b"),
            body: "Body.\n".into(),
        };
        assert_eq!(
            parts.text(),
            "---\ndescription: \"Memory of the `reviewer` agent: \\\"diffs\\\"\"\n\
             keywords: [\"reviewer\", \"subagent\", \"rust\", \"a: b\"]\n---\nBody.\n"
        );
    }

    #[test]
    fn a_save_says_what_the_daemon_kept() {
        assert_eq!(
            saved_status("user://style", false, None),
            "Saved user://style."
        );
        assert_eq!(
            saved_status(
                "agent://reviewer",
                true,
                Some("The daemon kept its description.")
            ),
            "Created agent://reviewer. The daemon kept its description."
        );
    }
}
