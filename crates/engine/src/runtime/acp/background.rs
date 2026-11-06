//! The ACP agent's commands still running after their tool calls ended,
//! such as Claude Code's commands run in the background
//! (`docs/site/specs/acp-v2.md` section 9).
//!
//! A command's ACP terminal stays open until it exits (`exitStatus`). While
//! it does after its call ended, the command is background work of the
//! session: listed beside cookie's background calls (`agent_task`), kept
//! from eviction, waited for by an idle daemon restart, and keeping the
//! agent's process from idling out. It ends when the terminal exits, or with
//! the agent's process.
//!
//! An agent can take a while to follow up on a command that ended, so an
//! exited command settles before it counts as gone: it leaves the list at
//! once, but until the agent's next round, its process's end, or
//! [`ACP_BACKGROUND_SETTLE`] after the exit, it still holds the session, an
//! idle restart, and the process as it did while it ran.

use std::time::Duration;

use cookie_agent_protocol::{
    ACP_BACKGROUND_SETTLE, AcpBackgroundEnded, BackgroundActor, BackgroundCall, SessionId,
    ToolCallId,
};
use tokio::time::Instant;

use super::super::Engine;
use super::process::ProcessUse;
use super::runner::LiveSession;
use super::terminal::Exit;

/// One command of the agent's, running after its call ended.
pub(crate) struct AgentCommand {
    session: SessionId,
    /// The runtime the agent runs on.
    runtime: cookie_agent_protocol::RuntimeId,
    /// Its terminal's ID, the job clients name it by.
    terminal: String,
    tool_call_id: ToolCallId,
    tool: String,
    display: Option<String>,
    since: jiff::Timestamp,
    /// Once its terminal exited: when it stops waiting for the agent to
    /// follow up.
    settles_at: Option<Instant>,
    /// Keeps the agent's process from idling out meanwhile.
    _use: ProcessUse,
}

impl Engine {
    fn acp_commands(&self) -> std::sync::MutexGuard<'_, Vec<AgentCommand>> {
        self.inner
            .acp
            .background
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The agent's call `tool_call_id` (tool `tool`, shown as `display`)
    /// ended while its terminal `terminal` still runs.
    pub(super) fn acp_command_backgrounded(
        &self,
        live: &LiveSession,
        terminal: String,
        tool_call_id: ToolCallId,
        tool: String,
        display: Option<String>,
    ) {
        if live.process.exited.is_cancelled() {
            return;
        }
        {
            let mut commands = self.acp_commands();
            if commands
                .iter()
                .any(|command| command.session == live.session && command.terminal == terminal)
            {
                return;
            }
            commands.push(AgentCommand {
                session: live.session,
                runtime: live.agent.runtime.clone(),
                terminal,
                tool_call_id,
                tool,
                display,
                since: jiff::Timestamp::now(),
                settles_at: None,
                _use: ProcessUse::hold(&live.process, &self.inner),
            });
        }
        self.notify_background_changed(live.session);
        self.refresh_live_waiting(live.session);
    }

    /// Terminal `terminal` of `session` exited, as `exit` says: the
    /// command that ran in the background on it, when one did, for the
    /// session to record that it ended. The command settles from here.
    pub(super) fn acp_terminal_exited(
        &self,
        session: SessionId,
        terminal: &str,
        exit: Exit,
    ) -> Option<AcpBackgroundEnded> {
        let settles_at = Instant::now() + self.acp_settle_window();
        let ended = {
            let mut commands = self.acp_commands();
            let command = commands.iter_mut().find(|command| {
                command.session == session
                    && command.terminal == terminal
                    && command.settles_at.is_none()
            })?;
            command.settles_at = Some(settles_at);
            AcpBackgroundEnded {
                tool_call_id: command.tool_call_id,
                tool: command.tool.clone(),
                display: command.display.clone(),
                started_at: command.since,
                exit_code: exit.code,
                signal: exit.signal,
            }
        };
        let inner = std::sync::Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            tokio::time::sleep_until(settles_at).await;
            if let Some(inner) = inner.upgrade() {
                Engine { inner }.acp_commands_settle(session, |at| at <= Instant::now());
            }
        });
        self.notify_background_changed(session);
        self.refresh_live_waiting(session);
        Some(ended)
    }

    /// A round of `session`'s agent started, which takes in any follow-up:
    /// its exited commands stop waiting for one.
    pub(super) fn acp_commands_followed_up(&self, session: SessionId) {
        self.acp_commands_settle(session, |_| true);
    }

    /// Ends `session`'s exited commands whose settling time `due` says.
    fn acp_commands_settle(&self, session: SessionId, due: impl Fn(Instant) -> bool) {
        // The process may idle once nothing else holds it, which takes its
        // own locks: these go after the list's.
        let settled = {
            let mut commands = self.acp_commands();
            let (settled, kept) = std::mem::take(&mut *commands)
                .into_iter()
                .partition::<Vec<_>, _>(|command| {
                    command.session == session && command.settles_at.is_some_and(&due)
                });
            *commands = kept;
            settled
        };
        if !settled.is_empty() {
            drop(settled);
            self.notify_background_changed(session);
            self.refresh_live_waiting(session);
        }
    }

    /// How long an exited command waits for the agent to follow up.
    fn acp_settle_window(&self) -> Duration {
        #[cfg(test)]
        if let Some(window) = *self
            .inner
            .acp
            .settle_window
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            return window;
        }
        ACP_BACKGROUND_SETTLE
    }

    /// The agent's process that ran `session`'s commands is gone, and they
    /// with it, and the work of `session` when it is a subagent of the
    /// agent's.
    pub(super) fn acp_commands_gone(&self, session: SessionId) {
        self.set_acp_subagent_working(session, false);
        let removed = {
            let mut commands = self.acp_commands();
            let before = commands.len();
            commands.retain(|command| command.session != session);
            commands.len() != before
        };
        if removed {
            self.notify_background_changed(session);
            self.refresh_live_waiting(session);
        }
    }

    /// `session`'s agent commands, as background calls.
    pub(crate) fn acp_background_calls(&self, session: SessionId) -> Vec<BackgroundCall> {
        self.acp_commands()
            .iter()
            .filter(|command| command.session == session && command.settles_at.is_none())
            .map(|command| BackgroundCall {
                job: command.terminal.clone(),
                tool: command.tool.clone(),
                tool_call_id: command.tool_call_id,
                display: command.display.clone(),
                started_at: command.since,
                detached_at: command.since,
                by: BackgroundActor::Model,
                agent_task: true,
            })
            .collect()
    }

    /// Whether `job` is a running agent command of `session`.
    pub(crate) fn acp_terminal_running(&self, session: SessionId, job: &str) -> bool {
        self.acp_commands().iter().any(|command| {
            command.session == session && command.terminal == job && command.settles_at.is_none()
        })
    }

    /// Whether `session` has an agent command running.
    pub(crate) fn acp_background_running(&self, session: SessionId) -> bool {
        self.acp_commands()
            .iter()
            .any(|command| command.session == session && command.settles_at.is_none())
    }

    /// Whether `session` has an agent command running or settling, which
    /// may yet wake it.
    pub(crate) fn acp_background_pending(&self, session: SessionId) -> bool {
        self.acp_commands()
            .iter()
            .any(|command| command.session == session)
    }

    /// How many agent commands run or settle, in every session.
    pub(crate) fn acp_background_count(&self) -> usize {
        self.acp_commands().len()
    }

    /// The runtime of each agent command that runs or settles, in every
    /// session.
    pub(crate) fn acp_background_runtimes(&self) -> Vec<String> {
        self.acp_commands()
            .iter()
            .map(|command| command.runtime.as_str().to_owned())
            .collect()
    }
}
