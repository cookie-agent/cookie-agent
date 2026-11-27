//! Temporary runtimes from the engine's side
//! (`docs/site/specs/temporary-runtimes.md` section 4): stopping a process
//! once it has been idle for its template's `idle_stop_after`.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use cookie_agent_protocol::SessionId;

use super::Engine;

/// How often idle processes are looked for, besides whenever a run or a
/// background call changes.
const IDLE_CHECK: Duration = Duration::from_secs(10);

/// A process of a template: its runtime ID and owning root session.
type Process = (String, Option<SessionId>);

impl Engine {
    /// Watches the processes of templates that stop when idle, and stops
    /// each that nothing has used for its `idle_stop_after`.
    pub(crate) fn start_idle_runtime_watch(&self) {
        let Some(runtime) = self.inner.runtime.clone() else {
            return;
        };
        let inner = std::sync::Arc::downgrade(&self.inner);
        let mut activity = self.subscribe_runtime_activity();
        runtime.spawn(async move {
            let mut idle_since = HashMap::<Process, Instant>::new();
            loop {
                tokio::select! {
                    () = tokio::time::sleep(IDLE_CHECK) => {}
                    changed = activity.changed() => if changed.is_err() { return },
                }
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let engine = Engine { inner };
                for process in engine.idle_processes(&mut idle_since, Instant::now()) {
                    let connections = engine.inner.runtimes.stop_owner(&process.0, process.1);
                    futures_util::future::join_all(
                        connections
                            .iter()
                            .map(crate::runtimes::RuntimeClient::shutdown),
                    )
                    .await;
                }
            }
        });
    }

    /// The processes idle for their template's `idle_stop_after` at `now`,
    /// updating when each became idle in `idle_since`.
    fn idle_processes(
        &self,
        idle_since: &mut HashMap<Process, Instant>,
        now: Instant,
    ) -> Vec<Process> {
        let candidates = self.inner.runtimes.idle_candidates();
        if candidates.is_empty() {
            idle_since.clear();
            return Vec::new();
        }
        let busy = self.busy_processes();
        let mut idle = Vec::new();
        let mut seen = HashSet::new();
        for (runtime, owner, after) in candidates {
            let process = (runtime, owner);
            seen.insert(process.clone());
            if busy.contains(&process) {
                idle_since.remove(&process);
                continue;
            }
            let since = *idle_since.entry(process.clone()).or_insert(now);
            if now.duration_since(since) >= after {
                idle_since.remove(&process);
                idle.push(process);
            }
        }
        idle_since.retain(|process, _| seen.contains(process));
        idle
    }

    /// The processes something uses now, as a client's disconnect counts
    /// them ([`Engine::runtime_activity`]): an active run of a session
    /// attached to the runtime, or a background call executing on it, each
    /// on its tree's process of a per-session runtime.
    pub(super) fn busy_processes(&self) -> HashSet<Process> {
        let process = |runtime: &str, session: SessionId| {
            let owner = self
                .inner
                .runtimes
                .is_per_session(runtime)
                .then(|| self.root_session_of(session));
            (runtime.to_owned(), owner)
        };
        let sessions = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|run| run.session)
            .collect::<Vec<_>>();
        let mut busy = HashSet::new();
        for session in sessions {
            for attachment in self.session_runtimes(session).unwrap_or_default() {
                busy.insert(process(attachment.runtime_id.as_str(), session));
            }
        }
        for (session, runtime) in self
            .inner
            .background
            .pending_calls()
            .into_iter()
            .chain(self.acp_background_runtime_calls())
        {
            busy.insert(process(&runtime, session));
        }
        busy
    }
}
