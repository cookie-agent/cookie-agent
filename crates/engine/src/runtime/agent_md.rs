use cookie_agent_protocol::{AgentMdEntry, AgentMdProject, AgentMdSkipped, SessionId};

use super::{Engine, EngineError};

/// The `AGENTS.md` files one coding-runtime attachment returned, or the
/// session's project's `AGENTS.md`.
pub(crate) struct AttachmentAgentMd {
    pub(crate) attachment: cookie_agent_protocol::AttachmentName,
    pub(crate) entries: Vec<AgentMdEntry>,
    pub(crate) skipped: Vec<AgentMdSkipped>,
    /// The project whose `AGENTS.md` this is; `None` for a workspace's.
    pub(crate) project: Option<AgentMdProject>,
}

impl Engine {
    /// `agent_override` is the run agent's `agent_md` frontmatter; when present
    /// it replaces the global `[agent_md] enabled` switch. Each coding-runtime
    /// attachment of `session` applies the lookup to its own workspace: the
    /// preset's `.cookie-agent/agents/<preset>/AGENTS.md`, else
    /// `.cookie-agent/agents/AGENTS.md`, then `<workspace>/AGENTS.md`.
    /// The session's project's `AGENTS.md` comes first, before every
    /// workspace's (`docs/site/specs/projects.md` section 6): general
    /// before specific. It needs no runtime, not even `global`.
    pub(crate) async fn load_agent_md(
        &self,
        session: SessionId,
        preset: Option<&str>,
        agent_override: Option<bool>,
    ) -> Result<Vec<AttachmentAgentMd>, EngineError> {
        if !agent_override.unwrap_or(self.inner.config.load().runtime.agent_md.enabled) {
            return Ok(Vec::new());
        }
        let mut loaded = Vec::new();
        if let Some(project) = self.project_agent_md(session).await {
            loaded.push(project);
        }
        for attachment in self.session_runtimes(session)? {
            // `AGENTS.md` lives in a workspace; runtimes without one have none.
            if attachment.workspace.is_none() {
                continue;
            }
            let Some(instance) = self.inner.runtimes.instance(&attachment) else {
                continue;
            };
            let Some(context) = instance.agent_md(preset).await? else {
                continue;
            };
            if !context.entries.is_empty() || !context.skipped.is_empty() {
                loaded.push(AttachmentAgentMd {
                    attachment: attachment.name,
                    entries: context.entries,
                    skipped: context.skipped,
                    project: None,
                });
            }
        }
        Ok(loaded)
    }
}
