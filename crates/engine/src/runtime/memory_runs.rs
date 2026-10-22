//! Memory as runs meet it (`docs/site/specs/memory.md` sections 6, 7.2, and
//! 9): whether a run has memory and may write it, the session's machines,
//! the snapshot taken at run admission, and what the memory tools read of
//! the calling run.

use std::collections::BTreeSet;

use cookie_agent_protocol::{
    AgentId, AgentMode, AgentSnapshot, AttachmentName, EventPayload as Event, MachineName,
    MemorySnapshot, PermissionAction, PermissionEffect, PermissionRule, ProjectName, RunId,
    SessionId, SessionOrigin, SessionPermissionOverlay, Sha256Digest, StoredEvent,
};
use cookie_agent_runtime_api::GLOBAL_RUNTIME_ID;

use super::{Engine, EngineError, event_origin};
use crate::{
    memory::{
        MemoryRoot,
        search::Visibility,
        snapshot::{ReadRules, SnapshotLimits, SnapshotRequest},
        url::MemoryUrl,
    },
    permissions::{
        MemoryVariables, PermissionScope, effective_permission_with_overlay, patterns_intersect,
        tool_visible,
    },
    policy::FrozenRunPolicy,
};

/// What a run may do with memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MemoryAccess {
    /// The run gets the snapshot and the memory tools.
    pub(crate) available: bool,
    /// The run also gets the write tools.
    pub(crate) writable: bool,
}

/// Whether a run of `agent` has memory (section 6): its agent is not
/// internal, `global_attached`, and the agent or the session overlay holds
/// an `allow` or `ask` rule for `memory` that reaches the `global`
/// runtime. A writable run can also both read and write some memory URL
/// (section 16.9.1): the pattern of one of those `memory` rules and the
/// pattern of an `allow` or `ask` `memory_write` rule overlap. The frozen
/// rules already have their variables expanded.
pub(crate) fn memory_access(
    global_attached: bool,
    agent: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
) -> MemoryAccess {
    let visible = |action: &PermissionAction| {
        tool_visible(&agent.permissions, overlay, action, Some(GLOBAL_RUNTIME_ID))
    };
    let patterns = |action: PermissionAction| {
        agent
            .permissions
            .iter()
            .chain(overlay.into_iter().flat_map(|overlay| &overlay.rules))
            .filter(move |rule| {
                rule.action == action
                    && rule.applies_to(Some(GLOBAL_RUNTIME_ID))
                    && matches!(rule.effect, PermissionEffect::Allow | PermissionEffect::Ask)
            })
            .map(|rule| rule.resource.as_str())
    };
    let available =
        global_attached && agent.mode != AgentMode::Internal && visible(&PermissionAction::MEMORY);
    let overlap = || {
        patterns(PermissionAction::MEMORY).any(|readable| {
            patterns(PermissionAction::MEMORY_WRITE)
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
                rule.action == PermissionAction::MEMORY && rule.applies_to(Some(GLOBAL_RUNTIME_ID))
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
            let changed = compacted
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

/// The latest `memory_loaded` of `events`, and whether a compaction
/// finished after it.
fn latest_snapshot(events: &[std::sync::Arc<StoredEvent>]) -> Option<(&MemorySnapshot, bool)> {
    let mut compacted = false;
    for event in events.iter().rev() {
        match &event.payload {
            Event::MemoryLoaded { snapshot } => return Some((snapshot, compacted)),
            Event::ContextCheckpointCommitted { .. } => compacted = true,
            _ => {}
        }
    }
    None
}

/// What the memory tools read of the run calling them.
#[derive(Clone, Debug)]
pub(crate) struct MemoryCaller {
    pub(crate) agent: AgentSnapshot,
    pub(crate) overlay: SessionPermissionOverlay,
}

impl MemoryCaller {
    /// How much of a search hit at `url` the caller may see: its `memory`
    /// rule for the URL (section 9.3).
    pub(crate) fn visibility(&self, url: &MemoryUrl) -> Visibility {
        match effective_permission_with_overlay(
            &self.agent,
            Some(&self.overlay),
            &PermissionAction::MEMORY,
            &url.to_string(),
            PermissionScope::runtime(GLOBAL_RUNTIME_ID, None),
        )
        .0
        {
            PermissionEffect::Allow => Visibility::Full,
            PermissionEffect::Ask => Visibility::Summary,
            PermissionEffect::Deny => Visibility::Hidden,
        }
    }
}

impl Engine {
    /// The memory folder of the daemon's data root.
    pub(crate) fn memory_root(&self) -> &MemoryRoot {
        self.inner.machines.memory()
    }

    /// The `[memory]` limits as configured now.
    pub(crate) fn memory_limits(&self) -> SnapshotLimits {
        SnapshotLimits::of(&self.inner.config.load().runtime.memory)
    }

    /// Whether the session has the `global` runtime attached.
    pub(crate) fn global_attached(&self, session: SessionId) -> bool {
        self.session_runtimes(session).is_ok_and(|attachments| {
            attachments
                .iter()
                .any(|attachment| attachment.runtime_id.as_str() == GLOBAL_RUNTIME_ID)
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
                .runtime_machine(GLOBAL_RUNTIME_ID)
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
                .session_project(session)
                .map(|project| project.name.as_str().to_owned()),
        }
    }

    /// Appends the run's `memory_loaded` at its admission when section 7.2
    /// asks for one. A snapshot that cannot be read is reported and left
    /// out, keeping the latest one.
    pub(super) async fn refresh_memory_snapshot(
        &self,
        session: SessionId,
        run: RunId,
        policy: &FrozenRunPolicy,
    ) -> Result<(), EngineError> {
        let projection = self.inner.store.get(session)?;
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
            tokio::task::spawn_blocking(move || engine.session_project(session))
                .await
                .ok()
                .flatten()
                .map(|project| project.name)
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
        let events = projection.log.event_snapshot();
        let snapshot = match snapshot_change(latest_snapshot(&events), &facts) {
            SnapshotChange::Keep => return Ok(()),
            SnapshotChange::Clear => MemorySnapshot::unavailable(),
            SnapshotChange::Take => {
                let caller = MemoryCaller {
                    agent: policy.agent.clone(),
                    overlay: projection.permission_overlay.clone(),
                };
                let request = SnapshotRequest {
                    agent: facts.agent,
                    delegated,
                    writable: access.writable,
                    machines,
                    project: facts.project,
                    limits: self.memory_limits(),
                    read: ReadRules::new(move |url| caller.visibility(url)),
                    rules: Some(facts.rules),
                };
                let root = self.memory_root().clone();
                match tokio::task::spawn_blocking(move || root.snapshot(&request)).await {
                    Ok(Ok(snapshot)) => snapshot,
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

    /// The run `run` as the memory tools see it.
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
        Ok(MemoryCaller {
            agent: active.policy.agent.clone(),
            overlay: projection.permission_overlay.clone(),
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
