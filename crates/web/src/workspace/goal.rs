//! The root session's goal: `/goal` and the goal bar. Only durable
//! `goal_*` events change the projection the bar reads; an answer only
//! says how the command went.

use cookie_agent_protocol::{
    GoalLifecycleAction, GoalState, SessionGoalGetParams, SessionGoalLifecycleParams,
    SessionGoalSetParams,
};
use leptos::{prelude::*, task::spawn_local};

use super::{Panel, Workspace};
use crate::goal::{GoalCommand, needs_selection, status_name};

impl Workspace {
    /// Whether the viewed session is the root of its tree, where goals
    /// live.
    pub fn viewing_root(self) -> bool {
        let Some(session_id) = self.selected.get() else {
            return false;
        };
        !self.new_session.get()
            && self.tree_root.get().is_none_or(|root| root == session_id)
            && self.sessions.with(|sessions| {
                sessions
                    .iter()
                    .find(|session| session.session_id == session_id)
                    .is_none_or(|session| {
                        matches!(session.origin, cookie_agent_protocol::SessionOrigin::Root)
                    })
            })
    }

    /// The viewed root session's goal.
    pub fn goal(self) -> Option<GoalState> {
        if !self.viewing_root() {
            return None;
        }
        let session_id = self.selected.get()?;
        self.with_store(|store| store.sessions.get(&session_id)?.goal.clone())
    }

    /// Why a goal command cannot run in this view, if it cannot.
    fn goal_blocker(self) -> Option<&'static str> {
        let session_id = self.selected.get_untracked()?;
        if !self.viewing_root() {
            return Some("goal commands are only available in root sessions");
        }
        if self
            .read_only
            .with_untracked(|read_only| read_only.contains(&session_id))
        {
            return Some("cannot change a goal in a read-only session");
        }
        None
    }

    /// Runs `/goal`.
    pub fn goal_command(self, command: GoalCommand) {
        if self.selected.get_untracked().is_none() || self.new_session.get_untracked() {
            self.status
                .set("select a root session before using /goal".into());
            return;
        }
        if command == GoalCommand::Show {
            self.panel.set(Some(Panel::Goal));
            return;
        }
        if let Some(reason) = self.goal_blocker() {
            self.status.set(reason.into());
            return;
        }
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        let wants_selection = match &command {
            GoalCommand::Objective(_) => true,
            GoalCommand::Lifecycle(action) => needs_selection(*action),
            GoalCommand::Show => false,
        };
        let selection = if wants_selection {
            let Some(selection) = self.draft.get_untracked() else {
                self.status
                    .set("select an agent and model before activating or resuming a goal".into());
                return;
            };
            Some(selection)
        } else {
            None
        };
        self.status.set("updating goal…".into());
        spawn_local(async move {
            let result = match command {
                GoalCommand::Objective(objective) => client
                    .set_session_goal(SessionGoalSetParams {
                        session_id,
                        objective,
                        selection,
                    })
                    .await
                    .map(|result| result.goal)
                    .map_err(|error| crate::errors::describe_full(&error)),
                GoalCommand::Lifecycle(action) => {
                    lifecycle(&client, session_id, action, selection).await
                }
                GoalCommand::Show => return,
            };
            if self.selected.get_untracked() == Some(session_id) {
                self.status.set(match result {
                    Ok(goal) => format!("goal {}", status_name(goal.status)),
                    Err(error) => format!("goal command failed: {error}"),
                });
            }
        });
    }
}

/// Pauses, resumes, or cancels the session's goal at the revision the
/// daemon has now.
async fn lifecycle(
    client: &cookie_agent_protocol::Client,
    session_id: cookie_agent_protocol::SessionId,
    action: GoalLifecycleAction,
    selection: Option<cookie_agent_protocol::RunSelection>,
) -> Result<GoalState, String> {
    let goal = client
        .get_session_goal(SessionGoalGetParams { session_id })
        .await
        .map_err(|error| crate::errors::describe_full(&error))?
        .goal
        .ok_or_else(|| "no goal is set for this session".to_owned())?;
    client
        .change_session_goal_lifecycle(SessionGoalLifecycleParams {
            session_id,
            goal_id: goal.goal_id,
            expected_revision: goal.revision,
            action,
            selection,
        })
        .await
        .map(|result| result.goal)
        .map_err(|error| crate::errors::describe_full(&error))
}
