//! What a cookie session's history says about its ACP sessions across forks
//! and reverts (`docs/site/specs/acp-agents.md` section 15, lifecycle).
//!
//! A cookie session talks to its agent in the ACP session its log records
//! last (`acp_session_started`). A fork copies that record, and a revert
//! can cut the turns the agent still remembers; either way the recorded
//! session is no longer the cookie session's conversation as it stands. The
//! fork or the revert then records the session as detached
//! (`acp_session_updated.detached`) with the agent's last message the
//! cookie session keeps, and the agent's next turn branches off it with
//! the agent's `session/fork`, naming that message as the fork point (the
//! JetBrains AIR extension). A session a revert detached is the cookie
//! session's own and is deleted once the turn has branched; one a fork
//! detached stays its source's. With no agent message kept, the turn
//! starts a new session.
//!
//! An agent that cannot fork there is refused up front
//! ([`rewind_refusal`], [`fork_refusal`]): a revert past its first reply
//! needs a fork that takes a fork point, and a fork of the cookie session
//! needs `session/fork` (and a fork point when it cuts into the
//! conversation). Whether the fork takes a fork point the engine finds by
//! asking once per process ([`super::super::Engine::probe_fork_point`]) and
//! records on the session.
//!
//! The free functions read events; the engine's methods at the end fork,
//! probe, and end the agent's sessions.

use std::{collections::HashMap, sync::Arc, time::Duration};

use cookie_agent_protocol::{
    AcpDetachReason, AcpDetached, AcpSessionRecord, AcpSessionStart, AgentId,
    EventPayload as Event, SessionId, StoredEvent,
};

use super::{
    process::{AgentProcess, agent_error_message},
    runner::SESSION_TIMEOUT,
    wire,
};

/// The member of a committed ACP turn's `response_metadata` that lists the
/// ACP `messageId`s of the agent messages the turn holds, in order.
pub(crate) const MESSAGE_IDS: &str = "acp_message_ids";

/// What the agent's next turn in a cookie session starts from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Plan {
    /// No ACP session recorded for the agent: a new one.
    New,
    /// The recorded session, resumed or loaded.
    Resume(AcpSessionRecord),
    /// The recorded session was left behind by a fork or a revert: branch
    /// off it at `message_id`.
    Branch {
        from: AcpSessionRecord,
        message_id: Option<String>,
        /// Whether the session is the cookie session's own, which a revert
        /// detached: deleted once the turn has branched.
        delete: bool,
    },
}

/// What the agent `agent`'s next turn starts from, by `events`.
pub(crate) fn plan(events: &[Arc<StoredEvent>], agent: &AgentId) -> Plan {
    let mut latest: Option<&AcpSessionRecord> = None;
    let mut detached: Vec<&AcpDetached> = Vec::new();
    for event in events {
        match &event.payload {
            Event::AcpSessionStarted { record } if &record.agent == agent => {
                latest = Some(record);
                detached.clear();
            }
            Event::AcpSessionUpdated { update } => {
                if let (Some(record), Some(marker)) = (latest, &update.detached)
                    && marker.acp_session_id == record.acp_session_id
                {
                    detached.push(marker);
                }
            }
            _ => {}
        }
    }
    let Some(record) = latest else {
        return Plan::New;
    };
    let Some(last) = detached.last() else {
        return Plan::Resume(record.clone());
    };
    let reverted = detached
        .iter()
        .any(|marker| marker.reason == AcpDetachReason::Reverted);
    let forked = detached
        .iter()
        .any(|marker| marker.reason != AcpDetachReason::Reverted);
    Plan::Branch {
        from: record.clone(),
        message_id: last.message_id.clone(),
        delete: reverted && !forked,
    }
}

/// The ACP sessions a fork through `through_seq` of `events` leaves behind:
/// the latest recorded session of each agent, which stays the source's.
pub(crate) fn fork_detached(events: &[Arc<StoredEvent>], through_seq: u64) -> Vec<AcpDetached> {
    let kept = events
        .iter()
        .take_while(|event| event.seq <= through_seq)
        .cloned()
        .collect::<Vec<_>>();
    latest_records(&kept)
        .into_iter()
        .map(|record| AcpDetached {
            message_id: last_message_id(&kept, &record),
            acp_session_id: record.acp_session_id,
            reason: AcpDetachReason::Forked,
        })
        .collect()
}

/// The ACP sessions a revert of `events` (the whole log before it) to
/// `cut` leaves behind: for each agent, the latest session recorded at or
/// before the cut whose conversation reaches past it. The reason is
/// `forked` for a session a fork already left behind, which stays the
/// source's, else `reverted`.
pub(crate) fn revert_detached(events: &[Arc<StoredEvent>], cut: u64) -> Vec<AcpDetached> {
    let kept = events
        .iter()
        .take_while(|event| event.seq <= cut)
        .cloned()
        .collect::<Vec<_>>();
    let removed = &events[kept.len()..];
    latest_records(&kept)
        .into_iter()
        .filter(|record| reaches(removed, record))
        .map(|record| {
            let forked = events.iter().any(|event| {
                matches!(&event.payload, Event::AcpSessionUpdated { update }
                if update.detached.as_ref().is_some_and(|marker| {
                    marker.acp_session_id == record.acp_session_id
                        && marker.reason != AcpDetachReason::Reverted
                }))
            });
            AcpDetached {
                message_id: last_message_id(&kept, &record),
                acp_session_id: record.acp_session_id,
                reason: if forked {
                    AcpDetachReason::Forked
                } else {
                    AcpDetachReason::Reverted
                },
            }
        })
        .collect()
}

/// The agent sessions a revert of `events` to `cut` removes every record
/// of, and that are the cookie session's own (no fork left them behind):
/// gone with the turns the revert removes.
pub(crate) fn revert_removed(events: &[Arc<StoredEvent>], cut: u64) -> Vec<AcpSessionRecord> {
    let kept = events
        .iter()
        .take_while(|event| event.seq <= cut)
        .filter_map(|event| match &event.payload {
            Event::AcpSessionStarted { record } => Some(record.acp_session_id.as_str()),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    let inherited = inherited_ids(events);
    distinct_records(events.iter().filter(|event| event.seq > cut), |id| {
        !kept.contains(id) && !inherited.contains(id)
    })
}

/// The agent sessions `events` record that are the cookie session's own,
/// each once: every one but those a fork left behind, which stay their
/// source's. Deleting the cookie session deletes them.
pub(crate) fn own_records(events: &[Arc<StoredEvent>]) -> Vec<AcpSessionRecord> {
    let inherited = inherited_ids(events);
    distinct_records(events.iter(), |id| !inherited.contains(id))
}

/// The IDs of the agent sessions a fork left behind in `events`.
fn inherited_ids(events: &[Arc<StoredEvent>]) -> std::collections::HashSet<&str> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            Event::AcpSessionUpdated { update } => update
                .detached
                .as_ref()
                .filter(|marker| marker.reason != AcpDetachReason::Reverted)
                .map(|marker| marker.acp_session_id.as_str()),
            _ => None,
        })
        .collect()
}

/// The first record of each agent session `events` record whose ID
/// `keep` accepts.
fn distinct_records<'a>(
    events: impl Iterator<Item = &'a Arc<StoredEvent>>,
    keep: impl Fn(&str) -> bool,
) -> Vec<AcpSessionRecord> {
    let mut records: Vec<AcpSessionRecord> = Vec::new();
    for event in events {
        if let Event::AcpSessionStarted { record } = &event.payload
            && keep(&record.acp_session_id)
            && !records
                .iter()
                .any(|known| known.acp_session_id == record.acp_session_id)
        {
            records.push(record.clone());
        }
    }
    records
}

/// The latest record of each agent in `events`, in the order the agents
/// first appear.
fn latest_records(events: &[Arc<StoredEvent>]) -> Vec<AcpSessionRecord> {
    let mut order = Vec::new();
    let mut latest = HashMap::new();
    for event in events {
        if let Event::AcpSessionStarted { record } = &event.payload {
            if !latest.contains_key(&record.agent) {
                order.push(record.agent.clone());
            }
            latest.insert(record.agent.clone(), record.clone());
        }
    }
    order
        .into_iter()
        .filter_map(|agent| latest.remove(&agent))
        .collect()
}

/// Whether `removed`, the events a revert cuts, held turns of `record`'s
/// agent or a later session of it: what the agent remembers and the cookie
/// session no longer has.
fn reaches(removed: &[Arc<StoredEvent>], record: &AcpSessionRecord) -> bool {
    let model = cookie_agent_protocol::acp_model_selection(&record.agent).model;
    removed.iter().any(|event| match &event.payload {
        Event::ModelAttemptStarted { resolved_model, .. }
        | Event::ModelTurnCommitted { resolved_model, .. } => {
            resolved_model.selection.model == model
        }
        Event::AcpSessionStarted { record: later } => later.agent == record.agent,
        _ => false,
    })
}

/// The ACP `messageId` of the agent's last message in `events` within the
/// conversation of `record`: the committed turns of its agent after the
/// latest new session at or before it (a resumed, loaded, or forked one
/// continues the conversation before it).
fn last_message_id(events: &[Arc<StoredEvent>], record: &AcpSessionRecord) -> Option<String> {
    let model = cookie_agent_protocol::acp_model_selection(&record.agent).model;
    let mut last = None;
    for event in events {
        match &event.payload {
            Event::AcpSessionStarted { record: started }
                if started.agent == record.agent && started.how == AcpSessionStart::New =>
            {
                last = None;
            }
            Event::ModelTurnCommitted {
                resolved_model,
                turn,
                ..
            } if resolved_model.selection.model == model => {
                if let Some(id) = turn
                    .response_metadata
                    .get(MESSAGE_IDS)
                    .and_then(serde_json::Value::as_array)
                    .and_then(|ids| ids.last())
                    .and_then(serde_json::Value::as_str)
                {
                    last = Some(id.to_owned());
                }
            }
            _ => {}
        }
    }
    last
}

/// What the log says about how the agent of `record` forks: whether it
/// offers `session/fork`, and whether that takes a fork point (the
/// record's own word, else a later `acp_session_updated.fork_point`).
/// `None` where the log does not say.
pub(crate) fn fork_support(
    events: &[Arc<StoredEvent>],
    record: &AcpSessionRecord,
) -> (Option<bool>, Option<bool>) {
    let mut fork_point = None;
    let mut started = false;
    for event in events {
        match &event.payload {
            Event::AcpSessionStarted {
                record: started_record,
            } if started_record.acp_session_id == record.acp_session_id => {
                started = true;
                fork_point = started_record.fork_point.or(fork_point);
            }
            Event::AcpSessionUpdated { update } if started => {
                fork_point = update.fork_point.or(fork_point);
            }
            _ => {}
        }
    }
    (record.fork, fork_point)
}

/// How the agent of a record forks, by the log and, where the log does not
/// say, by its running process: whether it offers `session/fork`, and
/// whether that takes a fork point.
pub(crate) type ForkKnowledge<'a> = &'a dyn Fn(&AcpSessionRecord) -> (Option<bool>, Option<bool>);

/// `<agent> on <runtime>`, as refusals name an agent.
fn agent_label(record: &AcpSessionRecord) -> String {
    format!("`{}` on `{}`", record.agent, record.runtime_id)
}

/// Why a revert of `events` to `cut` is refused, if it is: it cuts into
/// the conversation of an agent whose `session/fork` does not take a fork
/// point, or that the engine has not asked yet. A revert to before the
/// agent's first reply is allowed: the next turn starts a new session.
pub(crate) fn rewind_refusal(
    events: &[Arc<StoredEvent>],
    cut: u64,
    known: ForkKnowledge<'_>,
) -> Option<String> {
    let kept = events
        .iter()
        .take_while(|event| event.seq <= cut)
        .cloned()
        .collect::<Vec<_>>();
    revert_detached(events, cut)
        .into_iter()
        .filter(|detached| detached.message_id.is_some())
        .find_map(|detached| {
            let record = latest_records(&kept)
                .into_iter()
                .find(|record| record.acp_session_id == detached.acp_session_id)?;
            let label = agent_label(&record);
            match known(&record) {
                (_, Some(true)) => None,
                (Some(false), _) | (_, Some(false)) => Some(format!(
                    "{label} can't rewind its own conversation, so the session can't be \
                     reverted past the agent's first reply"
                )),
                _ => Some(format!(
                    "cookie has not yet checked whether {label} can rewind its own \
                     conversation (it checks after the agent's next reply), so the session \
                     can't be reverted past the agent's first reply yet"
                )),
            }
        })
}

/// The source's ACP sessions a fork of `events` through `through_seq`
/// forks at once, as the cookie fork is made: those with an agent reply
/// to keep whose fork takes no fork point (or that the engine has not
/// asked yet). [`fork_refusal`] lets such a fork through only at the end
/// of the agent's conversation, where the agent's whole-session fork is
/// exact now but would take in whatever the source runs before the fork's
/// first turn. A fork that takes a fork point waits for that turn.
pub(crate) fn eager_forks(
    events: &[Arc<StoredEvent>],
    through_seq: u64,
    known: ForkKnowledge<'_>,
) -> Vec<AcpSessionRecord> {
    let kept = events
        .iter()
        .take_while(|event| event.seq <= through_seq)
        .cloned()
        .collect::<Vec<_>>();
    latest_records(&kept)
        .into_iter()
        .filter(|record| last_message_id(&kept, record).is_some())
        .filter(|record| known(record).1 != Some(true))
        .collect()
}

/// Why a fork of `events` through `through_seq` is refused, if it is: the
/// agent replied before the fork point but offers no `session/fork`, or
/// the fork point cuts into its conversation and its fork does not take a
/// fork point. A fork before the agent's first reply is allowed: the
/// fork's first turn starts a new session.
pub(crate) fn fork_refusal(
    events: &[Arc<StoredEvent>],
    through_seq: u64,
    known: ForkKnowledge<'_>,
) -> Option<String> {
    let kept = events
        .iter()
        .take_while(|event| event.seq <= through_seq)
        .cloned()
        .collect::<Vec<_>>();
    let after = &events[kept.len()..];
    latest_records(&kept).into_iter().find_map(|record| {
        last_message_id(&kept, &record)?;
        let label = agent_label(&record);
        let (fork, fork_point) = known(&record);
        match fork {
            Some(true) => {}
            Some(false) => {
                return Some(format!(
                    "{label} can't fork its own conversation, so the session can't be forked \
                     after the agent's first reply"
                ));
            }
            None => {
                return Some(format!(
                    "cookie does not know yet whether {label} can fork its own conversation (it \
                     finds out when the agent next starts), so the session can't be forked after \
                     the agent's first reply yet"
                ));
            }
        }
        (reaches(after, &record) && fork_point != Some(true)).then(|| {
            format!(
                "{label} can't fork its own conversation at an earlier point, so the session can \
                 only be forked at the end of the agent's conversation or before its first reply"
            )
        })
    })
}

impl super::super::Engine {
    /// Branches off `from`, a session a fork or a revert left behind
    /// (`delete`: a revert), at `message_id`, the agent's last message the
    /// cookie session `session` keeps: the agent's `session/fork` there
    /// and its answer, or `None` when no agent message is kept and the turn
    /// starts a new session. A notice says how the agent goes on; a fork
    /// the agent refuses fails the turn.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn branch_acp_session(
        &self,
        session: SessionId,
        process: &AgentProcess,
        from: &AcpSessionRecord,
        message_id: Option<&str>,
        delete: bool,
        cwd: &str,
        additional_directories: &[String],
        mcp_servers: &[wire::McpServer],
    ) -> Result<Option<(String, wire::SessionResponse)>, super::super::EngineError> {
        let Some(message_id) = message_id else {
            let notice = if delete {
                "The revert point is before the agent's first reply, so this turn starts a new \
                 session."
            } else {
                "The session was forked before the agent's first reply, so this turn starts a \
                 new session."
            };
            self.acp_notice(session, None, notice.to_owned()).await;
            return Ok(None);
        };
        let forked = self
            .fork_acp_session(
                process,
                from,
                message_id,
                cwd,
                additional_directories,
                mcp_servers,
            )
            .await
            .map_err(|reason| {
                let what = if delete {
                    "continue from the revert point"
                } else {
                    "fork the conversation this session was forked from"
                };
                super::super::EngineError::Acp(format!("the agent cannot {what}: {reason}"))
            })?;
        if delete {
            self.acp_notice(
                session,
                None,
                "The agent continues from the revert point: its conversation after it is gone."
                    .to_owned(),
            )
            .await;
        }
        Ok(Some(forked))
    }

    /// Asks the agent of `live` once per process whether its
    /// `session/fork` takes a fork point: a fork of `live`'s session at a
    /// message that cannot exist is refused as invalid params (`-32602`)
    /// by an agent that reads the fork point, and succeeds for one that
    /// ignores it, whose fork is then deleted; any other answer counts as
    /// no. `live`'s session needs a reply of the agent's in it (Claude
    /// Code's adapter cannot fork a session it has not written yet). The
    /// answer stays with the process and is recorded on every cookie
    /// session it serves.
    pub(super) async fn probe_fork_point(&self, live: &super::runner::LiveSession) {
        let process = &live.process;
        if !process.init().capabilities.session("fork")
            || process.fork_point.get().is_some()
            || process
                .fork_probe
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let missing = format!("cookie-fork-probe-{}", SessionId::new_v7());
        let answer = tokio::time::timeout(
            SESSION_TIMEOUT,
            process.peer.request::<_, wire::SessionResponse>(
                wire::method::SESSION_FORK,
                &wire::ForkSessionRequest {
                    session_id: &live.acp_session_id,
                    cwd: &live.cwd,
                    additional_directories: &[],
                    mcp_servers: &[],
                    meta: Some(wire::air_fork_point(&missing)),
                },
            ),
        )
        .await;
        let supported = match answer {
            Ok(Err(cookie_agent_runtime_api::rpc::RpcError::Remote { code: -32602, .. })) => true,
            Ok(Ok(response)) => {
                if let Some(id) = response.session_id.filter(|id| !id.is_empty()) {
                    let deletes = process.init().capabilities.session("delete");
                    end_agent_session(process, &id, !deletes, deletes).await;
                    process.unroute(&id);
                }
                false
            }
            _ => false,
        };
        let _ = process.fork_point.set(supported);
        let sessions = self
            .inner
            .acp
            .processes
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|known| Arc::ptr_eq(&known.process, process))
            .map(|known| known.session)
            .collect::<Vec<_>>();
        for session in sessions {
            let _ = self
                .append(
                    session,
                    None,
                    super::super::event_origin("engine:acp"),
                    Event::AcpSessionUpdated {
                        update: cookie_agent_protocol::AcpSessionUpdate {
                            fork_point: Some(supported),
                            ..Default::default()
                        },
                    },
                )
                .await;
        }
    }

    /// Forks, for the cookie fork `forked` just made, each of its source's
    /// ACP sessions [`eager_forks`] names, starting the agent's process
    /// when it is not running: the fork's agent session then holds exactly
    /// the conversation up to the fork point. Refused, naming the agent,
    /// when the agent cannot.
    pub(crate) async fn fork_acp_sessions_now(
        &self,
        source_events: &[Arc<StoredEvent>],
        through_seq: u64,
        forked: SessionId,
    ) -> Result<(), super::super::EngineError> {
        let known = |record: &AcpSessionRecord| self.fork_knowledge(source_events, record);
        for record in eager_forks(source_events, through_seq, &known) {
            if let Err(error) = self.fork_acp_session_now(forked, &record).await {
                return Err(super::super::EngineError::AcpUnsupported(format!(
                    "{} could not fork its own conversation for the new session: {}",
                    agent_label(&record),
                    error.user_message().trim_end_matches('.')
                )));
            }
        }
        Ok(())
    }

    /// Opens `forked`'s ACP session for the agent of `record` now, which
    /// branches off `record`'s session as the fork's first turn would.
    async fn fork_acp_session_now(
        &self,
        forked: SessionId,
        record: &AcpSessionRecord,
    ) -> Result<(), super::super::EngineError> {
        let runtime = self.current_runtime();
        let resolved = runtime
            .agents
            .get(&record.agent)
            .filter(|agent| agent.acp.is_some())
            .ok_or_else(|| {
                super::super::EngineError::Acp("no runtime offers the agent now".to_owned())
            })?;
        let offers = resolved.acp.clone().expect("filtered to ACP agents");
        let agent = self.place_acp_agent(forked, &offers)?;
        let (process, _use) = self
            .inner
            .acp
            .processes
            .acquire(&self.inner, &agent)
            .await
            .map_err(super::super::EngineError::Acp)?;
        self.acp_session(forked, None, &agent, &resolved.document, &process)
            .await?;
        // A session on another runtime than the one that runs it now, or
        // of another declaration, starts anew there, which forks nothing.
        match plan(
            &self.inner.store.get(forked)?.log.event_snapshot(),
            &record.agent,
        ) {
            Plan::Resume(started) if started.how == AcpSessionStart::Forked => Ok(()),
            _ if record.runtime_id == agent.runtime => {
                Err(super::super::EngineError::Acp(format!(
                    "its session is of ACP agent `{}`, and its document now runs `{}`",
                    record.runtime_agent, agent.agent
                )))
            }
            _ => Err(super::super::EngineError::Acp(format!(
                "its session is on runtime `{}`, which does not run it for the new session",
                record.runtime_id
            ))),
        }
    }

    /// How the agent of `record` forks: by the log of `events`, and where
    /// that does not say, by the agent's running process.
    pub(crate) fn fork_knowledge(
        &self,
        events: &[Arc<StoredEvent>],
        record: &AcpSessionRecord,
    ) -> (Option<bool>, Option<bool>) {
        let (fork, fork_point) = fork_support(events, record);
        let process = self
            .inner
            .acp
            .processes
            .running(&super::process::ProcessKey {
                runtime: record.runtime_id.to_string(),
                agent: record.runtime_agent.to_string(),
            });
        let live_fork = process
            .as_ref()
            .map(|process| process.init().capabilities.session("fork"));
        let live_point = process
            .as_ref()
            .and_then(|process| process.fork_point.get().copied());
        (live_fork.or(fork), live_point.or(fork_point))
    }

    /// `session/fork` of `from` at the agent message `message_id`, as the
    /// next turn branches off a session a fork or a revert left behind:
    /// the new session's ID and the agent's answer, or why it could not.
    async fn fork_acp_session(
        &self,
        process: &AgentProcess,
        from: &AcpSessionRecord,
        message_id: &str,
        cwd: &str,
        additional_directories: &[String],
        mcp_servers: &[wire::McpServer],
    ) -> Result<(String, wire::SessionResponse), String> {
        if !process.init().capabilities.session("fork") {
            return Err("it does not offer `session/fork`".to_owned());
        }
        let response = tokio::time::timeout(
            SESSION_TIMEOUT,
            process.peer.request::<_, wire::SessionResponse>(
                wire::method::SESSION_FORK,
                &wire::ForkSessionRequest {
                    session_id: &from.acp_session_id,
                    cwd,
                    additional_directories,
                    mcp_servers,
                    meta: Some(wire::air_fork_point(message_id)),
                },
            ),
        )
        .await
        .map_err(|_| {
            format!(
                "it did not answer `session/fork` within {} s",
                SESSION_TIMEOUT.as_secs()
            )
        })?
        .map_err(|error| {
            format!(
                "`session/fork` failed: {}",
                process
                    .exit_reason()
                    .unwrap_or_else(|| agent_error_message(&error))
            )
        })?;
        let id = response
            .session_id
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "it answered `session/fork` without a session ID".to_owned())?;
        Ok((id, response))
    }

    /// Ends the agent's session `acp_session_id`, which a revert left
    /// behind and the turn replaced: the live one of `session` closes
    /// (`session/close`) as a deleted session's does, and the agent deletes
    /// it when it offers `session/delete`.
    pub(super) async fn retire_acp_session(
        &self,
        session: SessionId,
        process: &Arc<AgentProcess>,
        acp_session_id: &str,
    ) {
        let live = {
            let mut sessions = self
                .inner
                .acp
                .processes
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match sessions.get(&session) {
                Some(live) if live.acp_session_id == acp_session_id => sessions.remove(&session),
                _ => None,
            }
        };
        match live {
            Some(live) => self.end_live_session(live, true).await,
            None => end_agent_session(process, acp_session_id, false, true).await,
        }
    }

    /// Deletes the agent's sessions `records`, which were `session`'s own
    /// and no longer are: its live one closes first, and the others go to
    /// the agent's process on the runtime that holds them, started for it
    /// when it is not running. An agent its runtime no longer offers keeps
    /// them.
    pub(crate) async fn delete_acp_sessions(
        &self,
        session: SessionId,
        records: Vec<AcpSessionRecord>,
    ) {
        for record in records {
            let live = {
                let mut sessions = self
                    .inner
                    .acp
                    .processes
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match sessions.get(&session) {
                    Some(live) if live.acp_session_id == record.acp_session_id => {
                        sessions.remove(&session)
                    }
                    _ => None,
                }
            };
            if let Some(live) = live {
                self.end_live_session(live, true).await;
                continue;
            }
            let Some(agent) = self.inner.acp.runtime_agents().into_iter().find(|agent| {
                agent.runtime == record.runtime_id
                    && agent.agent == record.runtime_agent
                    && agent.available()
            }) else {
                continue;
            };
            if let Ok((process, _use)) = self.inner.acp.processes.acquire(&self.inner, &agent).await
            {
                end_agent_session(&process, &record.acp_session_id, false, true).await;
            }
        }
    }
}

/// How long the agent gets to answer `session/close` and `session/delete`.
const END_TIMEOUT: Duration = Duration::from_secs(10);

/// Asks the agent to close its session `acp_session_id` (`close`; every v2
/// agent offers `session/close`) and then to delete it (`delete`, when it
/// offers `session/delete`). What it answers changes nothing.
pub(super) async fn end_agent_session(
    process: &AgentProcess,
    acp_session_id: &str,
    close: bool,
    delete: bool,
) {
    let caps = &process.init().capabilities;
    let reference = wire::SessionRef {
        session_id: acp_session_id,
    };
    let request = |method| {
        tokio::time::timeout(
            END_TIMEOUT,
            process
                .peer
                .request::<_, serde_json::Value>(method, &reference),
        )
    };
    if close {
        let _ = request(wire::method::SESSION_CLOSE).await;
    }
    if delete && caps.session("delete") {
        let _ = request(wire::method::SESSION_DELETE).await;
    }
}
