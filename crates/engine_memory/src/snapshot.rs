//! Taking a memory snapshot (`docs/site/specs/memory.md` section 7.1): the
//! user categories, the run's agent's file, the index of the session's
//! machines' memories, and the session's project's overview and index
//! (`docs/site/specs/projects.md` section 7), each fitted to its limit on
//! rendered text. Each memory is first filtered by the run's `memory` rules
//! (section 7.5): one they allow is shown, one they ask for is listed by
//! URL and description only, and one they deny is left out uncounted.
//! Every memory shown carries when it was modified, its size, and its
//! kind's limit (section 16.9.7); the snapshot records when it was taken,
//! so its ages render the same every time.

use std::{cmp::Reverse, sync::Arc};

use cookie_agent_protocol::{
    AgentId, AttachmentName, MachineName, MemoryDocumentStatus, MemorySnapshot,
    MemorySnapshotAgent, MemorySnapshotCategory, MemorySnapshotIndexEntry, MemorySnapshotMachine,
    MemorySnapshotMemory, MemorySnapshotProject, ProjectName, Sha256Digest,
};
use jiff::Timestamp;

use super::{
    MemoryError, MemoryRoot,
    file::{MemoryDocument, MemoryRevision},
    limits::MemoryLimits,
    render,
    search::Visibility,
    store::{ListScope, MemoryEntry, StoreError},
    url::{MemoryLayer, MemoryUrl},
};

/// What the run's `memory` rules let it read of each memory URL.
#[derive(Clone)]
pub struct ReadRules(Arc<dyn Fn(&MemoryUrl) -> Visibility + Send + Sync>);

impl ReadRules {
    pub fn new(rules: impl Fn(&MemoryUrl) -> Visibility + Send + Sync + 'static) -> Self {
        Self(Arc::new(rules))
    }

    /// Rules that let the run read every memory.
    #[cfg(any(test, feature = "test-support"))]
    pub fn all() -> Self {
        Self::new(|_| Visibility::Full)
    }

    fn of(&self, url: &MemoryUrl) -> Visibility {
        (self.0)(url)
    }
}

impl std::fmt::Debug for ReadRules {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ReadRules")
    }
}

/// What a run needs from memory.
#[derive(Clone, Debug)]
pub struct SnapshotRequest {
    pub agent: AgentId,
    pub delegated: bool,
    pub writable: bool,
    /// The session's machines in order, each with the session's attachments
    /// on it. Ignored for a delegated run.
    pub machines: Vec<(MachineName, Vec<AttachmentName>)>,
    /// The session's project. Ignored for a delegated run.
    pub project: Option<ProjectName>,
    pub limits: MemoryLimits,
    /// When the snapshot is taken.
    pub now: Timestamp,
    /// What the run's `memory` rules let it read.
    pub read: ReadRules,
    /// The fingerprint of those rules, which the snapshot records.
    pub rules: Option<Sha256Digest>,
}

/// The user section: the categories shown, the names of those that did not
/// fit, and the categories listed only.
type UserEntries = (
    Vec<MemorySnapshotCategory>,
    Vec<String>,
    Vec<MemorySnapshotIndexEntry>,
);

/// A memory's index line, with when it was modified for ordering.
type Dated = (Timestamp, MemorySnapshotIndexEntry);

/// `entries` newest first, then by URL.
fn newest_first(entries: &mut [Dated]) {
    entries.sort_by(|left, right| {
        (Reverse(left.0), &left.1.url).cmp(&(Reverse(right.0), &right.1.url))
    });
}

/// A snapshot and the memories it shows whole, with the revisions it
/// showed, which the session has then seen (section 16.9.8).
#[derive(Clone, Debug)]
pub struct TakenSnapshot {
    pub snapshot: MemorySnapshot,
    pub shown: Vec<(MemoryUrl, MemoryRevision)>,
}

/// How full `entry` is under `limits`, and when it was modified.
fn status(entry: &MemoryEntry, limits: &MemoryLimits) -> MemoryDocumentStatus {
    MemoryDocumentStatus {
        modified: Some(entry.modified),
        size: Some(entry.size),
        max_size: Some(limits.max_bytes(&entry.url) as u64),
    }
}

/// `entry`'s index line.
fn index_entry(
    entry: &MemoryEntry,
    document: &MemoryDocument,
    limits: &MemoryLimits,
) -> MemorySnapshotIndexEntry {
    MemorySnapshotIndexEntry {
        url: entry.url.to_string(),
        description: document.description().to_owned(),
        keywords: document.keywords().to_vec(),
        status: status(entry, limits),
    }
}

/// `entry`'s content, whole.
fn whole(
    entry: &MemoryEntry,
    document: &MemoryDocument,
    limits: &MemoryLimits,
) -> MemorySnapshotMemory {
    MemorySnapshotMemory {
        description: document.description().to_owned(),
        keywords: document.keywords().to_vec(),
        body: document.body().to_owned(),
        status: status(entry, limits),
    }
}

impl MemoryRoot {
    /// Reads memory into a snapshot for `request`. Invalid files are left
    /// out; only a failure to read a folder fails.
    pub fn snapshot(&self, request: &SnapshotRequest) -> Result<TakenSnapshot, MemoryError> {
        let limits = request.limits;
        let mut shown = Vec::new();
        let agent = self.agent_entry(&request.agent, &limits, &request.read, &mut shown)?;
        if request.delegated {
            return Ok(TakenSnapshot {
                snapshot: MemorySnapshot {
                    delegated: true,
                    writable: request.writable,
                    agent: Some(agent),
                    rules: request.rules.clone(),
                    taken_at: Some(request.now),
                    ..MemorySnapshot::default()
                },
                shown,
            });
        }
        // The project's overview comes out of the user section's room, and
        // its index out of the machines index's, each before them: the
        // session's own project matters most.
        let (overview, project_memories) = match &request.project {
            Some(project) => self.project_entries(project, &limits, &request.read)?,
            None => (None, Vec::new()),
        };
        let overview_bytes = match (&request.project, &overview) {
            (Some(project), Some((overview, _))) => {
                render::project_overview(project, overview, Some(request.now)).len()
            }
            _ => 0,
        };
        let (overview, overview_bytes, project_memories) =
            if overview_bytes > limits.snapshot_user_bytes {
                // Too long to show whole: listed in the index instead.
                let project = request.project.as_ref().expect("an overview has a project");
                let url = super::projects::overview_url(
                    &super::url::MemoryName::new(project.as_str())
                        .expect("project names are memory names"),
                );
                let mut memories = project_memories;
                let (overview, _) = overview.expect("a measured overview");
                memories.insert(
                    0,
                    (
                        request.now,
                        MemorySnapshotIndexEntry {
                            url: url.to_string(),
                            description: overview.description,
                            keywords: overview.keywords,
                            status: overview.status,
                        },
                    ),
                );
                (None, 0, memories)
            } else {
                if let Some((_, revision)) = &overview {
                    let project = request.project.as_ref().expect("an overview has a project");
                    shown.push((
                        super::projects::overview_url(
                            &super::url::MemoryName::new(project.as_str())
                                .expect("project names are memory names"),
                        ),
                        revision.clone(),
                    ));
                }
                (
                    overview.map(|(overview, _)| overview),
                    overview_bytes,
                    project_memories,
                )
            };
        let user_limits = MemoryLimits {
            snapshot_user_bytes: limits.snapshot_user_bytes - overview_bytes,
            ..limits
        };
        let (user, user_omitted, user_listed) =
            self.user_entries(&user_limits, &request.read, &mut shown, request.now)?;
        let project = request.project.as_ref().map(|project| {
            // Listing one more can be shorter than counting it, so every
            // count is measured.
            let fits = (1..=project_memories.len())
                .rev()
                .find(|&count| {
                    render::project_index(
                        &project_index(project, &project_memories, count),
                        Some(request.now),
                    )
                    .len()
                        <= limits.snapshot_index_bytes
                })
                .unwrap_or(0);
            let mut entry = project_index(project, &project_memories, fits);
            entry.overview = overview;
            entry
        });
        let project_bytes = project.as_ref().map_or(0, |project| {
            render::project_index(project, Some(request.now)).len()
        });
        let machine_limits = MemoryLimits {
            snapshot_index_bytes: limits.snapshot_index_bytes.saturating_sub(project_bytes),
            ..limits
        };
        let machines = self.machine_entries(
            &request.machines,
            &machine_limits,
            &request.read,
            request.now,
        )?;
        Ok(TakenSnapshot {
            snapshot: MemorySnapshot {
                delegated: false,
                writable: request.writable,
                user,
                user_omitted,
                user_listed,
                agent: Some(agent),
                machines,
                project,
                rules: request.rules.clone(),
                taken_at: Some(request.now),
            },
            shown,
        })
    }

    /// The project's overview, whole, when it exists, is valid, has a body,
    /// and the rules allow it; and its other valid memories the rules let
    /// the run see, newest first, the overview among them when the rules
    /// ask before reading it.
    #[allow(clippy::type_complexity)]
    fn project_entries(
        &self,
        project: &ProjectName,
        limits: &MemoryLimits,
        read: &ReadRules,
    ) -> Result<(Option<(MemorySnapshotMemory, MemoryRevision)>, Vec<Dated>), MemoryError> {
        let name =
            super::url::MemoryName::new(project.as_str()).expect("project names are memory names");
        let mut overview = None;
        let mut memories = Vec::new();
        for entry in self.list(&ListScope::Project(name))?.entries {
            let Ok(document) = &entry.document else {
                continue;
            };
            let visibility = read.of(&entry.url);
            if visibility == Visibility::Hidden {
                continue;
            }
            if super::projects::is_overview(&entry.url) && visibility == Visibility::Full {
                if !document.body().trim().is_empty() {
                    overview = Some((whole(&entry, document, limits), entry.revision.clone()));
                }
                continue;
            }
            memories.push((entry.modified, index_entry(&entry, document, limits)));
        }
        newest_first(&mut memories);
        Ok((overview, memories))
    }

    /// Every valid category the rules allow in name order, as many whole as
    /// fit `user_bytes` of rendered user section, the rest by name; and the
    /// categories the rules ask before reading, listed.
    fn user_entries(
        &self,
        limits: &MemoryLimits,
        read: &ReadRules,
        shown: &mut Vec<(MemoryUrl, MemoryRevision)>,
        now: Timestamp,
    ) -> Result<UserEntries, MemoryError> {
        let mut categories = Vec::new();
        let mut revisions = Vec::new();
        let mut listed = Vec::new();
        for entry in self.list(&ListScope::Layer(MemoryLayer::User))?.entries {
            let Ok(document) = &entry.document else {
                continue;
            };
            match read.of(&entry.url) {
                Visibility::Full => {
                    categories.push(MemorySnapshotCategory {
                        category: entry.url.name().to_owned(),
                        description: document.description().to_owned(),
                        keywords: document.keywords().to_vec(),
                        body: document.body().to_owned(),
                        status: status(&entry, limits),
                    });
                    revisions.push((entry.url.clone(), entry.revision.clone()));
                }
                Visibility::Summary => listed.push(index_entry(&entry, document, limits)),
                Visibility::Hidden => {}
            }
        }
        let names = categories
            .iter()
            .map(|category| category.category.clone())
            .collect::<Vec<_>>();
        // Each check renders the section as it would end with this file the
        // last one shown, so the section that is kept was measured whole.
        let fits = (1..=categories.len())
            .take_while(|&count| {
                render::user_section(&categories[..count], &names[count..], &listed, Some(now))
                    .len()
                    <= limits.snapshot_user_bytes
            })
            .last()
            .unwrap_or(0);
        let mut user = categories;
        user.truncate(fits);
        shown.extend(revisions.into_iter().take(fits));
        Ok((user, names[fits..].to_vec(), listed))
    }

    /// The run's agent's file, whole, when it exists, is valid, and the
    /// rules allow it; marked omitted when its rendered content exceeds the
    /// room the snapshot's other limits leave in the event bound, which is
    /// at least `agent_max_bytes`, so a file over its limit still shows
    /// while it fits; listed when the rules ask before reading it.
    fn agent_entry(
        &self,
        agent: &AgentId,
        limits: &MemoryLimits,
        read: &ReadRules,
        shown: &mut Vec<(MemoryUrl, MemoryRevision)>,
    ) -> Result<MemorySnapshotAgent, MemoryError> {
        let url = MemoryUrl::Agent {
            agent: agent.clone(),
        };
        let entry = match self.read(&url) {
            Ok(read) => Some(read.entry).filter(|entry| entry.document.is_ok()),
            Err(StoreError::NotFound { .. }) => None,
            Err(StoreError::Memory(error)) => return Err(error),
            Err(_) => None,
        };
        let mut agent_entry = MemorySnapshotAgent {
            agent: agent.clone(),
            memory: None,
            omitted: false,
            listed: None,
        };
        let found = match read.of(&url) {
            Visibility::Full => entry,
            Visibility::Summary => {
                agent_entry.listed = entry.as_ref().map(|entry| {
                    let document = entry.document.as_ref().expect("a valid agent file");
                    index_entry(entry, document, limits)
                });
                None
            }
            Visibility::Hidden => None,
        };
        if let Some(found) = found {
            let document = found.document.as_ref().expect("a valid agent file");
            let room = cookie_agent_config::MemoryConfig::MAX_LIMITS_SUM
                .saturating_sub(limits.snapshot_user_bytes + limits.snapshot_index_bytes)
                .max(limits.agent_max_bytes);
            if render::agent_content_bytes(document.description(), document.body()) <= room {
                agent_entry.memory = Some(whole(&found, document, limits));
                shown.push((url, found.revision.clone()));
            } else {
                agent_entry.omitted = true;
            }
        }
        Ok(agent_entry)
    }

    /// One entry per machine, its valid memories the rules let the run see
    /// newest first, as many as fit `index_bytes` of rendered machines
    /// section over all machines; the rest counted.
    fn machine_entries(
        &self,
        machines: &[(MachineName, Vec<AttachmentName>)],
        limits: &MemoryLimits,
        read: &ReadRules,
        now: Timestamp,
    ) -> Result<Vec<MemorySnapshotMachine>, MemoryError> {
        let mut all = Vec::with_capacity(machines.len());
        for (machine, _) in machines {
            let mut entries = self
                .list(&ListScope::Machine(machine.clone()))?
                .entries
                .into_iter()
                .filter(|entry| read.of(&entry.url) != Visibility::Hidden)
                .filter_map(|entry| {
                    let document = entry.document.as_ref().ok()?;
                    Some((entry.modified, index_entry(&entry, document, limits)))
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
                render::machines_section(&shown(count), Some(now)).len()
                    <= limits.snapshot_index_bytes
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

#[cfg(any(test, feature = "test-support"))]
impl MemoryRoot {
    /// The snapshot alone, for tests that do not look at what it showed.
    pub fn snapshot_only(&self, request: &SnapshotRequest) -> Result<MemorySnapshot, MemoryError> {
        self.snapshot(request).map(|taken| taken.snapshot)
    }
}

#[cfg(test)]
mod tests;
