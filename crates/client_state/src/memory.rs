//! The `/memory` panel's rules every client shares: the memories as rows
//! by layer and group, what a new memory starts from and where, what
//! `memory.write` answered, and how a delete's refusal reads.

use cookie_agent_protocol::{
    ClientError, MemoryIgnoredEntry, MemoryInfo, MemoryLayer, MemoryListResult, MemoryWriteResult,
    errors,
};

/// The text a new memory starts from.
pub const NEW_MEMORY: &str = "---\ndescription: \nkeywords: []\n---\n";

/// What `memory.write` answered.
#[derive(Debug, Eq, PartialEq)]
pub enum MemoryWriteOutcome {
    Saved,
    /// The text is not a valid memory: why.
    Invalid(String),
    /// The memory changed, appeared, or went away since it was read.
    Changed,
    Failed(String),
}

impl MemoryWriteOutcome {
    pub fn from_result(result: Result<MemoryWriteResult, ClientError>) -> Self {
        match result {
            Ok(_) => Self::Saved,
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

    use super::{MemoryRow, memory_rows, new_memory_refusal};

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
}
