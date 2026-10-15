use std::{fs::File, io::Read as _, path::Path};

use cookie_agent_protocol::{AgentMdEntry, AgentMdSkipped, SafeDisplayText, SessionId};

use super::{Engine, EngineError};

/// Files larger than this are skipped entirely and surfaced as an
/// `AgentMdSkipped` warning event, mirroring Claude Code behavior.
pub(crate) const AGENT_MD_MAX_BYTES: u64 = AgentMdEntry::MAX_CONTENT_BYTES as u64;

/// The `AGENTS.md` files one coding-runtime attachment returned.
pub(crate) struct AttachmentAgentMd {
    pub(crate) attachment: cookie_agent_protocol::AttachmentName,
    pub(crate) entries: Vec<AgentMdEntry>,
    pub(crate) skipped: Vec<AgentMdSkipped>,
}

impl Engine {
    /// `agent_override` is the run agent's `agent_md` frontmatter; when present
    /// it replaces the global `[agent_md] enabled` switch. Each coding-runtime
    /// attachment of `session` applies the lookup to its own workspace: the
    /// preset's `.cookie-agent/agents/<preset>/AGENTS.md`, else
    /// `.cookie-agent/agents/AGENTS.md`, then `<workspace>/AGENTS.md`.
    pub(crate) fn load_agent_md(
        &self,
        session: SessionId,
        preset: Option<&str>,
        agent_override: Option<bool>,
    ) -> Result<Vec<AttachmentAgentMd>, EngineError> {
        if !agent_override.unwrap_or(self.inner.config.runtime.agent_md.enabled) {
            return Ok(Vec::new());
        }
        let mut loaded = Vec::new();
        for attachment in self.session_runtimes(session)? {
            if attachment.kind != cookie_agent_protocol::RuntimeKind::Coding {
                continue;
            }
            let Some(instance) = self.inner.runtimes.instance(&attachment) else {
                continue;
            };
            let Some(workspace) = instance.info().workspace() else {
                continue;
            };
            let (entries, skipped) = self.load_workspace_agent_md(workspace.root(), preset)?;
            if !entries.is_empty() || !skipped.is_empty() {
                loaded.push(AttachmentAgentMd {
                    attachment: attachment.name,
                    entries,
                    skipped,
                });
            }
        }
        Ok(loaded)
    }

    fn load_workspace_agent_md(
        &self,
        workspace: &Path,
        preset: Option<&str>,
    ) -> Result<(Vec<AgentMdEntry>, Vec<AgentMdSkipped>), EngineError> {
        let agents_dir = workspace.join(".cookie-agent").join("agents");
        let mut entries = Vec::with_capacity(2);
        let mut skipped = Vec::new();
        let project_path = if let Some(preset) = preset {
            let preset_path = agents_dir.join(preset).join("AGENTS.md");
            if preset_path.is_file() {
                preset_path
            } else {
                agents_dir.join("AGENTS.md")
            }
        } else {
            agents_dir.join("AGENTS.md")
        };
        if let Some(entry) = self.read_agent_md_file(&project_path, workspace)? {
            entries.push(entry);
        } else if let Some(skip) = skipped_agent_md_file(&project_path) {
            skipped.push(skip);
        }
        let workspace_path = workspace.join("AGENTS.md");
        if let Some(entry) = self.read_agent_md_file(&workspace_path, workspace)? {
            entries.push(entry);
        } else if let Some(skip) = skipped_agent_md_file(&workspace_path) {
            skipped.push(skip);
        }
        Ok((entries, skipped))
    }

    fn read_agent_md_file(
        &self,
        path: &Path,
        workspace: &Path,
    ) -> Result<Option<AgentMdEntry>, EngineError> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(EngineError::AgentMdIo {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let metadata_len = file
            .metadata()
            .map_err(|source| EngineError::AgentMdIo {
                path: path.to_owned(),
                source,
            })?
            .len();
        if metadata_len > AGENT_MD_MAX_BYTES {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(metadata_len as usize);
        file.read_to_end(&mut bytes)
            .map_err(|source| EngineError::AgentMdIo {
                path: path.to_owned(),
                source,
            })?;
        let final_metadata_len = file
            .metadata()
            .map_err(|source| EngineError::AgentMdIo {
                path: path.to_owned(),
                source,
            })?
            .len();
        let observed_bytes = metadata_len.max(final_metadata_len).max(bytes.len() as u64);
        if observed_bytes > AGENT_MD_MAX_BYTES {
            return Ok(None);
        }
        let content = match std::str::from_utf8(&bytes) {
            Ok(content) => content.to_owned(),
            Err(error) => {
                return Err(EngineError::AgentMdIo {
                    path: path.to_owned(),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                });
            }
        };
        Ok(Some(AgentMdEntry {
            source: SafeDisplayText::new(relative_display_path(path, workspace))
                .expect("AGENTS.md relative path is bounded"),
            byte_length: observed_bytes,
            content,
        }))
    }
}

pub(crate) fn skipped_agent_md_file(path: &Path) -> Option<AgentMdSkipped> {
    let byte_length = std::fs::metadata(path).ok()?.len();
    (byte_length > AGENT_MD_MAX_BYTES).then(|| AgentMdSkipped {
        path: SafeDisplayText::new(path.to_string_lossy().into_owned())
            .expect("AGENTS.md absolute path is bounded"),
        byte_length,
    })
}

/// Display path relative to the runtime's workspace when possible, so
/// model-facing context (e.g. AGENTS.md `from=` attributes) reinforces
/// workspace-relative path style.
fn relative_display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).map_or_else(
        |_| path.to_string_lossy().into_owned(),
        |rel| rel.display().to_string(),
    )
}
