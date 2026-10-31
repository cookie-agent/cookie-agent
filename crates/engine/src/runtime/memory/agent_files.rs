//! The agent memory files the daemon keeps (`docs/site/specs/memory.md`
//! section 16.7): which agents get one, what the store is told about an
//! `agent://` URL, and when the files are brought in line with the agent
//! registries: at start, after every configuration apply or reload, and
//! after every other runtime publication, since one can add or remove the
//! built-in `default` agent.

use std::{collections::BTreeMap, sync::Arc};

use cookie_agent_protocol::{AgentId, AgentMode};

use crate::runtime::Engine;
use crate::{
    memory::{
        MemoryError,
        agents::{AgentMemory, AgentMemorySpec, AgentSync},
    },
    runtime_snapshot::PublishedRuntime,
};

/// The keyword an agent mode gives its memory; internal agents get no file.
const fn mode_keyword(mode: AgentMode) -> Option<&'static str> {
    match mode {
        AgentMode::Primary => Some("primary"),
        AgentMode::Subagent => Some("subagent"),
        AgentMode::All => Some("all"),
        AgentMode::Internal => None,
    }
}

/// Every agent of `runtime`'s registries that gets a memory file, with what
/// the daemon sets in it. An ID in several registries takes the base
/// registry's document, or else the alphabetically first preset's.
pub(crate) fn desired_agent_memory(
    runtime: &PublishedRuntime,
) -> BTreeMap<AgentId, AgentMemorySpec> {
    let mut desired = BTreeMap::new();
    for registry in std::iter::once(&runtime.agents).chain(runtime.agent_presets.values()) {
        for (id, agent) in registry.agents() {
            let frontmatter = &agent.document.frontmatter;
            let Some(mode) = mode_keyword(frontmatter.mode) else {
                continue;
            };
            desired
                .entry(id.clone())
                .or_insert_with(|| AgentMemorySpec::new(id, &frontmatter.description, mode));
        }
    }
    desired
}

impl Engine {
    /// What the memory store is told about `agent`'s file.
    pub(crate) fn agent_memory(&self, agent: &AgentId) -> AgentMemory {
        match desired_agent_memory(&self.current_runtime()).remove(agent) {
            Some(spec) => AgentMemory::Managed(spec),
            None if self.agent_registered(agent) => AgentMemory::Registered,
            None => AgentMemory::Unknown,
        }
    }

    /// Brings the agent files in line with the registries published when
    /// the memory lock is taken. Blocking: it reads and writes files and
    /// may wait for the lock.
    pub(crate) fn sync_agent_memory_now(&self) -> Result<AgentSync, MemoryError> {
        let engine = self.clone();
        self.memory_root().sync_agents(
            move || desired_agent_memory(&engine.current_runtime()),
            self.memory_limits().agent_max_bytes,
        )
    }

    /// [`Self::sync_agent_memory_now`] on a blocking thread; a failure is
    /// reported on standard error, and the next sync tries again.
    pub(crate) async fn sync_agent_memory(&self) {
        let engine = self.clone();
        match tokio::task::spawn_blocking(move || engine.sync_agent_memory_now()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => report(&error),
            Err(error) => eprintln!("cookie-agent: agent memory files: {error}"),
        }
    }

    /// Syncs the agent files once, now, then again after every runtime
    /// publication.
    pub(in crate::runtime) fn install_agent_memory_sync(&self) {
        if let Err(error) = self.sync_agent_memory_now() {
            report(&error);
        }
        let Some(runtime) = self.inner.runtime.clone() else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        let mut changes = self.subscribe_runtime_changes();
        self.spawn_admission_task(&runtime, async move {
            loop {
                match changes.recv().await {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                Engine { inner }.sync_agent_memory().await;
            }
        });
    }
}

fn report(error: &MemoryError) {
    eprintln!("cookie-agent: agent memory files: {error}");
}
