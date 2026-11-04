//! The viewed session's tool groups and skills
//! (`docs/site/specs/session-tool-groups.md` section 12): read with
//! `session.tools.get`, and each choice of one group or one skill written
//! with `session.tools.set`; and the choices a new session, or a
//! project's new sessions, start with, on the runtime cards.

use cookie_agent_protocol::{
    ProjectId, RuntimeGroupsParams, RuntimeGroupsResult, SessionToolsGetParams,
    SessionToolsSetParams, SkillState, ToolExposure, ToolGroupSelector, ToolGroupState,
};
use leptos::{prelude::*, task::spawn_local};

use super::{CardSet, Workspace};
use crate::cards::Card;

/// What `runtimes.groups` is asked about: a runtime, its capability, and
/// the workspace typed on its card.
pub type GroupsKey = (String, Option<String>, Option<String>);

/// The key of `card` at `workspace`, a field left empty as none.
pub fn groups_key(card: &Card, workspace: Option<&str>) -> GroupsKey {
    (
        card.runtime_id.to_string(),
        card.capability.as_ref().map(ToString::to_string),
        workspace
            .map(str::trim)
            .filter(|workspace| !workspace.is_empty())
            .map(str::to_owned),
    )
}

pub use cookie_agent_client_state::exposure::from_project;
use cookie_agent_client_state::exposure::{with_group, with_skill};

impl Workspace {
    /// Reads the viewed session's tool groups and skills.
    pub fn poll_session_tools(self) {
        let read = self.begin_read(|reads| &mut reads.session_tools);
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        if self.new_session.get_untracked() {
            return;
        }
        spawn_local(async move {
            let result = client
                .get_session_tools(SessionToolsGetParams { session_id })
                .await
                .map_err(|error| crate::errors::describe_full(&error));
            if self.read_is_current(|reads| &mut reads.session_tools, read) {
                self.session_tools.set(Some(result));
            }
        });
    }

    /// The session's own choices now, as `session.tools.get` last read them.
    fn session_exposure(self) -> Option<ToolExposure> {
        self.session_tools.with_untracked(|tools| match tools {
            Some(Ok(tools)) => Some(tools.exposure.clone()),
            _ => None,
        })
    }

    /// Chooses `state` for the group `selector`, or with `None` removes the
    /// session's choice. Choosing what the layers below give (`below`)
    /// removes it too, rather than storing a copy.
    pub fn choose_tool_group(
        self,
        selector: ToolGroupSelector,
        state: Option<ToolGroupState>,
        below: ToolGroupState,
    ) {
        let Some(exposure) = self.session_exposure() else {
            return;
        };
        self.write_session_tools(with_group(exposure, selector, state, below));
    }

    /// Chooses `state` for the skill `name`, or with `None` removes the
    /// session's choice.
    pub fn choose_skill(self, name: String, state: Option<SkillState>, below: SkillState) {
        let Some(exposure) = self.session_exposure() else {
            return;
        };
        self.write_session_tools(with_skill(exposure, &name, state, below));
    }

    fn write_session_tools(self, exposure: ToolExposure) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        spawn_local(async move {
            match client
                .set_session_tools(SessionToolsSetParams {
                    session_id,
                    exposure,
                })
                .await
            {
                Ok(result) => {
                    // A read that left before the change would land stale.
                    let _ = self.begin_read(|reads| &mut reads.session_tools);
                    self.session_tools.set(Some(Ok(result)));
                }
                Err(error) => self.status.set(format!(
                    "tool groups not changed: {}",
                    crate::errors::describe(&error)
                )),
            }
        });
    }
}

impl Workspace {
    /// Reads what `card` offers at `workspace` once a draft: live from a
    /// running instance, else as it last ran.
    pub fn read_card_offer(self, card: &Card, workspace: Option<&str>) {
        let key = groups_key(card, workspace);
        let asked = self
            .runtime_groups
            .with_untracked(|groups| groups.contains_key(&key));
        let Some(client) = self.client() else {
            return;
        };
        if asked {
            return;
        }
        self.runtime_groups.update(|groups| {
            groups.insert(key.clone(), None);
        });
        let params = RuntimeGroupsParams {
            runtime_id: card.runtime_id.clone(),
            capability: card.capability.clone(),
            workspace: key.2.clone(),
        };
        spawn_local(async move {
            let result = client
                .runtime_groups(params)
                .await
                .map_err(|error| crate::errors::describe(&error));
            self.runtime_groups.update(|groups| {
                groups.insert(key, Some(result));
            });
        });
    }

    /// What `card` offers at `workspace`, as last read; `None` before.
    pub fn card_offer(self, card: &Card, workspace: Option<&str>) -> Option<RuntimeGroupsResult> {
        let key = groups_key(card, workspace);
        self.runtime_groups.with(|groups| match groups.get(&key) {
            Some(Some(Ok(result))) => Some(result.clone()),
            _ => None,
        })
    }

    /// The tool group and skill choices of `project`'s defaults, as a new
    /// session of it starts with them.
    pub fn project_exposure(self, project: Option<&ProjectId>) -> ToolExposure {
        if !self.has_projects.get_untracked() {
            return ToolExposure::default();
        }
        untrack(|| self.project(project))
            .and_then(|info| info.defaults)
            .map(|defaults| from_project(&defaults.exposure))
            .unwrap_or_default()
    }

    /// Chooses `state` for the group `selector` in `set` (`None` drops the
    /// choice).
    pub fn choose_card_group(
        self,
        set: CardSet,
        selector: ToolGroupSelector,
        state: Option<ToolGroupState>,
        below: ToolGroupState,
    ) {
        set.exposure.update(|exposure| {
            *exposure = with_group(std::mem::take(exposure), selector, state, below)
        });
    }

    /// Chooses `state` for the skill `name` in `set` (`None` drops the
    /// choice).
    pub fn choose_card_skill(
        self,
        set: CardSet,
        name: String,
        state: Option<SkillState>,
        below: SkillState,
    ) {
        set.exposure.update(|exposure| {
            *exposure = with_skill(std::mem::take(exposure), &name, state, below)
        });
    }

    /// The new session's choices for `session.create`: none while they are
    /// its project's defaults, which the daemon copies itself.
    pub(super) fn draft_exposure_to_send(self) -> Option<ToolExposure> {
        let project = self
            .new_project
            .get_untracked()
            .filter(|_| self.has_projects.get_untracked());
        let copy = self.project_exposure(project.as_ref());
        let draft = self.draft_exposure.get_untracked();
        (draft != copy).then_some(draft)
    }
}
