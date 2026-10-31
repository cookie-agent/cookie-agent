//! Memory and projects as the engine serves them
//! (`docs/site/specs/memory.md`, `docs/site/specs/projects.md`): client
//! requests, what runs meet, the agent memory files, and projects. The
//! memory folder itself is `cookie_agent_engine_memory`.

pub(super) mod agent_files;
pub(crate) mod api;
pub(super) mod projects;
pub(crate) mod runs;
