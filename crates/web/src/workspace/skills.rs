//! `/skills`: the session's skills (`skills.list`), searched, and run with
//! their arguments: `skills.get` checks the skill, then the encoded skill
//! submission is sent as the TUI sends it.

use cookie_agent_protocol::{InputDelivery, SkillsGetParams, SkillsListParams};
use leptos::{prelude::*, task::spawn_local};

use super::{Panel, Workspace};

impl Workspace {
    /// Opens `/skills`, searching for `query`.
    pub(super) fn open_skills(self, query: String) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            self.status
                .set("select a session before running a skill".into());
            return;
        };
        self.skill_query.set(query);
        self.skill_pick.set(None);
        self.skill_args.set(String::new());
        self.skills.set(None);
        self.panel.set(Some(Panel::Skills));
        spawn_local(async move {
            let result = client
                .list_skills(SkillsListParams { session_id })
                .await
                .map(|result| result.skills)
                .map_err(|error| error.to_string());
            self.skills.set(Some(result));
        });
    }

    /// Runs the picked skill with the typed arguments.
    pub fn run_skill(self) {
        let (Some(client), Some(session_id), Some(skill)) = (
            self.client(),
            self.selected.get_untracked(),
            self.skill_pick.get_untracked(),
        ) else {
            return;
        };
        if !self.writable() {
            self.status
                .set("cannot run a skill in a read-only session".into());
            return;
        }
        let name = skill.name;
        let args = self.skill_args.get_untracked();
        self.panel.set(None);
        spawn_local(async move {
            match client
                .get_skill(SkillsGetParams {
                    session_id,
                    name: name.clone(),
                    args: args.clone(),
                })
                .await
            {
                Ok(result) if result.skill.user_invocable => {
                    let text = cookie_agent_protocol::encode_skill_submission(&name, &args);
                    let content = cookie_agent_client_state::ComposerInput::text(text.clone())
                        .content()
                        .expect("a skill submission is text");
                    self.send_content(content, InputDelivery::Steer, String::new(), Vec::new());
                }
                Ok(_) => self.status.set("skill is not user-invocable".into()),
                Err(error) => self.status.set(format!("load skill failed: {error}")),
            }
        });
    }
}
