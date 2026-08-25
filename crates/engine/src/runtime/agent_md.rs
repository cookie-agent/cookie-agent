use std::{fs::File, io::Read as _, path::Path};

use cookie_agent_protocol::{AgentMdEntry, SafeDisplayText};

use super::{Engine, EngineError};

impl Engine {
    pub(crate) fn load_agent_md(
        &self,
        preset: Option<&str>,
    ) -> Result<Vec<AgentMdEntry>, EngineError> {
        let config = &self.inner.config.runtime.agent_md;
        if !config.enabled {
            return Ok(Vec::new());
        }
        let cwd = self.inner.store.cwd();
        let agents_dir = cwd.join(".cookie-agent").join("agents");
        let mut entries = Vec::with_capacity(2);
        let project_entry = if let Some(preset) = preset {
            let source = format!(".cookie-agent/agents/{preset}/AGENTS.md");
            match self.read_agent_md_file(
                &agents_dir.join(preset).join("AGENTS.md"),
                &source,
                config.max_bytes,
            )? {
                Some(entry) => Some(entry),
                None => self.read_agent_md_file(
                    &agents_dir.join("AGENTS.md"),
                    ".cookie-agent/agents/AGENTS.md",
                    config.max_bytes,
                )?,
            }
        } else {
            self.read_agent_md_file(
                &agents_dir.join("AGENTS.md"),
                ".cookie-agent/agents/AGENTS.md",
                config.max_bytes,
            )?
        };
        if let Some(entry) = project_entry {
            entries.push(entry);
        }
        if let Some(entry) =
            self.read_agent_md_file(&cwd.join("AGENTS.md"), "AGENTS.md", config.max_bytes)?
        {
            entries.push(entry);
        }
        Ok(entries)
    }

    fn read_agent_md_file(
        &self,
        path: &Path,
        source: &str,
        max_bytes: usize,
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
        let mut bytes = Vec::with_capacity(max_bytes.saturating_add(1));
        (&mut file)
            .take(max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
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
        let prefix_len = max_bytes.min(bytes.len());
        let content = match std::str::from_utf8(&bytes[..prefix_len]) {
            Ok(content) => content.to_owned(),
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()])
                    .expect("valid_up_to is valid UTF-8")
                    .to_owned()
            }
            Err(error) => {
                return Err(EngineError::AgentMdIo {
                    path: path.to_owned(),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidData, error),
                });
            }
        };
        let original_bytes = observed_bytes.max(content.len() as u64);
        Ok(Some(AgentMdEntry {
            source: SafeDisplayText::new(source).expect("AGENTS.md context source is bounded"),
            truncated: original_bytes > content.len() as u64,
            original_bytes,
            content,
        }))
    }
}
