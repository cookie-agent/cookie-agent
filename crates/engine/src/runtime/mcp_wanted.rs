//! MCP servers on demand (`docs/site/specs/session-tool-groups.md`
//! section 10): a server is wanted while a loaded session that reaches it
//! leaves its group on or deferred. The daemon's own servers and those of
//! every runtime that takes them on demand connect when a session wants
//! them, and stop once none has for `[mcp] idle_stop_after`.

use std::{
    collections::{BTreeSet, HashMap},
    sync::Mutex,
    time::{Duration, Instant},
};

use cookie_agent_protocol::{RuntimeId, ToolGroupKind};

use super::Engine;
use crate::exposure::{GroupKey, Layers};

/// What hosts a set of MCP servers.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Host {
    /// The daemon's own, top-level servers.
    Global,
    /// A runtime's runtime-scoped servers, on one connection.
    Runtime { runtime: String, epoch: u64 },
    /// One instance's workspace-scoped servers, on one connection.
    Instance {
        runtime: String,
        instance: String,
        epoch: u64,
    },
}

#[derive(Default)]
struct HostState {
    /// When each server was last wanted.
    wanted_at: HashMap<String, Instant>,
    /// What the host was last told to keep.
    told: Option<BTreeSet<String>>,
}

/// When each server was last wanted, by host, and what each was told.
#[derive(Default)]
pub(crate) struct McpWanted {
    hosts: Mutex<HashMap<Host, HostState>>,
}

impl McpWanted {
    /// What `host` keeps now: the servers `wanted` names and those wanted
    /// within `idle` (every one ever wanted, with `None`); with whether it
    /// differs from what the host was last told.
    fn keep(
        &self,
        host: &Host,
        wanted: &BTreeSet<String>,
        idle: Option<Duration>,
        now: Instant,
    ) -> (BTreeSet<String>, bool) {
        let mut hosts = lock(&self.hosts);
        let state = hosts.entry(host.clone()).or_default();
        for server in wanted {
            state.wanted_at.insert(server.clone(), now);
        }
        if let Some(idle) = idle {
            state
                .wanted_at
                .retain(|_, at| now.saturating_duration_since(*at) < idle);
        }
        let keep = state.wanted_at.keys().cloned().collect::<BTreeSet<_>>();
        let changed = state.told.as_ref() != Some(&keep);
        state.told = Some(keep.clone());
        (keep, changed)
    }

    /// A daemon server started by hand counts as wanted now: it stops again
    /// once nobody wanted it for `[mcp] idle_stop_after`.
    pub(crate) fn touch_global(&self, server: &str) {
        lock(&self.hosts)
            .entry(Host::Global)
            .or_default()
            .wanted_at
            .insert(server.to_owned(), Instant::now());
    }

    /// Forgets the hosts `live` leaves out: connections that ended.
    fn retain(&self, live: &HashMap<Host, BTreeSet<String>>) {
        lock(&self.hosts).retain(|host, _| *host == Host::Global || live.contains_key(host));
    }
}

impl Engine {
    /// Works out which MCP servers the loaded sessions want, connects them,
    /// and stops those no session wanted for `[mcp] idle_stop_after`: when
    /// a session opens, a run starts, a session's choices change, and every
    /// janitor pass.
    pub(crate) fn refresh_mcp_wanted(&self) {
        self.refresh_mcp_wanted_at(Instant::now());
    }

    /// Refreshes the wanted servers whenever an MCP server's configuration
    /// or status changes anywhere, a quarter second after the change: a
    /// runtime's new server, or one that dropped.
    pub(crate) fn start_mcp_wanted_watch(&self) {
        let Some(runtime) = self.inner.runtime.clone() else {
            return;
        };
        let mut changes = self.inner.mcp_hub.subscribe_changes();
        let inner = std::sync::Arc::downgrade(&self.inner);
        runtime.spawn(async move {
            while changes.changed().await.is_ok() {
                tokio::time::sleep(Duration::from_millis(250)).await;
                changes.borrow_and_update();
                let Some(inner) = inner.upgrade() else {
                    break;
                };
                Engine { inner }.refresh_mcp_wanted();
            }
        });
    }

    pub(crate) fn refresh_mcp_wanted_at(&self, now: Instant) {
        let Some(handle) = self.inner.runtime.clone() else {
            return;
        };
        let idle = self.inner.config.load().runtime.mcp_idle_stop_after.0;
        let global_servers = self
            .inner
            .mcp
            .statuses()
            .into_iter()
            .map(|status| status.server)
            .collect::<Vec<_>>();
        let mut wanted = HashMap::<Host, BTreeSet<String>>::from([(Host::Global, BTreeSet::new())]);
        let mut connections = HashMap::<Host, (crate::runtimes::InstanceConnection, bool)>::new();
        // Every host that takes servers on demand, wanted or not: one that no
        // session reaches any more stops its servers too.
        for instance in self.inner.runtimes.published_instances() {
            let Some(connection) = instance.connection() else {
                continue;
            };
            let Some(instance_id) = connection.instance.clone() else {
                continue;
            };
            // A runtime without servers has nothing to be told.
            if connection
                .mcp_candidates()
                .is_none_or(|(own, shared)| own.is_empty() && shared.is_empty())
            {
                continue;
            }
            let runtime = instance.info().id().to_owned();
            let epoch = connection.epoch();
            let own = Host::Instance {
                runtime: runtime.clone(),
                instance: instance_id,
                epoch,
            };
            let shared = Host::Runtime { runtime, epoch };
            wanted.entry(own.clone()).or_default();
            wanted.entry(shared.clone()).or_default();
            connections.insert(own, (connection.clone(), false));
            connections.entry(shared).or_insert((connection, true));
        }
        let current = self.current_runtime();
        for projection in self.inner.store.all() {
            let agent = crate::permissions::governing_agent(&projection);
            let acp = current
                .agents
                .get(&agent.agent)
                .is_some_and(|resolved| resolved.acp.is_some());
            let layers = Layers::new(&agent.exposure, &projection.tool_exposure, acp);
            let offers = |name: &str, runtime: Option<&RuntimeId>| {
                layers.offers(&GroupKey {
                    kind: ToolGroupKind::Mcp,
                    name: name.to_owned(),
                    runtime: runtime.cloned(),
                })
            };
            for attachment in self.attachments_of(&projection) {
                if attachment.runtime_id.as_str() == RuntimeId::GLOBAL {
                    let set = wanted.entry(Host::Global).or_default();
                    set.extend(
                        global_servers
                            .iter()
                            .filter(|server| offers(server, None))
                            .cloned(),
                    );
                    continue;
                }
                let Some(connection) = self
                    .inner
                    .runtimes
                    .instance(&attachment)
                    .and_then(|instance| instance.connection())
                else {
                    continue;
                };
                let (Some((own, shared)), Some(instance_id)) =
                    (connection.mcp_candidates(), connection.instance.clone())
                else {
                    continue;
                };
                if own.is_empty() && shared.is_empty() {
                    continue;
                }
                let runtime = attachment.runtime_id.as_str().to_owned();
                let epoch = connection.epoch();
                let reached = Some(&attachment.runtime_id);
                wanted
                    .entry(Host::Instance {
                        runtime: runtime.clone(),
                        instance: instance_id,
                        epoch,
                    })
                    .or_default()
                    .extend(own.into_iter().filter(|server| offers(server, reached)));
                wanted
                    .entry(Host::Runtime { runtime, epoch })
                    .or_default()
                    .extend(shared.into_iter().filter(|server| offers(server, reached)));
            }
        }
        self.inner.mcp_wanted.retain(&wanted);
        for (host, servers) in &wanted {
            let (keep, changed) = self.inner.mcp_wanted.keep(host, servers, idle, now);
            match host {
                // Cheap when nothing changes; also catches a server that
                // dropped and was not restarted.
                Host::Global => self.inner.mcp.set_wanted(&keep, &handle),
                Host::Runtime { .. } | Host::Instance { .. } if changed => {
                    if let Some((connection, runtime_scope)) = connections.get(host) {
                        connection.want_mcp(*runtime_scope, keep.into_iter().collect());
                    }
                }
                Host::Runtime { .. } | Host::Instance { .. } => {}
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_server_is_kept_for_the_idle_delay_after_it_was_last_wanted() {
        let wanted = McpWanted::default();
        let start = Instant::now();
        let idle = Some(Duration::from_secs(600));
        let (keep, changed) = wanted.keep(&Host::Global, &set(&["exa"]), idle, start);
        assert_eq!((keep, changed), (set(&["exa"]), true));
        let (keep, changed) = wanted.keep(
            &Host::Global,
            &set(&[]),
            idle,
            start + Duration::from_secs(599),
        );
        assert_eq!((keep, changed), (set(&["exa"]), false));
        let (keep, changed) = wanted.keep(
            &Host::Global,
            &set(&[]),
            idle,
            start + Duration::from_secs(600),
        );
        assert_eq!((keep, changed), (set(&[]), true));
        // Without a delay a server once wanted stays.
        let (keep, _) = wanted.keep(&Host::Global, &set(&["docs"]), None, start);
        assert_eq!(keep, set(&["docs"]));
        let (keep, _) = wanted.keep(
            &Host::Global,
            &set(&[]),
            None,
            start + Duration::from_secs(86_400),
        );
        assert_eq!(keep, set(&["docs"]));
    }
}
