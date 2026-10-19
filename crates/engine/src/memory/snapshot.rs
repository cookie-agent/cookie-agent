//! Taking a memory snapshot (`docs/site/specs/memory.md` section 7.1): the
//! user categories, the run's agent's file, and the index of the session's
//! machines' memories, each fitted to its limit on rendered text.

use std::cmp::Reverse;

use cookie_agent_protocol::{
    AgentId, AttachmentName, MachineName, MemorySnapshot, MemorySnapshotAgent,
    MemorySnapshotCategory, MemorySnapshotIndexEntry, MemorySnapshotMachine, MemorySnapshotMemory,
};

use super::{
    MemoryError, MemoryRoot, render,
    store::{ListScope, StoreError},
    url::{MemoryLayer, MemoryUrl},
};

/// The `[memory]` limits a snapshot is taken under, read when it is taken.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotLimits {
    pub(crate) max_file_bytes: usize,
    pub(crate) user_bytes: usize,
    pub(crate) index_bytes: usize,
}

impl SnapshotLimits {
    pub(crate) fn of(config: &cookie_agent_config::MemoryConfig) -> Self {
        Self {
            max_file_bytes: config.max_file_bytes,
            user_bytes: config.snapshot_user_bytes,
            index_bytes: config.snapshot_index_bytes,
        }
    }
}

/// What a run needs from memory.
#[derive(Clone, Debug)]
pub(crate) struct SnapshotRequest {
    pub(crate) agent: AgentId,
    pub(crate) delegated: bool,
    pub(crate) writable: bool,
    /// The session's machines in order, each with the session's attachments
    /// on it. Ignored for a delegated run.
    pub(crate) machines: Vec<(MachineName, Vec<AttachmentName>)>,
    pub(crate) limits: SnapshotLimits,
}

impl MemoryRoot {
    /// Reads memory into a snapshot for `request`. Invalid files are left
    /// out; only a failure to read a folder fails.
    pub(crate) fn snapshot(
        &self,
        request: &SnapshotRequest,
    ) -> Result<MemorySnapshot, MemoryError> {
        let limits = request.limits;
        let agent = self.agent_entry(&request.agent, limits.max_file_bytes)?;
        if request.delegated {
            return Ok(MemorySnapshot {
                delegated: true,
                writable: request.writable,
                agent: Some(agent),
                ..MemorySnapshot::default()
            });
        }
        let (user, user_omitted) = self.user_entries(limits)?;
        let machines = self.machine_entries(&request.machines, limits)?;
        Ok(MemorySnapshot {
            delegated: false,
            writable: request.writable,
            user,
            user_omitted,
            agent: Some(agent),
            machines,
        })
    }

    /// Every valid category in name order, as many whole as fit
    /// `user_bytes` of rendered user section; the rest by name.
    fn user_entries(
        &self,
        limits: SnapshotLimits,
    ) -> Result<(Vec<MemorySnapshotCategory>, Vec<String>), MemoryError> {
        let categories = self
            .list(&ListScope::Layer(MemoryLayer::User), limits.max_file_bytes)?
            .entries
            .into_iter()
            .filter_map(|entry| {
                let document = entry.document.ok()?;
                Some(MemorySnapshotCategory {
                    category: entry.url.name().to_owned(),
                    description: document.description().to_owned(),
                    keywords: document.keywords().to_vec(),
                    body: document.body().to_owned(),
                })
            })
            .collect::<Vec<_>>();
        let names = categories
            .iter()
            .map(|category| category.category.clone())
            .collect::<Vec<_>>();
        // Each check renders the section as it would end with this file the
        // last one shown, so the section that is kept was measured whole.
        let fits = (1..=categories.len())
            .take_while(|&shown| {
                render::user_section(&categories[..shown], &names[shown..]).len()
                    <= limits.user_bytes
            })
            .last()
            .unwrap_or(0);
        let mut user = categories;
        user.truncate(fits);
        Ok((user, names[fits..].to_vec()))
    }

    /// The run's agent's file, whole, when it exists and is valid; marked
    /// omitted when its rendered content exceeds `max_file_bytes`.
    fn agent_entry(
        &self,
        agent: &AgentId,
        max_file_bytes: usize,
    ) -> Result<MemorySnapshotAgent, MemoryError> {
        let url = MemoryUrl::Agent {
            agent: agent.clone(),
        };
        let document = match self.read(&url, max_file_bytes) {
            Ok(read) => read.entry.document.ok(),
            Err(StoreError::NotFound { .. }) => None,
            Err(StoreError::Memory(error)) => return Err(error),
            Err(_) => None,
        };
        let mut entry = MemorySnapshotAgent {
            agent: agent.clone(),
            memory: None,
            omitted: false,
        };
        if let Some(document) = document {
            if render::agent_content_bytes(document.description(), document.body())
                <= max_file_bytes
            {
                entry.memory = Some(MemorySnapshotMemory {
                    description: document.description().to_owned(),
                    keywords: document.keywords().to_vec(),
                    body: document.body().to_owned(),
                });
            } else {
                entry.omitted = true;
            }
        }
        Ok(entry)
    }

    /// One entry per machine, its valid memories newest first, as many as
    /// fit `index_bytes` of rendered machines section over all machines;
    /// the rest counted.
    fn machine_entries(
        &self,
        machines: &[(MachineName, Vec<AttachmentName>)],
        limits: SnapshotLimits,
    ) -> Result<Vec<MemorySnapshotMachine>, MemoryError> {
        let mut all = Vec::with_capacity(machines.len());
        for (machine, _) in machines {
            let mut entries = self
                .list(&ListScope::Machine(machine.clone()), limits.max_file_bytes)?
                .entries
                .into_iter()
                .filter_map(|entry| {
                    let document = entry.document.ok()?;
                    Some((
                        entry.modified,
                        MemorySnapshotIndexEntry {
                            url: entry.url.to_string(),
                            description: document.description().to_owned(),
                            keywords: document.keywords().to_vec(),
                        },
                    ))
                })
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| {
                (Reverse(left.0), &left.1.url).cmp(&(Reverse(right.0), &right.1.url))
            });
            all.push(
                entries
                    .into_iter()
                    .map(|(_, entry)| entry)
                    .collect::<Vec<_>>(),
            );
        }
        let total = all.iter().map(Vec::len).sum::<usize>();
        let shown = |count: usize| {
            let mut left = count;
            machines
                .iter()
                .zip(&all)
                .map(|((machine, attachments), memories)| {
                    let take = left.min(memories.len());
                    left -= take;
                    MemorySnapshotMachine {
                        machine: machine.clone(),
                        attachments: attachments.clone(),
                        memories: memories[..take].to_vec(),
                        omitted: (memories.len() - take) as u64,
                    }
                })
                .collect::<Vec<_>>()
        };
        let fits = (1..=total)
            .take_while(|&count| {
                render::machines_section(&shown(count)).len() <= limits.index_bytes
            })
            .last()
            .unwrap_or(0);
        Ok(shown(fits))
    }
}

#[cfg(test)]
mod tests;
