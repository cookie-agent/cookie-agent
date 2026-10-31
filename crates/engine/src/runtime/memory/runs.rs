//! Memory as runs meet it (`docs/site/specs/memory.md` sections 6, 7.2, and
//! 9): whether a run has memory and may write it, the session's machines,
//! the snapshot taken at run admission, and what the memory tools read of
//! the calling run.

use std::{
    collections::{BTreeSet, HashMap},
    sync::Mutex,
};

use cookie_agent_protocol::{
    AgentId, AgentMode, AgentSnapshot, AttachmentName, EventPayload as Event, MachineName,
    MemorySnapshot, PermissionAction, PermissionEffect, PermissionRule, ProjectName, RunId,
    RuntimeId, SessionId, SessionOrigin, SessionPermissionOverlay, Sha256Digest, StoredEvent,
};

use crate::runtime::{Engine, EngineError, event_origin};
use crate::{
    memory::{
        MemoryRoot,
        file::MemoryRevision,
        limits::MemoryLimits,
        search::Visibility,
        snapshot::{ReadRules, SnapshotRequest},
        url::MemoryUrl,
    },
    permissions::{
        MemoryVariables, MergedRules, PermissionScope, effective_permission_with_overlay,
        patterns_intersect,
    },
    policy::FrozenRunPolicy,
};

/// The revision each session last saw of each memory (section 16.9.8):
/// through `memory_read`, the snapshot, or a write of its own. Out of the
/// model's sight, and kept in memory only: after a restart a session reads
/// a memory again before it replaces it whole.
#[derive(Debug, Default)]
pub(crate) struct SeenMemories(Mutex<HashMap<SessionId, HashMap<MemoryUrl, MemoryRevision>>>);

impl SeenMemories {
    fn with<T>(
        &self,
        work: impl FnOnce(&mut HashMap<SessionId, HashMap<MemoryUrl, MemoryRevision>>) -> T,
    ) -> T {
        work(
            &mut self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Records that `session` saw `url` at `revision`.
    pub(crate) fn saw(&self, session: SessionId, url: MemoryUrl, revision: MemoryRevision) {
        self.with(|seen| seen.entry(session).or_default().insert(url, revision));
    }

    /// The revision `session` last saw of `url`.
    pub(crate) fn seen(&self, session: SessionId, url: &MemoryUrl) -> Option<MemoryRevision> {
        self.with(|seen| seen.get(&session)?.get(url).cloned())
    }

    /// Forgets what `sessions` saw, as they are deleted or paged out: a
    /// session reloaded later reads a memory again before it replaces it
    /// whole, as after a restart.
    pub(crate) fn forget_sessions(&self, sessions: &std::collections::HashSet<SessionId>) {
        self.with(|seen| seen.retain(|session, _| !sessions.contains(session)));
    }

    /// Forgets what `session` saw of `url`, which it deleted.
    pub(crate) fn forget(&self, session: SessionId, url: &MemoryUrl) {
        self.with(|seen| {
            if let Some(urls) = seen.get_mut(&session) {
                urls.remove(url);
            }
        });
    }
}

/// What a run may do with memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MemoryAccess {
    /// The run gets the snapshot and the memory tools.
    pub(crate) available: bool,
    /// The run also gets the write tools.
    pub(crate) writable: bool,
}

/// Whether a run of `agent` has memory (section 6): its agent is not
/// internal, `global_attached`, and the merged list of its rules and the
/// session overlay ([`MergedRules`]) offers `memory` on the `global`
/// runtime. A writable run can also both read and write some memory URL
/// (section 16.9.1): the pattern of one of its `allow` or `ask` `memory`
/// rules and that of an `allow` or `ask` `memory_write` rule overlap, an
/// action without a `*` rule counting as `*`, since what no rule matches
/// asks. The frozen rules already
/// have their variables expanded.
pub(crate) fn memory_access(
    global_attached: bool,
    agent: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
) -> MemoryAccess {
    let merged = MergedRules::new(&agent.permissions, overlay, None);
    let visible = |action: &PermissionAction| merged.offers(action, Some(RuntimeId::GLOBAL));
    // What no rule matches asks: without a `*` rule, every URL the rules
    // leave out is reachable.
    let patterns = |action: PermissionAction| {
        let applying = merged
            .rules()
            .iter()
            .map(|merged| merged.rule)
            .filter(|rule| rule.action == action && rule.applies_to(Some(RuntimeId::GLOBAL)))
            .collect::<Vec<_>>();
        let unmatched_asks = !applying.iter().any(|rule| rule.resource.as_str() == "*");
        applying
            .iter()
            .filter(|rule| matches!(rule.effect, PermissionEffect::Allow | PermissionEffect::Ask))
            .map(|rule| rule.resource.as_str())
            .chain(unmatched_asks.then_some("*"))
            .collect::<Vec<_>>()
    };
    let available =
        global_attached && agent.mode != AgentMode::Internal && visible(&PermissionAction::MEMORY);
    let overlap = || {
        let writable = patterns(PermissionAction::MEMORY_WRITE);
        patterns(PermissionAction::MEMORY).iter().any(|readable| {
            writable
                .iter()
                .any(|writable| patterns_intersect(readable, writable))
        })
    };
    MemoryAccess {
        available,
        writable: available && overlap(),
    }
}

/// What a run admission knows when it decides on a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotFacts {
    pub(crate) access: MemoryAccess,
    pub(crate) agent: AgentId,
    pub(crate) delegated: bool,
    /// The session's machines; compared only for root runs, whose snapshot
    /// lists them.
    pub(crate) machines: BTreeSet<MachineName>,
    /// The name of the session's project; compared only for root runs,
    /// whose snapshot shows it (`docs/site/specs/projects.md` section 7).
    pub(crate) project: Option<ProjectName>,
    /// The fingerprint of the run's `memory` rules, which filter what the
    /// snapshot shows (section 7.5).
    pub(crate) rules: Sha256Digest,
}

/// The fingerprint of the `memory` rules that reach `global` for a run of
/// `agent` with `overlay`: the agent's, then the overlay's, in order, with
/// the agent's ID.
pub(crate) fn memory_rules_fingerprint(
    agent: &AgentSnapshot,
    overlay: &SessionPermissionOverlay,
) -> Sha256Digest {
    let memory = |rules: &[PermissionRule]| {
        rules
            .iter()
            .filter(|rule| {
                rule.action == PermissionAction::MEMORY && rule.applies_to(Some(RuntimeId::GLOBAL))
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    let bytes = serde_json::to_vec(&(
        agent.agent.as_str(),
        memory(&agent.permissions),
        memory(&overlay.rules),
    ))
    .expect("permission rules serialize");
    Sha256Digest::of_bytes(&bytes)
}

/// What a run admission appends (section 7.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotChange {
    /// The latest snapshot still holds.
    Keep,
    /// Take a new snapshot.
    Take,
    /// Memory is no longer available: append an empty snapshot.
    Clear,
}

/// The change section 7.2 asks for, given the latest snapshot of the log
/// (and whether a compaction finished after it) and the run's facts.
pub(crate) fn snapshot_change(
    latest: Option<(&MemorySnapshot, bool)>,
    facts: &SnapshotFacts,
) -> SnapshotChange {
    let available = facts.access.available;
    let Some((latest, compacted)) = latest else {
        return if available {
            SnapshotChange::Take
        } else {
            SnapshotChange::Keep
        };
    };
    match (&latest.agent, available) {
        (None, false) => SnapshotChange::Keep,
        (Some(_), false) => SnapshotChange::Clear,
        (None, true) => SnapshotChange::Take,
        (Some(agent), true) => {
            let machines = latest
                .machines
                .iter()
                .map(|machine| machine.machine.clone())
                .collect::<BTreeSet<_>>();
            let project = latest.project.as_ref().map(|project| &project.project);
            // A snapshot an earlier daemon took has no time, so its ages
            // and fullness could not be shown.
            let changed = compacted
                || latest.taken_at.is_none()
                || latest.rules.as_ref() != Some(&facts.rules)
                || agent.agent != facts.agent
                || latest.writable != facts.access.writable
                || latest.delegated != facts.delegated
                || (!facts.delegated && machines != facts.machines)
                || (!facts.delegated && project != facts.project.as_ref());
            if changed {
                SnapshotChange::Take
            } else {
                SnapshotChange::Keep
            }
        }
    }
}

/// The snapshot of the `memory_loaded` record `latest`, and whether a
/// compaction finished after it.
fn latest_snapshot(
    latest: Option<&(std::sync::Arc<StoredEvent>, bool)>,
) -> Option<(&MemorySnapshot, bool)> {
    let (event, compacted) = latest?;
    match &event.payload {
        Event::MemoryLoaded { snapshot } => Some((snapshot, *compacted)),
        _ => None,
    }
}

/// The memory related to a session (section 16.9.11): what its snapshot
/// includes automatically.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RelatedMemory {
    /// A delegated run's related memory is its own agent file alone.
    pub(crate) delegated: bool,
    /// The session's machines, and the name of its project.
    pub(crate) machines: Vec<MachineName>,
    pub(crate) project: Option<String>,
}

impl RelatedMemory {
    /// Whether the memory at `url` is related to a run of `agent`: for a
    /// root run every `user://` memory, its own agent file, its machines'
    /// memories, and its project's; for a delegated run its own agent file.
    pub(crate) fn contains(&self, url: &MemoryUrl, agent: &AgentId) -> bool {
        match url {
            MemoryUrl::Agent { agent: owner } => owner == agent,
            _ if self.delegated => false,
            MemoryUrl::User { .. } => true,
            MemoryUrl::Machine { machine, .. } => self.machines.contains(machine),
            MemoryUrl::Project { project, .. } => self.project.as_deref() == Some(project.as_str()),
        }
    }
}

/// What the memory tools read of the run calling them.
#[derive(Clone, Debug)]
pub(crate) struct MemoryCaller {
    pub(crate) agent: AgentSnapshot,
    pub(crate) overlay: SessionPermissionOverlay,
    pub(crate) related: RelatedMemory,
}

impl MemoryCaller {
    fn effect(&self, action: &PermissionAction, url: &MemoryUrl) -> PermissionEffect {
        effective_permission_with_overlay(
            &self.agent,
            Some(&self.overlay),
            action,
            &url.to_string(),
            PermissionScope::runtime(RuntimeId::GLOBAL, None),
        )
        .0
    }

    /// How much of a search hit at `url` the caller may see: its `memory`
    /// rule for the URL (section 9.3).
    pub(crate) fn visibility(&self, url: &MemoryUrl) -> Visibility {
        match self.effect(&PermissionAction::MEMORY, url) {
            PermissionEffect::Allow => Visibility::Full,
            PermissionEffect::Ask => Visibility::Summary,
            PermissionEffect::Deny => Visibility::Hidden,
        }
    }

    /// Whether the caller's `memory_write` rules let it change the memory
    /// at `url`, with or without asking (section 16.9.4).
    pub(crate) fn editable(&self, url: &MemoryUrl) -> bool {
        self.effect(&PermissionAction::MEMORY_WRITE, url) != PermissionEffect::Deny
    }

    /// Whether the memory at `url` is related to the caller's session.
    pub(crate) fn related(&self, url: &MemoryUrl) -> bool {
        self.related.contains(url, &self.agent.agent)
    }
}

impl Engine {
    /// The memory folder of the daemon's data root.
    pub(crate) fn memory_root(&self) -> &MemoryRoot {
        self.inner.machines.memory()
    }

    /// The `[memory]` limits as configured now.
    pub(crate) fn memory_limits(&self) -> MemoryLimits {
        MemoryLimits::of(&self.inner.config.load().runtime.memory)
    }

    /// Whether the session has the `global` runtime attached.
    pub(crate) fn global_attached(&self, session: SessionId) -> bool {
        self.session_runtimes(session).is_ok_and(|attachments| {
            attachments
                .iter()
                .any(|attachment| attachment.runtime_id.as_str() == RuntimeId::GLOBAL)
        })
    }

    /// The session's machines (section 6): the machines of its attachments
    /// besides `global` in the order they first name them, each with the
    /// attachments on it; the daemon's machine for a session with only
    /// `global`. An attachment whose machine the daemon cannot name adds
    /// nothing.
    pub(crate) fn session_machines(
        &self,
        session: SessionId,
    ) -> Vec<(MachineName, Vec<AttachmentName>)> {
        let facts = self.attachment_facts(session);
        if facts.is_empty() {
            return self
                .runtime_machine(RuntimeId::GLOBAL)
                .map(|machine| vec![(machine, Vec::new())])
                .unwrap_or_default();
        }
        let mut machines: Vec<(MachineName, Vec<AttachmentName>)> = Vec::new();
        for attachment in facts {
            let Some(machine) = attachment.machine else {
                continue;
            };
            match machines.iter_mut().find(|(known, _)| *known == machine) {
                Some((_, attachments)) => attachments.push(attachment.name),
                None => machines.push((machine, vec![attachment.name])),
            }
        }
        machines
    }

    /// What the memory rule variables stand for in `session`'s runs
    /// (`docs/site/specs/memory.md` section 16.9.9): its machines and its
    /// project's name; a delegated session's project is its root's.
    pub(crate) fn memory_variables(&self, session: SessionId) -> MemoryVariables {
        MemoryVariables {
            machines: self
                .session_machines(session)
                .into_iter()
                .map(|(machine, _)| machine.as_str().to_owned())
                .collect(),
            project: self
                .session_project_name(session)
                .map(|project| project.as_str().to_owned()),
        }
    }

    /// Appends the run's `memory_loaded` at its admission when section 7.2
    /// asks for one. A snapshot that cannot be read is reported and left
    /// out, keeping the latest one.
    pub(in crate::runtime) async fn refresh_memory_snapshot(
        &self,
        session: SessionId,
        run: RunId,
        policy: &FrozenRunPolicy,
    ) -> Result<(), EngineError> {
        let projection = self.inner.store.get(session)?;
        // An ACP agent's own subagent gets what its agent gives it, never a
        // snapshot of cookie's: none is recorded as though it had.
        if projection.log.is_acp_subagent() {
            return Ok(());
        }
        let delegated = matches!(projection.meta.origin, SessionOrigin::Delegated { .. });
        let access = memory_access(
            self.global_attached(session),
            &policy.agent,
            Some(&projection.permission_overlay),
        );
        let machines = if access.available && !delegated {
            self.session_machines(session)
        } else {
            Vec::new()
        };
        let project = if access.available && !delegated {
            let engine = self.clone();
            tokio::task::spawn_blocking(move || engine.session_project_name(session))
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        let facts = SnapshotFacts {
            access,
            agent: policy.agent.agent.clone(),
            delegated,
            machines: machines
                .iter()
                .map(|(machine, _)| machine.clone())
                .collect(),
            project,
            rules: memory_rules_fingerprint(&policy.agent, &projection.permission_overlay),
        };
        let latest = projection.log.last_memory_loaded();
        let snapshot = match snapshot_change(latest_snapshot(latest.as_ref()), &facts) {
            SnapshotChange::Keep => return Ok(()),
            SnapshotChange::Clear => MemorySnapshot::unavailable(),
            SnapshotChange::Take => {
                let caller = MemoryCaller {
                    agent: policy.agent.clone(),
                    overlay: projection.permission_overlay.clone(),
                    related: RelatedMemory::default(),
                };
                let request = SnapshotRequest {
                    agent: facts.agent,
                    delegated,
                    writable: access.writable,
                    machines,
                    project: facts.project,
                    limits: self.memory_limits(),
                    now: jiff::Timestamp::now(),
                    read: ReadRules::new(move |url| caller.visibility(url)),
                    rules: Some(facts.rules),
                };
                let root = self.memory_root().clone();
                let now = request.now;
                match tokio::task::spawn_blocking(move || {
                    let taken = root.snapshot(&request)?;
                    root.record_reads(taken.shown.iter().map(|(url, _)| url), now);
                    Ok::<_, crate::memory::MemoryError>(taken)
                })
                .await
                {
                    Ok(Ok(taken)) => {
                        for (url, revision) in taken.shown {
                            self.inner.memory_seen.saw(session, url, revision);
                        }
                        taken.snapshot
                    }
                    Ok(Err(error)) => {
                        eprintln!("cookie-agent: memory snapshot of session {session}: {error}");
                        return Ok(());
                    }
                    Err(error) => {
                        eprintln!("cookie-agent: memory snapshot of session {session}: {error}");
                        return Ok(());
                    }
                }
            }
        };
        self.append(
            session,
            Some(run),
            event_origin("engine:memory"),
            Event::MemoryLoaded { snapshot },
        )
        .await
    }

    /// The run `run` as the memory tools see it. Blocking: it reads the
    /// session's project.
    pub(crate) fn memory_caller(&self, run: RunId) -> Result<MemoryCaller, String> {
        let active = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run)
            .cloned()
            .ok_or_else(|| "the calling run is not active".to_owned())?;
        let projection = self
            .inner
            .store
            .get(active.session)
            .map_err(|error| error.to_string())?;
        let variables = self.memory_variables(active.session);
        Ok(MemoryCaller {
            agent: active.policy.agent.clone(),
            overlay: projection.permission_overlay.clone(),
            related: RelatedMemory {
                delegated: matches!(projection.meta.origin, SessionOrigin::Delegated { .. }),
                machines: variables
                    .machines
                    .iter()
                    .filter_map(|machine| MachineName::new(machine.as_str()).ok())
                    .collect(),
                project: variables.project,
            },
        })
    }

    /// Whether `agent` names an agent of the registry, so its memory file
    /// may be created.
    pub(crate) fn agent_registered(&self, agent: &AgentId) -> bool {
        let runtime = self.current_runtime();
        runtime.agents.get(agent).is_some()
            || runtime
                .agent_presets
                .values()
                .any(|registry| registry.get(agent).is_some())
    }
}

#[cfg(test)]
mod tests;
