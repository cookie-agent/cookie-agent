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

use cookie_agent_protocol::{BackgroundActor, BackgroundCall, SessionId, ToolCallId};

use super::super::Engine;
use super::process::ProcessUse;
use super::runner::LiveSession;

/// One command of the agent's, running after its call ended.
pub(crate) struct AgentCommand {
    session: SessionId,
    /// Its terminal's ID, the job clients name it by.
    terminal: String,
    tool_call_id: ToolCallId,
    tool: String,
    display: Option<String>,
    since: jiff::Timestamp,
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
                terminal,
                tool_call_id,
                tool,
                display,
                since: jiff::Timestamp::now(),
                _use: ProcessUse::hold(&live.process, &self.inner),
            });
        }
        self.notify_background_changed(live.session);
    }

    /// Terminal `terminal` of `session` exited.
    pub(super) fn acp_terminal_exited(&self, session: SessionId, terminal: &str) {
        let removed = {
            let mut commands = self.acp_commands();
            let before = commands.len();
            commands
                .retain(|command| !(command.session == session && command.terminal == terminal));
            commands.len() != before
        };
        if removed {
            self.notify_background_changed(session);
        }
    }

    /// The agent's process that ran `session`'s commands is gone, and they
    /// with it.
    pub(super) fn acp_commands_gone(&self, session: SessionId) {
        let removed = {
            let mut commands = self.acp_commands();
            let before = commands.len();
            commands.retain(|command| command.session != session);
            commands.len() != before
        };
        if removed {
            self.notify_background_changed(session);
        }
    }

    /// `session`'s agent commands, as background calls.
    pub(crate) fn acp_background_calls(&self, session: SessionId) -> Vec<BackgroundCall> {
        self.acp_commands()
            .iter()
            .filter(|command| command.session == session)
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
        self.acp_commands()
            .iter()
            .any(|command| command.session == session && command.terminal == job)
    }

    /// Whether `session` has an agent command running.
    pub(crate) fn acp_background_running(&self, session: SessionId) -> bool {
        self.acp_commands()
            .iter()
            .any(|command| command.session == session)
    }

    /// How many agent commands run, in every session.
    pub(crate) fn acp_background_count(&self) -> usize {
        self.acp_commands().len()
    }
}
