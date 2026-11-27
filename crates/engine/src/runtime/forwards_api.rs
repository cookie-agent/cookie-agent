//! The engine's forwards (`docs/site/specs/forwards.md`): the client
//! methods, the tools' calls, and the supervisor and relay they start.

use std::{collections::HashSet, sync::Arc};

use cookie_agent_protocol::{
    AgentId, Forward, ForwardCreateParams, ForwardDeleteParams, ForwardDeleteResult, ForwardKind,
    ForwardResult, ForwardStamp, ForwardUpdateParams, ForwardsListParams, ForwardsListResult,
    SessionId,
};

use super::{Engine, EngineError};
use crate::forwards::{ForwardError, ForwardRecord, MAX_FORWARDS};
use crate::runtimes::RuntimeClient;

/// Who makes a forward: the user, or an agent's call in a session.
#[derive(Clone, Debug)]
pub(crate) enum ForwardMaker {
    User,
    Agent { session: SessionId, agent: AgentId },
}

impl Engine {
    /// Installs the forwards' relay of runtimes' `net/*` notifications and
    /// starts their supervisor.
    pub(crate) fn start_forwards(&self) {
        let forwards = Arc::clone(&self.inner.forwards);
        self.inner
            .mcp_hub
            .set_net_handler(Arc::new(crate::forwards::NetRelay {
                forwards: Arc::clone(&forwards),
                engine: Arc::downgrade(&self.inner),
            }));
        // An engine opened outside a runtime, as some tests open one, runs
        // no supervisor.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(forwards.supervise(Arc::downgrade(&self.inner)));
        }
    }

    /// A connected runtime of `machine` that takes part in forwards.
    pub(crate) async fn forward_client(&self, machine: &str) -> Option<RuntimeClient> {
        let runtimes = self
            .machines()
            .into_iter()
            .find(|info| info.name.as_str() == machine)?
            .connected;
        for runtime in runtimes {
            let Ok(link) = self
                .inner
                .runtimes
                .agent_connection(runtime.runtime_id.as_str(), None)
                .await
            else {
                continue;
            };
            let client = link.0;
            if client.net() && !client.is_closed() {
                return Some(client);
            }
        }
        None
    }

    /// Bumped whenever a forward is made, changed, deleted, or its status
    /// or counts move.
    pub fn subscribe_forward_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.forwards.subscribe()
    }

    /// `forwards.list`.
    pub fn list_forwards(
        &self,
        params: ForwardsListParams,
    ) -> Result<ForwardsListResult, EngineError> {
        let owner = params.session.map(|session| self.root_session_of(session));
        Ok(ForwardsListResult {
            forwards: self.inner.forwards.list(owner),
        })
    }

    /// The forwards a session's tree can use: its own and the user's.
    pub(crate) fn session_forwards(&self, session: SessionId) -> Vec<Forward> {
        let root = self.root_session_of(session);
        self.inner
            .forwards
            .list(None)
            .into_iter()
            .filter(|forward| forward.owner.is_none_or(|owner| owner == root))
            .collect()
    }

    /// `forward.create`: the user's forward, which belongs to no session.
    pub fn create_forward(
        &self,
        params: ForwardCreateParams,
    ) -> Result<ForwardResult, EngineError> {
        Ok(ForwardResult {
            forward: self.create_forward_as(params, &ForwardMaker::User)?,
        })
    }

    /// Makes a forward as `maker`: an agent's belongs to its session's
    /// tree.
    pub(crate) fn create_forward_as(
        &self,
        params: ForwardCreateParams,
        maker: &ForwardMaker,
    ) -> Result<Forward, ForwardError> {
        let invalid = |reason: String| ForwardError::Invalid(reason);
        match (params.kind, &params.from) {
            (ForwardKind::Http, Some(_)) => {
                return Err(invalid(
                    "an http forward is served at the daemon's address; it takes no `from`".into(),
                ));
            }
            (ForwardKind::Tcp | ForwardKind::Udp, None) => {
                return Err(invalid(format!(
                    "a {} forward listens somewhere: name `from`",
                    params.kind.as_str()
                )));
            }
            _ => {}
        }
        if params.to.port == 0 {
            return Err(invalid("the target's port is 0".into()));
        }
        if let Some(title) = &params.title
            && title.chars().count() > cookie_agent_protocol::FORWARD_TITLE_MAX_CHARS
        {
            return Err(invalid(format!(
                "a title has at most {} characters",
                cookie_agent_protocol::FORWARD_TITLE_MAX_CHARS
            )));
        }
        let known = self
            .machines()
            .into_iter()
            .map(|info| info.name.as_str().to_owned())
            .collect::<HashSet<_>>();
        for machine in std::iter::once(&params.to.machine)
            .chain(params.from.as_ref().map(|from| &from.machine))
        {
            if !known.contains(machine) {
                let mut names = known.iter().cloned().collect::<Vec<_>>();
                names.sort();
                return Err(invalid(format!(
                    "there is no machine {machine:?}; the machines are {}",
                    names.join(", ")
                )));
            }
        }
        let now = jiff::Timestamp::now();
        let expires_at = expiry(params.expires_in_hours, now)?;
        let (owner, created) = match maker {
            ForwardMaker::User => (
                None,
                ForwardStamp {
                    at: now,
                    session: None,
                    agent: None,
                },
            ),
            ForwardMaker::Agent { session, agent } => (
                Some(self.root_session_of(*session)),
                ForwardStamp {
                    at: now,
                    session: Some(*session),
                    agent: Some(agent.clone()),
                },
            ),
        };
        let handle = self.inner.forwards.mutate(|records| {
            if records.len() >= MAX_FORWARDS {
                return Err(format!(
                    "the daemon keeps at most {MAX_FORWARDS} forwards; delete one first"
                ));
            }
            let taken = records
                .iter()
                .map(|record| record.handle.clone())
                .collect::<HashSet<_>>();
            let handle = crate::forwards::new_handle(&taken);
            records.push(ForwardRecord {
                id: uuid::Uuid::now_v7().to_string(),
                handle: handle.clone(),
                kind: params.kind,
                to: params.to,
                from: params.from,
                title: params.title.filter(|title| !title.trim().is_empty()),
                owner,
                created,
                expires_at,
            });
            Ok(handle)
        });
        let handle = handle.map_err(invalid)?;
        self.inner
            .forwards
            .get(&handle)
            .ok_or(ForwardError::NotFound(handle))
    }

    /// `forward.update`.
    pub fn update_forward(
        &self,
        params: ForwardUpdateParams,
    ) -> Result<ForwardResult, EngineError> {
        let expires_at = params
            .expires_in_hours
            .map(|hours| expiry(Some(hours), jiff::Timestamp::now()))
            .transpose()?;
        let handle = params.forward.clone();
        self.inner
            .forwards
            .mutate(|records| {
                let Some(record) = records.iter_mut().find(|record| record.handle == handle) else {
                    return Err(String::new());
                };
                if let Some(title) = params.title {
                    record.title = Some(title).filter(|title| !title.trim().is_empty());
                }
                if let Some(expires_at) = expires_at {
                    record.expires_at = expires_at;
                }
                Ok(())
            })
            .map_err(|reason| {
                if reason.is_empty() {
                    ForwardError::NotFound(params.forward.clone())
                } else {
                    ForwardError::Invalid(reason)
                }
            })?;
        Ok(ForwardResult {
            forward: self
                .inner
                .forwards
                .get(&params.forward)
                .ok_or(ForwardError::NotFound(params.forward))?,
        })
    }

    /// `forward.delete`: it stops, and its share links end.
    pub fn delete_forward(
        &self,
        params: ForwardDeleteParams,
    ) -> Result<ForwardDeleteResult, EngineError> {
        self.delete_forward_handle(&params.forward)?;
        Ok(ForwardDeleteResult {})
    }

    pub(crate) fn delete_forward_handle(&self, handle: &str) -> Result<Forward, ForwardError> {
        let forward = self
            .inner
            .forwards
            .get(handle)
            .ok_or_else(|| ForwardError::NotFound(handle.to_owned()))?;
        self.inner
            .forwards
            .mutate(|records| {
                records.retain(|record| record.handle != handle);
                Ok(())
            })
            .map_err(ForwardError::Invalid)?;
        self.revoke_forward_shares(&HashSet::from([handle.to_owned()]));
        Ok(forward)
    }

    /// Deletes the forwards the session trees `roots` made, as their trees
    /// go.
    pub(crate) fn delete_forwards_owned_by(&self, roots: &HashSet<SessionId>) {
        let owned = self
            .inner
            .forwards
            .records()
            .into_iter()
            .filter(|record| record.owner.is_some_and(|owner| roots.contains(&owner)))
            .map(|record| record.handle)
            .collect::<HashSet<_>>();
        if owned.is_empty() {
            return;
        }
        if let Err(error) = self.inner.forwards.mutate(|records| {
            records.retain(|record| !owned.contains(&record.handle));
            Ok(())
        }) {
            eprintln!("cookie-agent: the deleted sessions' forwards stay: {error}");
        }
        self.revoke_forward_shares(&owned);
    }

    /// Ends the share links of the forwards `handles` (section 6.1); none
    /// exist before share links name forwards.
    pub(crate) fn revoke_forward_shares(&self, _handles: &HashSet<String>) {}
}

/// When a forward made or extended at `now` for `hours` ends: `None` for
/// never (0 or less), 24 hours by default.
fn expiry(
    hours: Option<i64>,
    now: jiff::Timestamp,
) -> Result<Option<jiff::Timestamp>, ForwardError> {
    let hours = hours.unwrap_or(cookie_agent_protocol::FORWARD_DEFAULT_HOURS);
    if hours <= 0 {
        return Ok(None);
    }
    now.checked_add(jiff::SignedDuration::from_hours(hours))
        .map(Some)
        .map_err(|_| ForwardError::Invalid(format!("{hours} hours is too far ahead")))
}
