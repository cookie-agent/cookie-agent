use std::{fs::File, io::Read as _, path::Path};

use cookie_agent_protocol::{AgentMdEntry, AgentMdSkipped, SafeDisplayText};

use super::{Engine, EngineError};

/// Files larger than this are skipped entirely and surfaced as an
/// `AgentMdSkipped` warning event, mirroring Claude Code behavior.
pub(crate) const AGENT_MD_MAX_BYTES: u64 = AgentMdEntry::MAX_CONTENT_BYTES as u64;

impl Engine {
    /// `agent_override` is the run agent's `agent_md` frontmatter; when present
    /// it replaces the global `[agent_md] enabled` switch.
    pub(crate) fn load_agent_md(
        &self,
        preset: Option<&str>,
        agent_override: Option<bool>,
    ) -> Result<(Vec<AgentMdEntry>, Vec<AgentMdSkipped>), EngineError> {
        if !agent_override.unwrap_or(self.inner.config.runtime.agent_md.enabled) {
            return Ok((Vec::new(), Vec::new()));
        }
        let cwd = self.inner.store.cwd();
        let agents_dir = cwd.join(".cookie-agent").join("agents");
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
        if let Some(entry) = self.read_agent_md_file(&project_path)? {
            entries.push(entry);
        } else if let Some(skip) = skipped_agent_md_file(&project_path) {
            skipped.push(skip);
        }
        let cwd_path = cwd.join("AGENTS.md");
        if let Some(entry) = self.read_agent_md_file(&cwd_path)? {
            entries.push(entry);
        } else if let Some(skip) = skipped_agent_md_file(&cwd_path) {
            skipped.push(skip);
        }
        Ok((entries, skipped))
    }

    fn read_agent_md_file(&self, path: &Path) -> Result<Option<AgentMdEntry>, EngineError> {
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
            source: SafeDisplayText::new(path.to_string_lossy().into_owned())
                .expect("AGENTS.md absolute path is bounded"),
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
