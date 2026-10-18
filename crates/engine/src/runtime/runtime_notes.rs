//! Where a runtime's tools run, told in their descriptions. With one
//! runtime besides `global`, there is nothing to tell apart and tool
//! descriptions stay as their runtime wrote them; with several, each of
//! their tools says which runtime it runs in and where its relative paths
//! resolve.

use std::collections::HashMap;

use cookie_agent_protocol::{AttachmentName, RuntimeId, SessionId};

use crate::Engine;

impl Engine {
    /// The note each attachment's tools carry in their descriptions; empty
    /// unless `session` attaches more than one runtime besides `global`.
    pub(crate) fn runtime_tool_notes(&self, session: SessionId) -> HashMap<AttachmentName, String> {
        let Ok(attachments) = self.session_runtimes(session) else {
            return HashMap::new();
        };
        let attachments = attachments
            .into_iter()
            .filter(|attachment| attachment.runtime_id.as_str() != RuntimeId::GLOBAL)
            .collect::<Vec<_>>();
        if attachments.len() < 2 {
            return HashMap::new();
        }
        attachments
            .into_iter()
            .map(|attachment| {
                let platform = self
                    .inner
                    .runtimes
                    .instance(&attachment)
                    .and_then(|instance| {
                        instance
                            .info()
                            .workspace()
                            .and_then(|workspace| workspace.platform().map(str::to_owned))
                    });
                let note = runtime_note(
                    attachment.name.as_str(),
                    attachment.workspace.as_deref(),
                    platform.as_deref(),
                );
                (attachment.name, note)
            })
            .collect()
    }
}

/// The note for one attachment's tools.
pub(crate) fn runtime_note(name: &str, workspace: Option<&str>, platform: Option<&str>) -> String {
    let on = platform
        .map(|platform| format!(" on {platform}"))
        .unwrap_or_default();
    match workspace {
        Some(workspace) => format!(
            "Runs in runtime `{name}`{on}, in the workspace `{workspace}`; relative paths resolve against it."
        ),
        None => format!("Runs in runtime `{name}`{on}."),
    }
}

#[cfg(test)]
mod tests {
    use super::runtime_note;

    #[test]
    fn a_note_names_the_runtime_and_where_its_paths_resolve() {
        assert_eq!(
            runtime_note("labpc_api", Some("/home/me/api"), Some("linux")),
            "Runs in runtime `labpc_api` on linux, in the workspace `/home/me/api`; relative paths resolve against it."
        );
        assert_eq!(
            runtime_note("desktop", None, None),
            "Runs in runtime `desktop`."
        );
    }
}
