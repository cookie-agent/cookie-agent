//! The agent memory files the daemon keeps (`docs/site/specs/memory.md`
//! section 16.7): one `agents/<agent>.md` for every agent of the agent
//! registries that is not internal, whose description and two keywords the
//! daemon owns.
//!
//! [`MemoryRoot::sync_agents`] brings the folder in line with the
//! registries under the memory lock: it creates a missing file with an
//! empty body, rewrites the frontmatter of a file whose description or
//! daemon keywords are off (keeping its body and any extra keywords), and
//! removes the file of an agent no registry has any more when the daemon
//! made it and nobody has touched it since. Doing nothing writes nothing
//! and signals nothing.

use std::collections::BTreeMap;

use cookie_agent_identity::AgentId;

use super::{
    AGENTS_DIR, MEMORY_EXTENSION, MemoryError, MemoryRoot,
    file::{InvalidMemory, MAX_DESCRIPTION_CHARS, MAX_KEYWORDS, MemoryDocument},
    store::{read_bytes, read_dir_sorted, write_file_unsynced_folder},
    url::MemoryUrl,
};

/// The keywords an agent's mode can give its memory.
const MODE_KEYWORDS: [&str; 3] = ["primary", "subagent", "all"];

/// What the daemon sets in one agent's memory file: the description, and
/// the mode keyword that follows the agent ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentMemorySpec {
    description: String,
    mode: &'static str,
}

/// What the memory store knows about the agent of an `agent://` URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AgentMemory {
    /// The daemon keeps this agent's file and owns its description.
    Managed(AgentMemorySpec),
    /// The registry has the agent, but the daemon keeps no file for it
    /// (an internal agent).
    Registered,
    /// No registry has the agent.
    Unknown,
}

impl AgentMemory {
    /// Whether a file for the agent may be created.
    pub(crate) const fn exists(&self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

fn daemon_keywords(agent: &AgentId, mode: &str) -> Vec<String> {
    let mut keywords = vec![agent.to_string()];
    if agent.as_str() != mode {
        keywords.push(mode.to_owned());
    }
    keywords
}

/// The start of every description the daemon writes for `agent`.
fn description_prefix(agent: &AgentId) -> String {
    format!("Memory of the `{agent}` agent")
}

/// `text` cut to `max` characters, ending with `…` when it was cut.
fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some(_) => {
            let end = text
                .char_indices()
                .nth(max - 1)
                .map_or(text.len(), |(index, _)| index);
            format!("{}…", text[..end].trim_end())
        }
    }
}

impl AgentMemorySpec {
    /// The spec of `agent`, from its agent document's description and its
    /// mode keyword (`primary`, `subagent`, or `all`). The description is
    /// ``Memory of the `<agent>` agent: <description>``, on one line: line
    /// breaks, tabs, and other control characters become spaces, runs of
    /// whitespace one space, and the whole is cut to 200 characters.
    pub(crate) fn new(agent: &AgentId, document_description: &str, mode: &'static str) -> Self {
        let words = document_description
            .split(|character: char| character.is_whitespace() || character.is_control())
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let prefix = description_prefix(agent);
        let description = if words.is_empty() {
            prefix
        } else {
            format!("{prefix}: {words}")
        };
        Self {
            description: cut(&description, MAX_DESCRIPTION_CHARS),
            mode,
        }
    }

    /// The daemon's two keywords, the agent ID and the mode; one when the
    /// agent ID is the mode's name.
    pub(crate) fn daemon_keywords(&self, agent: &AgentId) -> Vec<String> {
        daemon_keywords(agent, self.mode)
    }

    /// The daemon's two keywords, then `existing` without them, without
    /// any other mode keyword, and without repeats (all compared
    /// case-insensitively), at most 32 in all.
    pub(crate) fn keywords(&self, agent: &AgentId, existing: &[String]) -> Vec<String> {
        let mut keywords = self.daemon_keywords(agent);
        let mut seen = keywords
            .iter()
            .map(|keyword| keyword.to_lowercase())
            .chain(MODE_KEYWORDS.iter().map(|mode| (*mode).to_owned()))
            .collect::<Vec<_>>();
        for keyword in existing {
            let lower = keyword.to_lowercase();
            if keywords.len() == MAX_KEYWORDS || seen.contains(&lower) {
                continue;
            }
            seen.push(lower);
            keywords.push(keyword.clone());
        }
        keywords
    }

    /// `document` with the daemon's description and keywords; its stamps
    /// stay.
    pub(crate) fn apply(
        &self,
        agent: &AgentId,
        document: &MemoryDocument,
    ) -> Result<MemoryDocument, InvalidMemory> {
        Ok(MemoryDocument::new(
            self.description.clone(),
            self.keywords(agent, document.keywords()),
            document.body(),
        )?
        .with_stamps(document.created(), document.modified()))
    }

    /// A new file's text: the daemon's frontmatter, stamped now, and an
    /// empty body.
    fn new_file(&self, agent: &AgentId) -> Result<String, InvalidMemory> {
        let now = Some(jiff::Timestamp::now());
        MemoryDocument::new(self.description.clone(), self.daemon_keywords(agent), "")
            .map(|document| document.with_stamps(now, now).render())
    }
}

/// `text`, a valid memory file, with the frontmatter of `document`, its
/// stamps included, in place of its own; the body's bytes stay as they are.
pub(crate) fn with_frontmatter(text: &str, document: &MemoryDocument) -> Option<String> {
    let body = &text[MemoryDocument::frontmatter_len(text)?..];
    let mut replaced =
        MemoryDocument::new(document.description(), document.keywords().to_vec(), "")
            .ok()?
            .with_stamps(document.created(), document.modified())
            .render();
    replaced.push_str(body);
    Some(replaced)
}

/// Whether `bytes` are an agent file exactly as the daemon writes one, with
/// an empty body: its description starts as the daemon's do and its
/// keywords are only the daemon's two. Such a file was made by the daemon
/// and never touched, so it goes when its agent does.
fn untouched(agent: &AgentId, bytes: &[u8]) -> bool {
    let Ok(document) = MemoryDocument::parse(bytes) else {
        return false;
    };
    let prefix = description_prefix(agent);
    let described = document.description() == prefix
        || document
            .description()
            .strip_prefix(&prefix)
            .is_some_and(|rest| rest.starts_with(": "));
    described
        && document.body().is_empty()
        && MODE_KEYWORDS
            .iter()
            .any(|mode| daemon_keywords(agent, mode) == document.keywords())
        && document.render().as_bytes() == bytes
}

/// What [`MemoryRoot::sync_agents`] changed, by agent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AgentSync {
    pub(crate) created: Vec<AgentId>,
    pub(crate) updated: Vec<AgentId>,
    pub(crate) removed: Vec<AgentId>,
}

impl AgentSync {
    pub(crate) fn is_empty(&self) -> bool {
        self.created.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

impl MemoryRoot {
    /// Brings the agent files in line with `desired`, the agents the
    /// daemon keeps a file for, which is asked for once the memory lock is
    /// held so that the latest registries win a race between two syncs.
    /// A file that is invalid, or would grow past `agent_max_bytes`, is left
    /// as it is. Nothing happens while the memory folder does not exist:
    /// the next start's first start creates it.
    pub(crate) fn sync_agents(
        &self,
        desired: impl FnOnce() -> BTreeMap<AgentId, AgentMemorySpec>,
        agent_max_bytes: usize,
    ) -> Result<AgentSync, MemoryError> {
        if !self.root.is_dir() {
            return Ok(AgentSync::default());
        }
        let lock = self.lock()?;
        let desired = desired();
        let mut sync = AgentSync::default();
        for (agent, spec) in &desired {
            let path = self.file_path(&MemoryUrl::Agent {
                agent: agent.clone(),
            });
            let Some((bytes, metadata)) = read_bytes(&path)? else {
                if let Ok(text) = spec.new_file(agent) {
                    write_file_unsynced_folder(&path, text.as_bytes())?;
                    sync.created.push(agent.clone());
                }
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Ok(document) = MemoryDocument::parse(&bytes) else {
                continue;
            };
            let Ok(managed) = spec.apply(agent, &document) else {
                continue;
            };
            if managed.description() == document.description()
                && managed.keywords() == document.keywords()
            {
                continue;
            }
            let Some(text) = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|text| with_frontmatter(text, &managed))
            else {
                continue;
            };
            if text.len() > agent_max_bytes {
                continue;
            }
            write_file_unsynced_folder(&path, text.as_bytes())?;
            sync.updated.push(agent.clone());
        }
        let dir = self.root.join(AGENTS_DIR);
        for (file_name, kind) in read_dir_sorted(&dir)? {
            let Some(stem) = file_name.strip_suffix(&format!(".{MEMORY_EXTENSION}")) else {
                continue;
            };
            let Ok(MemoryUrl::Agent { agent }) = MemoryUrl::parse(&format!("agent://{stem}"))
            else {
                continue;
            };
            if !kind.is_file() || desired.contains_key(&agent) {
                continue;
            }
            let path = dir.join(&file_name);
            let Some((bytes, _)) = read_bytes(&path)? else {
                continue;
            };
            if untouched(&agent, &bytes) {
                std::fs::remove_file(&path).map_err(super::store::io_error(&path))?;
                sync.removed.push(agent);
            }
        }
        // One folder fsync for every file written or removed above.
        #[cfg(unix)]
        if !sync.is_empty() {
            std::fs::File::open(&dir)
                .and_then(|folder| folder.sync_all())
                .map_err(super::store::io_error(&dir))?;
        }
        drop(lock);
        if !sync.is_empty() {
            self.changed();
        }
        Ok(sync)
    }
}

#[cfg(test)]
mod tests;
