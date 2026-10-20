//! Workspace context a coding runtime contributes at run admission: the
//! `AGENTS.md` lookup of `docs/site/specs/runtimes.md` section 8.
//!
//! The coding runtime runs it on its own machine and answers
//! `runtime/context` with the result; the daemon runs the same lookup for an
//! in-process coding runtime.

use std::{
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
};

use cookie_agent_protocol::{AgentMdEntry, AgentMdSkipped, SafeDisplayText};

/// Largest `AGENTS.md` file the lookup reads; larger files are skipped and
/// reported.
pub const AGENT_MD_MAX_BYTES: u64 = AgentMdEntry::MAX_CONTENT_BYTES as u64;

/// The `AGENTS.md` files found in one workspace.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentMdContext {
    /// Files read, in prompt order.
    pub entries: Vec<AgentMdEntry>,
    /// Files skipped because they exceed the size limit.
    pub skipped: Vec<AgentMdSkipped>,
}

/// An `AGENTS.md` file that exists but could not be read.
#[derive(Debug, thiserror::Error)]
#[error("AGENTS.md context read failed at {}: {source}", path.display())]
pub struct AgentMdError {
    pub path: PathBuf,
    #[source]
    pub source: std::io::Error,
}

/// Applies the `AGENTS.md` lookup to `workspace`: the preset's
/// `.cookie-agent/agents/<preset>/AGENTS.md`, else
/// `.cookie-agent/agents/AGENTS.md`, then `<workspace>/AGENTS.md`. Files over
/// `max_bytes` (at most [`AGENT_MD_MAX_BYTES`]) are skipped.
///
/// # Errors
///
/// Fails when a file exists but cannot be read or is not UTF-8.
pub fn load_agent_md(
    workspace: &Path,
    preset: Option<&str>,
    max_bytes: u64,
) -> Result<AgentMdContext, AgentMdError> {
    let max_bytes = max_bytes.min(AGENT_MD_MAX_BYTES);
    let agents_dir = workspace.join(".cookie-agent").join("agents");
    let project_path = match preset {
        Some(preset) => {
            let preset_path = agents_dir.join(preset).join("AGENTS.md");
            if preset_path.is_file() {
                preset_path
            } else {
                agents_dir.join("AGENTS.md")
            }
        }
        None => agents_dir.join("AGENTS.md"),
    };
    let mut context = AgentMdContext::default();
    for path in [project_path, workspace.join("AGENTS.md")] {
        if let Some(entry) = read_agent_md_file(&path, workspace, max_bytes)? {
            context.entries.push(entry);
        } else if let Some(skip) = skipped_agent_md_file(&path, max_bytes) {
            context.skipped.push(skip);
        }
    }
    Ok(context)
}

fn read_agent_md_file(
    path: &Path,
    workspace: &Path,
    max_bytes: u64,
) -> Result<Option<AgentMdEntry>, AgentMdError> {
    let io_error = |source| AgentMdError {
        path: path.to_owned(),
        source,
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(source)),
    };
    let metadata_len = file.metadata().map_err(io_error)?.len();
    if metadata_len > max_bytes {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(metadata_len as usize);
    file.read_to_end(&mut bytes).map_err(io_error)?;
    let final_metadata_len = file.metadata().map_err(io_error)?.len();
    let observed_bytes = metadata_len.max(final_metadata_len).max(bytes.len() as u64);
    if observed_bytes > max_bytes {
        return Ok(None);
    }
    let content = String::from_utf8(bytes).map_err(|error| {
        io_error(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.utf8_error(),
        ))
    })?;
    Ok(Some(AgentMdEntry {
        source: SafeDisplayText::new(relative_display_path(path, workspace))
            .expect("AGENTS.md relative path is bounded"),
        byte_length: observed_bytes,
        content,
    }))
}

fn skipped_agent_md_file(path: &Path, max_bytes: u64) -> Option<AgentMdSkipped> {
    let byte_length = std::fs::metadata(path).ok()?.len();
    (byte_length > max_bytes).then(|| AgentMdSkipped {
        path: SafeDisplayText::new(path.to_string_lossy().into_owned())
            .expect("AGENTS.md absolute path is bounded"),
        byte_length,
    })
}

/// Display path relative to the runtime's workspace when possible, so
/// model-facing context (e.g. AGENTS.md `from=` attributes) reinforces
/// workspace-relative path style.
/// How an AGENTS.md file is named: relative to the workspace with `/`
/// between components on every platform, as the model reads paths in the
/// workspace; else its absolute path as the platform spells it.
fn relative_display_path(path: &Path, workspace: &Path) -> String {
    path.strip_prefix(workspace).map_or_else(
        |_| path.to_string_lossy().into_owned(),
        |relative| {
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        },
    )
}
