//! Memory as clients reach it (`docs/site/specs/memory.md` section 10):
//! listing, reading, writing, and deleting memory files for the user, with
//! no session involved. A client's change never refreshes an open session's
//! snapshot (section 7.2).

use cookie_agent_protocol::{
    MemoryDeleteParams, MemoryIgnoredEntry, MemoryInfo, MemoryLayer, MemoryListParams,
    MemoryListResult, MemoryReadParams, MemoryReadResult, MemoryWriteParams, MemoryWriteResult,
};

use super::{Engine, EngineError};
use crate::memory::{
    MemoryError,
    file::MemoryRevision,
    limits::MemoryLimits,
    store::{ListScope, MemoryEntry, StoreError, WriteMode},
    url::{self, MemoryName, MemoryUrl},
};

/// Why a `memory.*` request failed.
#[derive(Debug, thiserror::Error)]
pub enum MemoryRequestError {
    /// The URL is not a memory URL.
    #[error("{reason}")]
    InvalidUrl { url: String, reason: String },
    /// A member of the request is malformed or contradicts another.
    #[error("{reason}")]
    InvalidParams { field: &'static str, reason: String },
    #[error("there is no memory at {url}")]
    NotFound { url: String },
    #[error("{url} already exists at revision {revision}")]
    Exists { url: String, revision: String },
    #[error("{url} changed since it was read; its current revision is {revision}")]
    RevisionMismatch { url: String, revision: String },
    #[error("{url} is not a valid memory: {reason}")]
    Invalid { url: String, reason: String },
    #[error("{reason}")]
    Refused { url: String, reason: String },
    /// Another process holds the memory lock.
    #[error("{0}")]
    Busy(String),
    /// Reading or writing the memory folder failed.
    #[error("{0}")]
    Store(String),
}

impl From<MemoryError> for MemoryRequestError {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::Contention(_) => Self::Busy(error.to_string()),
            error => Self::Store(error.to_string()),
        }
    }
}

impl From<StoreError> for MemoryRequestError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Memory(error) => error.into(),
            StoreError::NotFound { url } => Self::NotFound {
                url: url.to_string(),
            },
            StoreError::Exists { url, revision } => Self::Exists {
                url: url.to_string(),
                revision: revision.to_string(),
            },
            StoreError::RevisionMismatch { url, current } => Self::RevisionMismatch {
                url: url.to_string(),
                revision: current.to_string(),
            },
            StoreError::Invalid { url, reason } => Self::Invalid {
                url: url.to_string(),
                reason: reason.to_string(),
            },
            StoreError::TooLarge { ref url, .. } => Self::Refused {
                url: url.to_string(),
                reason: error.to_string(),
            },
            // Edits, seen revisions, and the similar-memory check are a
            // tool's; a client replaces whole files at a revision.
            error @ (StoreError::Refused(_)
            | StoreError::Occurrences { .. }
            | StoreError::Unseen { .. }
            | StoreError::Similar { .. }
            | StoreError::Identical { .. }) => Self::Store(error.to_string()),
        }
    }
}

fn parse_url(url: &str) -> Result<MemoryUrl, MemoryRequestError> {
    MemoryUrl::parse(url).map_err(|error| MemoryRequestError::InvalidUrl {
        url: url.to_owned(),
        reason: error.to_string(),
    })
}

fn parse_revision(revision: &str) -> Result<MemoryRevision, MemoryRequestError> {
    MemoryRevision::parse(revision).ok_or_else(|| MemoryRequestError::InvalidParams {
        field: "revision",
        reason: format!("`{revision}` is not a memory revision (16 lowercase hex digits)"),
    })
}

const fn layer_of(layer: url::MemoryLayer) -> MemoryLayer {
    match layer {
        url::MemoryLayer::User => MemoryLayer::User,
        url::MemoryLayer::Agent => MemoryLayer::Agent,
        url::MemoryLayer::Machine => MemoryLayer::Machine,
        url::MemoryLayer::Project => MemoryLayer::Project,
    }
}

/// The store's scope for `params`: at most one of `agent`, `machine`, and
/// `project`, each agreeing with `layer` when that is given too.
fn list_scope(params: MemoryListParams) -> Result<ListScope, MemoryRequestError> {
    let invalid = |field, reason: &str| MemoryRequestError::InvalidParams {
        field,
        reason: reason.to_owned(),
    };
    let mut scopes = Vec::new();
    if let Some(agent) = params.agent {
        scopes.push(("agent", url::MemoryLayer::Agent, ListScope::Agent(agent)));
    }
    if let Some(machine) = params.machine {
        scopes.push((
            "machine",
            url::MemoryLayer::Machine,
            ListScope::Machine(machine),
        ));
    }
    if let Some(project) = params.project {
        let name = MemoryName::new(project.as_str()).ok_or_else(|| {
            invalid(
                "project",
                &format!(
                    "`{project}` is not a project name (a lowercase letter or digit, then up to 63 lowercase letters, digits, or `-`)"
                ),
            )
        })?;
        scopes.push((
            "project",
            url::MemoryLayer::Project,
            ListScope::Project(name),
        ));
    }
    let layer = match params.layer {
        None => None,
        Some(MemoryLayer::User) => Some(url::MemoryLayer::User),
        Some(MemoryLayer::Agent) => Some(url::MemoryLayer::Agent),
        Some(MemoryLayer::Machine) => Some(url::MemoryLayer::Machine),
        Some(MemoryLayer::Project) => Some(url::MemoryLayer::Project),
        Some(MemoryLayer::Unknown) => {
            return Err(invalid("layer", "the layer is not one this daemon knows"));
        }
    };
    if scopes.len() > 1 {
        return Err(invalid(
            scopes[1].0,
            "give at most one of agent, machine, and project",
        ));
    }
    match (scopes.pop(), layer) {
        (Some((field, scope_layer, _)), Some(layer)) if scope_layer != layer => Err(invalid(
            field,
            &format!("{field} lists the {scope_layer} layer, not {layer}"),
        )),
        (Some((_, _, scope)), _) => Ok(scope),
        (None, Some(layer)) => Ok(ListScope::Layer(layer)),
        (None, None) => Ok(ListScope::All),
    }
}

impl Engine {
    /// Runs a store call on a blocking thread: it reads files and may wait
    /// for the memory lock.
    async fn memory_blocking<T: Send + 'static>(
        &self,
        call: impl FnOnce(Engine) -> Result<T, MemoryRequestError> + Send + 'static,
    ) -> Result<T, EngineError> {
        let engine = self.clone();
        tokio::task::spawn_blocking(move || call(engine))
            .await
            .map_err(|error| {
                MemoryRequestError::Store(format!("the request did not finish: {error}"))
            })?
            .map_err(EngineError::from)
    }

    /// `memory.list`: the memory files `params` asks for, valid or not,
    /// with the agent files whose agent the registry no longer has marked.
    pub async fn list_memory(
        &self,
        params: MemoryListParams,
    ) -> Result<MemoryListResult, EngineError> {
        let scope = list_scope(params)?;
        self.memory_blocking(move |engine| {
            let limits = engine.memory_limits();
            let listing = engine.memory_root().list(&scope)?;
            Ok(MemoryListResult {
                memories: listing
                    .entries
                    .into_iter()
                    .map(|entry| memory_info(&engine, entry, &limits))
                    .collect(),
                ignored: listing
                    .ignored
                    .into_iter()
                    .map(|ignored| MemoryIgnoredEntry {
                        path: ignored.path,
                        reason: ignored.reason.to_owned(),
                    })
                    .collect(),
            })
        })
        .await
    }

    /// `memory.read`: one memory's whole text and revision.
    pub async fn read_memory(
        &self,
        params: MemoryReadParams,
    ) -> Result<MemoryReadResult, EngineError> {
        let url = parse_url(&params.url)?;
        self.memory_blocking(move |engine| {
            let limits = engine.memory_limits();
            let read = engine.memory_root().read(&url)?;
            Ok(MemoryReadResult {
                text: read.text,
                revision: read.entry.revision.as_str().to_owned(),
                invalid: read.entry.document.err().map(|reason| reason.to_string()),
                created: Some(read.entry.created),
                modified: Some(read.entry.modified),
                size: Some(read.entry.size),
                max_size: Some(limits.max_bytes(&url) as u64),
                url: url.to_string(),
            })
        })
        .await
    }

    /// `memory.write`: creates or replaces one memory from its whole text,
    /// for the user. The user may create a `user://` category; an
    /// `agent://` file still needs an agent the registry has, and keeps the
    /// description and keywords the daemon owns, which the result's
    /// `notice` reports (section 16.7). A `project://` file needs a project
    /// that exists (`docs/site/specs/projects.md` section 3).
    pub async fn write_memory(
        &self,
        params: MemoryWriteParams,
    ) -> Result<MemoryWriteResult, EngineError> {
        let url = parse_url(&params.url)?;
        let revision = params.revision.as_deref().map(parse_revision).transpose()?;
        let text = params.text;
        self.memory_blocking(move |engine| {
            let limits = engine.memory_limits();
            let written = engine
                .memory_root()
                .write(
                    &url,
                    &text,
                    WriteMode::Client(revision.as_ref()),
                    super::project_api::EngineMemoryOwners(&engine),
                    &limits,
                    jiff::Timestamp::now(),
                )
                .map_err(|error| match error {
                    StoreError::Refused(reason) => MemoryRequestError::Refused {
                        url: url.to_string(),
                        reason,
                    },
                    error => error.into(),
                })?;
            Ok(MemoryWriteResult {
                url: url.to_string(),
                revision: written.revision.as_str().to_owned(),
                notice: written.kept.as_ref().map(|kept| kept.describe(&url)),
            })
        })
        .await
    }

    /// `memory.delete`: deletes one memory at `revision`, for the user, who
    /// may delete `user://` and `agent://` files too.
    pub async fn delete_memory(&self, params: MemoryDeleteParams) -> Result<(), EngineError> {
        let url = parse_url(&params.url)?;
        let revision = parse_revision(&params.revision)?;
        self.memory_blocking(move |engine| {
            engine
                .memory_root()
                .delete(&url, Some(&revision))
                .map(drop)
                .map_err(|error| match error {
                    StoreError::Refused(reason) => MemoryRequestError::Refused {
                        url: url.to_string(),
                        reason,
                    },
                    error => error.into(),
                })
        })
        .await
    }

    /// Signals every memory file this daemon writes, edits, or deletes,
    /// for clients and model tools alike.
    #[must_use]
    pub fn subscribe_memory_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.memory_root().subscribe()
    }
}

fn memory_info(engine: &Engine, entry: MemoryEntry, limits: &MemoryLimits) -> MemoryInfo {
    let orphaned = match &entry.url {
        MemoryUrl::Agent { agent } => !engine.agent_registered(agent),
        _ => false,
    };
    let (description, keywords, invalid) = match entry.document {
        Ok(document) => (
            Some(document.description().to_owned()),
            document.keywords().to_vec(),
            None,
        ),
        Err(reason) => (None, Vec::new(), Some(reason.to_string())),
    };
    MemoryInfo {
        created: Some(entry.created),
        modified: Some(entry.modified),
        last_read: engine.memory_root().last_read(&entry.url),
        max_size: Some(limits.max_bytes(&entry.url) as u64),
        layer: layer_of(entry.url.layer()),
        group: entry.url.group().map(str::to_owned),
        url: entry.url.to_string(),
        revision: entry.revision.as_str().to_owned(),
        size: entry.size,
        description,
        keywords,
        invalid,
        orphaned,
    }
}

#[cfg(test)]
mod tests;
