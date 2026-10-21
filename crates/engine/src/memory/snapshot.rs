//! Taking a memory snapshot (`docs/site/specs/memory.md` section 7.1): the
//! user categories, the run's agent's file, the index of the session's
//! machines' memories, and the session's project's overview and index
//! (`docs/site/specs/projects.md` section 7), each fitted to its limit on
//! rendered text.

use std::cmp::Reverse;

use cookie_agent_protocol::{
    AgentId, AttachmentName, MachineName, MemorySnapshot, MemorySnapshotAgent,
    MemorySnapshotCategory, MemorySnapshotIndexEntry, MemorySnapshotMachine, MemorySnapshotMemory,
    MemorySnapshotProject, ProjectName,
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
    /// The session's project. Ignored for a delegated run.
    pub(crate) project: Option<ProjectName>,
    pub(crate) limits: SnapshotLimits,
}

/// A memory's index line, with its file's modification time for ordering.
type Dated = (std::time::SystemTime, MemorySnapshotIndexEntry);

/// `entries` newest first, then by URL.
fn newest_first(entries: &mut [Dated]) {
    entries.sort_by(|left, right| {
        (Reverse(left.0), &left.1.url).cmp(&(Reverse(right.0), &right.1.url))
    });
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
        // The project's overview comes out of the user section's room, and
        // its index out of the machines index's, each before them: the
        // session's own project matters most.
        let (overview, project_memories) = match &request.project {
            Some(project) => self.project_entries(project, limits)?,
            None => (None, Vec::new()),
        };
        let overview_bytes = match (&request.project, &overview) {
            (Some(project), Some(overview)) => render::project_overview(project, overview).len(),
            _ => 0,
        };
        let (overview, overview_bytes, project_memories) = if overview_bytes > limits.user_bytes {
            // Too long to show whole: listed in the index instead.
            let project = request.project.as_ref().expect("an overview has a project");
            let url = super::projects::overview_url(
                &super::url::MemoryName::new(project.as_str())
                    .expect("project names are memory names"),
            );
            let mut memories = project_memories;
            let overview = overview.expect("a measured overview");
            memories.insert(
                0,
                (
                    std::time::SystemTime::now(),
                    MemorySnapshotIndexEntry {
                        url: url.to_string(),
                        description: overview.description,
                        keywords: overview.keywords,
                    },
                ),
            );
            (None, 0, memories)
        } else {
            (overview, overview_bytes, project_memories)
        };
        let user_limits = SnapshotLimits {
            user_bytes: limits.user_bytes - overview_bytes,
            ..limits
        };
        let (user, user_omitted) = self.user_entries(user_limits)?;
        let project = request.project.as_ref().map(|project| {
            // Listing one more can be shorter than counting it, so every
            // count is measured.
            let fits = (1..=project_memories.len())
                .rev()
                .find(|&count| {
                    render::project_index(&project_index(project, &project_memories, count)).len()
                        <= limits.index_bytes
                })
                .unwrap_or(0);
            let mut entry = project_index(project, &project_memories, fits);
            entry.overview = overview;
            entry
        });
        let project_bytes = project
            .as_ref()
            .map_or(0, |project| render::project_index(project).len());
        let machine_limits = SnapshotLimits {
            index_bytes: limits.index_bytes.saturating_sub(project_bytes),
            ..limits
        };
        let machines = self.machine_entries(&request.machines, machine_limits)?;
        Ok(MemorySnapshot {
            delegated: false,
            writable: request.writable,
            user,
            user_omitted,
            agent: Some(agent),
            machines,
            project,
        })
    }

    /// The project's overview, whole, when it exists, is valid, and has a
    /// body; and its other valid memories, newest first.
    fn project_entries(
        &self,
        project: &ProjectName,
        limits: SnapshotLimits,
    ) -> Result<(Option<MemorySnapshotMemory>, Vec<Dated>), MemoryError> {
        let name =
            super::url::MemoryName::new(project.as_str()).expect("project names are memory names");
        let mut overview = None;
        let mut memories = Vec::new();
        for entry in self
            .list(&ListScope::Project(name), limits.max_file_bytes)?
            .entries
        {
            let Ok(document) = entry.document else {
                continue;
            };
            if super::projects::is_overview(&entry.url) {
                if !document.body().trim().is_empty() {
                    overview = Some(MemorySnapshotMemory {
                        description: document.description().to_owned(),
                        keywords: document.keywords().to_vec(),
                        body: document.body().to_owned(),
                    });
                }
                continue;
            }
            memories.push((
                entry.modified,
                MemorySnapshotIndexEntry {
                    url: entry.url.to_string(),
                    description: document.description().to_owned(),
                    keywords: document.keywords().to_vec(),
                },
            ));
        }
        newest_first(&mut memories);
        Ok((overview, memories))
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
            newest_first(&mut entries);
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

/// The project entry with the first `count` of `memories` listed and the
/// rest counted.
fn project_index(project: &ProjectName, memories: &[Dated], count: usize) -> MemorySnapshotProject {
    MemorySnapshotProject {
        project: project.clone(),
        overview: None,
        memories: memories[..count]
            .iter()
            .map(|(_, entry)| entry.clone())
            .collect(),
        omitted: (memories.len() - count) as u64,
    }
}

#[cfg(test)]
mod tests;
