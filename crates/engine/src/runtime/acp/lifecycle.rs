//! What a cookie session's history says about its ACP sessions across forks
//! and reverts (`docs/site/specs/acp-agents.md` section 15, lifecycle).
//!
//! A cookie session talks to its agent in the ACP session its log records
//! last (`acp_session_started`). A fork copies that record, and a revert
//! can cut the turns the agent still remembers; either way the recorded
//! session is no longer the cookie session's conversation as it stands. The
//! fork or the revert then records the session as detached
//! (`acp_session_updated.detached`) with the agent's last message the
//! cookie session keeps, and the agent's next turn branches off it: the
//! agent's `session/fork` at that message when it offers one, else a new
//! session whose first prompt carries the cookie transcript
//! ([`transcript`]). A session a revert detached is the cookie session's own
//! and is deleted once the turn has branched; one a fork detached stays its
//! source's.
//!
//! The free functions read events; the engine's methods at the end fork
//! and end the agent's sessions.

use std::{collections::HashMap, sync::Arc, time::Duration};

use cookie_agent_protocol::{
    AcpDetachReason, AcpDetached, AcpSessionRecord, AcpSessionStart, AgentId,
    EventPayload as Event, PersistedAssistantPart, SessionId, StoredEvent,
};

use super::{
    process::{AgentProcess, agent_error_message},
    runner::SESSION_TIMEOUT,
    wire,
};

/// The member of a committed ACP turn's `response_metadata` that lists the
/// ACP `messageId`s of the agent messages the turn holds, in order.
pub(crate) const MESSAGE_IDS: &str = "acp_message_ids";

/// The longest cookie transcript a new ACP session's first prompt carries,
/// in bytes; older entries are left out first.
const TRANSCRIPT_BYTES: usize = 64 * 1024;
/// How much of one tool call's input and of its result the transcript
/// keeps, in bytes.
const TOOL_INPUT_BYTES: usize = 200;
const TOOL_RESULT_BYTES: usize = 600;

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

/// The cookie transcript a new ACP session that continues a conversation
/// gets in its first prompt: the user's and producers' messages, the
/// assistant's text, and each tool call with its input and result cut
/// short, up to the input of the round about to run (after the last
/// committed turn's `input_through_seq`), which is the prompt itself.
/// `None` when there is nothing before that input.
pub(crate) fn transcript(events: &[Arc<StoredEvent>]) -> Option<String> {
    let boundary = events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            Event::ModelTurnCommitted {
                input_through_seq, ..
            } => Some(*input_through_seq),
            _ => None,
        })
        .unwrap_or(0);
    let producers = crate::goal_projection::GoalProducerProjection::from_events(events);
    let admitted = producers
        .messages
        .iter()
        .filter(|message| !message.discarded)
        .filter_map(|message| message.admission.map(|(_, seq)| (seq, &message.body)))
        .collect::<HashMap<_, _>>();
    let mut submitted = HashMap::new();
    let mut calls = HashMap::new();
    let mut entries = Vec::new();
    for event in events {
        match &event.payload {
            Event::UserInputSubmitted { content } => {
                submitted.insert(event.seq, content);
            }
            Event::UserInputApplied { user_input_seq } if event.seq <= boundary => {
                if let Some(content) = submitted.get(user_input_seq) {
                    entries.push(format!("User: {}", content.as_plain_text().trim()));
                }
            }
            Event::ProducerMessageAdmitted { .. } if event.seq <= boundary => {
                if let Some(body) = admitted.get(&event.seq) {
                    entries.push(format!("Message: {}", body.as_plain_text().trim()));
                }
            }
            Event::ModelTurnCommitted {
                model_turn_seq,
                turn,
                ..
            } => {
                for part in &turn.content {
                    match part {
                        PersistedAssistantPart::Text { text, .. } if !text.trim().is_empty() => {
                            entries.push(format!("Assistant: {}", text.trim()));
                        }
                        PersistedAssistantPart::ToolCall {
                            id, name, input, ..
                        } => {
                            calls.insert(
                                (*model_turn_seq, id.as_str().to_owned()),
                                (name.as_str().to_owned(), input.to_string()),
                            );
                        }
                        _ => {}
                    }
                }
            }
            Event::ToolCallTerminated { termination } => {
                let owner = &termination.owner;
                let Some((name, input)) = calls.get(&(
                    owner.model_turn_seq,
                    owner.model_call_id.as_str().to_owned(),
                )) else {
                    continue;
                };
                let result = match (&termination.result, &termination.error) {
                    (Some(result), _) => result.output.clone(),
                    (None, Some(error)) => format!("error: {}", error.message.as_str()),
                    (None, None) => String::new(),
                };
                entries.push(format!(
                    "Tool `{name}` {}: {}",
                    cut(input, TOOL_INPUT_BYTES),
                    cut(result.trim(), TOOL_RESULT_BYTES)
                ));
            }
            _ => {}
        }
    }
    if entries.is_empty() {
        return None;
    }
    let mut kept = Vec::new();
    let mut bytes = 0;
    for entry in entries.iter().rev() {
        if bytes + entry.len() > TRANSCRIPT_BYTES && !kept.is_empty() {
            break;
        }
        bytes += entry.len();
        kept.push(cut(entry, TRANSCRIPT_BYTES));
    }
    let omitted = entries.len() - kept.len();
    kept.reverse();
    let mut text = String::from(
        "<cookie_transcript>\nThis conversation continues one you do not remember (the session was \
         forked or reverted). Here it is so far, oldest first; tool inputs and results are cut \
         short.\n\n",
    );
    if omitted > 0 {
        text.push_str(&format!("[{omitted} earlier entries left out]\n\n"));
    }
    text.push_str(&kept.join("\n\n"));
    text.push_str("\n</cookie_transcript>");
    Some(text)
}

/// `text` cut to at most `max` bytes at a character boundary, with `…`
/// when it was cut.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

impl super::super::Engine {
    /// `session/fork` of `from` at the agent message `message_id`, as the
    /// next turn branches off a session a fork or a revert left behind:
    /// the new session's ID and the agent's answer, or why it could not.
    pub(super) async fn fork_acp_session(
        &self,
        process: &AgentProcess,
        from: &AcpSessionRecord,
        message_id: Option<&str>,
        cwd: &str,
        mcp_servers: &[wire::McpServer],
    ) -> Result<(String, wire::SessionResponse), String> {
        if !process.init().agent_capabilities.session("fork") {
            return Err("it does not offer `session/fork`".to_owned());
        }
        let Some(message_id) = message_id else {
            return Err("the session keeps none of its messages to fork at".to_owned());
        };
        let response = tokio::time::timeout(
            SESSION_TIMEOUT,
            process.peer.request::<_, wire::SessionResponse>(
                wire::method::SESSION_FORK,
                &wire::ForkSessionRequest {
                    session_id: &from.acp_session_id,
                    cwd,
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
    /// behind and the turn replaced: the live one of `session` closes as a
    /// deleted session's does, and the agent deletes it
    /// (`session/delete`, else `session/close`).
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
}

/// How long the agent gets to answer `session/close` and `session/delete`.
const END_TIMEOUT: Duration = Duration::from_secs(10);

/// Asks the agent to close (`close`, when it offers it) and then delete
/// (`delete`, when it offers it; else a close) its session
/// `acp_session_id`. What it answers changes nothing.
pub(super) async fn end_agent_session(
    process: &AgentProcess,
    acp_session_id: &str,
    close: bool,
    delete: bool,
) {
    let caps = &process.init().agent_capabilities;
    let deletes = delete && caps.session("delete");
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
    if (close || (delete && !deletes)) && caps.session("close") {
        let _ = request(wire::method::SESSION_CLOSE).await;
    }
    if deletes {
        let _ = request(wire::method::SESSION_DELETE).await;
    }
}
