//! Session directories, projections, and rebuildable metadata caches.

use std::{
    collections::HashMap,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use cookiecode_config::PolicySnapshot;
use cookiecode_protocol::{
    ChildSummary, Event, RunId, SessionId, SessionMeta, SessionOrigin, SessionStatus, SessionTree,
    ToolCallId, Usage,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::events::{EventLog, EventLogError, fsync_directory};

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Event(#[from] EventLogError),
    #[error("session IO failure at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid session metadata at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("session {0} not found")]
    Missing(SessionId),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunProjection {
    pub id: RunId,
    pub client_run_id: String,
    pub input: String,
    pub status: SessionStatus,
    pub final_text: Option<String>,
    pub pending_calls: HashMap<ToolCallId, String>,
}

#[derive(Clone, Debug)]
pub struct SessionProjection {
    pub meta: SessionMeta,
    pub policy: PolicySnapshot,
    pub status: SessionStatus,
    pub usage: Option<Usage>,
    pub runs: HashMap<RunId, RunProjection>,
    pub log: Arc<EventLog>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MetaCache {
    meta: SessionMeta,
    status: SessionStatus,
    usage: Option<Usage>,
}

#[derive(Debug)]
pub struct SessionStore {
    project_dir: PathBuf,
    sessions_dir: PathBuf,
    sessions: Mutex<HashMap<SessionId, SessionProjection>>,
    creation: Mutex<()>,
}

impl SessionStore {
    pub fn project_dir(data_root: &Path, cwd: &Path) -> PathBuf {
        let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        canonical.to_string_lossy().hash(&mut hash);
        data_root
            .join("projects")
            .join(format!("{:016x}", hash.finish()))
    }

    pub fn open(data_root: &Path, cwd: &Path) -> Result<Arc<Self>, SessionError> {
        let project_dir = Self::project_dir(data_root, cwd);
        let sessions_dir = project_dir.join("sessions");
        fs::create_dir_all(&sessions_dir).map_err(|source| SessionError::Io {
            path: sessions_dir.clone(),
            source,
        })?;
        let store = Arc::new(Self {
            project_dir,
            sessions_dir: sessions_dir.clone(),
            sessions: Mutex::new(HashMap::new()),
            creation: Mutex::new(()),
        });
        for entry in fs::read_dir(&sessions_dir).map_err(|source| SessionError::Io {
            path: sessions_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| SessionError::Io {
                path: sessions_dir.clone(),
                source,
            })?;
            if !entry
                .file_type()
                .map_err(|source| SessionError::Io {
                    path: entry.path(),
                    source,
                })?
                .is_dir()
            {
                continue;
            }
            let Ok(id) = entry.file_name().to_string_lossy().parse::<SessionId>() else {
                continue;
            };
            let log = EventLog::open(entry.path().join("events.jsonl"), id)?;
            let projection = projection(log)?;
            store
                .sessions
                .lock()
                .expect("session store lock poisoned")
                .insert(id, projection);
        }
        Ok(store)
    }

    pub fn create(
        &self,
        meta: SessionMeta,
        policy: PolicySnapshot,
    ) -> Result<Arc<EventLog>, SessionError> {
        self.create_with_status(meta, policy).map(|(log, _)| log)
    }

    /// Creates a session atomically and reports whether this caller won creation.
    pub fn create_with_status(
        &self,
        meta: SessionMeta,
        policy: PolicySnapshot,
    ) -> Result<(Arc<EventLog>, bool), SessionError> {
        let _creation = self
            .creation
            .lock()
            .expect("session creation lock poisoned");
        let final_dir = self.sessions_dir.join(meta.id.to_string());
        if final_dir.exists() {
            return Ok((self.get(meta.id)?.log, false));
        }
        let temporary = self
            .sessions_dir
            .join(format!(".{}.{}.tmp", meta.id, SessionId::new_v7()));
        fs::create_dir_all(&temporary).map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        let _log = EventLog::create(
            temporary.join("events.jsonl"),
            meta.id,
            Event::SessionCreated { meta: meta.clone() },
            policy.clone(),
        )?;
        write_cache(
            &temporary.join("meta.json"),
            &MetaCache {
                meta: meta.clone(),
                status: SessionStatus::Idle,
                usage: None,
            },
        )?;
        fsync_directory(&temporary)?;
        if let Err(source) = fs::rename(&temporary, &final_dir) {
            if source.kind() == std::io::ErrorKind::AlreadyExists || final_dir.exists() {
                let _ = fs::remove_dir_all(&temporary);
                return Ok((self.get(meta.id)?.log, false));
            }
            return Err(SessionError::Io {
                path: final_dir.clone(),
                source,
            });
        }
        fsync_directory(&self.sessions_dir)?;
        let final_log = EventLog::open(final_dir.join("events.jsonl"), meta.id)?;
        let result = projection(final_log.clone())?;
        self.sessions
            .lock()
            .expect("session store lock poisoned")
            .insert(meta.id, result);
        Ok((final_log, true))
    }

    pub fn get(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        self.sessions
            .lock()
            .expect("session store lock poisoned")
            .get(&id)
            .cloned()
            .ok_or(SessionError::Missing(id))
    }

    pub fn update(&self, id: SessionId) -> Result<(), SessionError> {
        let current = self.get(id)?;
        let rebuilt = projection(current.log.clone())?;
        write_cache(
            &self.sessions_dir.join(id.to_string()).join("meta.json"),
            &MetaCache {
                meta: rebuilt.meta.clone(),
                status: rebuilt.status,
                usage: rebuilt.usage.clone(),
            },
        )?;
        self.sessions
            .lock()
            .expect("session store lock poisoned")
            .insert(id, rebuilt);
        Ok(())
    }

    #[must_use]
    pub fn all(&self) -> Vec<SessionProjection> {
        self.sessions
            .lock()
            .expect("session store lock poisoned")
            .values()
            .cloned()
            .collect()
    }
    #[must_use]
    pub fn project_dir_path(&self) -> &Path {
        &self.project_dir
    }
    #[must_use]
    pub fn session_dir(&self, id: SessionId) -> PathBuf {
        self.sessions_dir.join(id.to_string())
    }

    pub fn children(&self, parent: SessionId) -> Vec<ChildSummary> {
        self.all()
            .into_iter()
            .filter_map(|child| match child.meta.origin {
                SessionOrigin::Delegated {
                    parent_session_id, ..
                } if parent_session_id == parent => Some(ChildSummary {
                    id: child.meta.id,
                    profile: child.meta.profile.name.clone(),
                    task_excerpt: child
                        .runs
                        .values()
                        .min_by_key(|run| run.id.to_string())
                        .map(|run| run.input.chars().take(160).collect()),
                    status: child.status,
                    usage: child.usage,
                }),
                _ => None,
            })
            .collect()
    }

    pub fn tree(&self, id: SessionId) -> Result<SessionTree, SessionError> {
        let session = self.get(id)?.meta;
        Ok(SessionTree {
            session,
            children: self
                .children(id)
                .into_iter()
                .map(|child| self.tree(child.id))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

fn projection(log: Arc<EventLog>) -> Result<SessionProjection, SessionError> {
    let policy = log
        .policy()
        .ok_or_else(|| EventLogError::MissingCreation(log.path().to_owned()))?;
    let events = log.events();
    let meta = match &events.first().expect("creation checked by EventLog").event {
        Event::SessionCreated { meta } => meta.clone(),
        _ => unreachable!(),
    };
    let mut runs = HashMap::new();
    let mut status = SessionStatus::Idle;
    let mut usage = None;
    for envelope in &events {
        let Some(run_id) = envelope.run_id else {
            continue;
        };
        match &envelope.event {
            Event::RunStarted {
                client_run_id,
                input,
            } => {
                status = SessionStatus::Running;
                runs.insert(
                    run_id,
                    RunProjection {
                        id: run_id,
                        client_run_id: client_run_id.clone(),
                        input: input.clone(),
                        status: SessionStatus::Running,
                        final_text: None,
                        pending_calls: HashMap::new(),
                    },
                );
            }
            // User input is prompt history, not a lifecycle transition.
            Event::UserInputSubmitted { .. } | Event::UserInputApplied { .. } => {}
            Event::RunCompleted { final_text } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Completed;
                    run.final_text = final_text.clone();
                    status = SessionStatus::Completed;
                }
            }
            Event::RunFailed { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Failed;
                    status = SessionStatus::Failed;
                }
            }
            Event::RunCancelled { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Cancelled;
                    status = SessionStatus::Cancelled;
                }
            }
            Event::RunInterrupted { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Interrupted;
                    status = SessionStatus::Interrupted;
                }
            }
            Event::ToolCallStarted {
                tool_call_id, tool, ..
            } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.pending_calls.insert(*tool_call_id, tool.clone());
                }
            }
            Event::ToolCallCompleted { tool_call_id, .. }
            | Event::ToolCallFailed { tool_call_id, .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.pending_calls.remove(tool_call_id);
                }
            }
            Event::UsageReported {
                usage: reported, ..
            } => {
                let total = usage.get_or_insert_with(Usage::default);
                total.input_tokens = total.input_tokens.saturating_add(reported.input_tokens);
                total.output_tokens = total.output_tokens.saturating_add(reported.output_tokens);
                total.cached_input_tokens =
                    match (total.cached_input_tokens, reported.cached_input_tokens) {
                        (Some(current), Some(next)) => Some(current.saturating_add(next)),
                        (None, Some(next)) => Some(next),
                        (current, None) => current,
                    };
            }
            _ => {}
        }
    }
    Ok(SessionProjection {
        meta,
        policy,
        status,
        usage,
        runs,
        log,
    })
}

fn write_cache(path: &Path, cache: &MetaCache) -> Result<(), SessionError> {
    let bytes = serde_json::to_vec_pretty(cache).map_err(|source| SessionError::Json {
        path: path.to_owned(),
        source,
    })?;
    fs::write(path, bytes).map_err(|source| SessionError::Io {
        path: path.to_owned(),
        source,
    })?;
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| SessionError::Io {
            path: path.to_owned(),
            source,
        })
}
