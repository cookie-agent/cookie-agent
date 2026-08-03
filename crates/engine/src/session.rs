//! Session directories, projections, and rebuildable metadata caches.

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use cookie_agent_config::PolicySnapshot;
use cookie_agent_protocol::{
    ChildSummary, Event, ProfileIdentity, ProfileSnapshot, RunId, SessionId, SessionMeta,
    SessionOrigin, SessionRenameRecord, SessionStatus, SessionTitleCommit, SessionTree, ToolCallId,
    Usage,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::events::{EventLog, EventLogError, fsync_directory};

const PROJECT_CWD_FILE: &str = "cwd";

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
    pub profile: ProfileSnapshot,
    pub current_profile: ProfileIdentity,
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
    pub rename_records: HashMap<cookie_agent_protocol::ClientRenameId, SessionRenameRecord>,
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
        write_project_cwd(&project_dir, cwd)?;
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
                .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(meta.id, result);
        Ok((final_log, true))
    }

    pub fn get(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, rebuilt);
        Ok(())
    }

    #[must_use]
    pub fn all(&self) -> Vec<SessionProjection> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    let mut meta = match &events.first().expect("creation checked by EventLog").event {
        Event::SessionCreated { meta } => meta.clone(),
        _ => unreachable!(),
    };
    let mut runs = HashMap::new();
    let mut status = SessionStatus::Idle;
    let mut usage = None;
    let mut rename_records = HashMap::new();
    let mut automatic_title = meta.title.clone();
    let mut user_title: Option<Option<cookie_agent_protocol::SessionTitle>> = None;
    for envelope in &events {
        if let Event::SessionTitleCommitted { commit, .. } = &envelope.event {
            match commit {
                SessionTitleCommit::UserSet { title, .. } => {
                    user_title = Some(Some(title.clone()));
                }
                SessionTitleCommit::UserClear { .. } => user_title = Some(None),
                SessionTitleCommit::UserReset { .. } => user_title = None,
                SessionTitleCommit::InternalAgentSet { title, .. }
                | SessionTitleCommit::FallbackSet { title } => {
                    automatic_title = Some(title.clone());
                }
            }
            meta.title = user_title
                .clone()
                .unwrap_or_else(|| automatic_title.clone());
            if let Some(record) = commit.user_rename_record() {
                rename_records.insert(record.client_rename_id.clone(), record);
            }
        }
        let Some(run_id) = envelope.run_id else {
            continue;
        };
        match &envelope.event {
            Event::RunStarted {
                client_run_id,
                input,
                profile,
                current_profile,
            } => {
                status = SessionStatus::Running;
                runs.insert(
                    run_id,
                    RunProjection {
                        id: run_id,
                        client_run_id: client_run_id.clone(),
                        input: input.clone(),
                        profile: profile.clone(),
                        current_profile: current_profile.clone(),
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
            Event::ModelTurnCommitted { turn, .. } => {
                let reported = &turn.usage;
                let total = usage.get_or_insert_with(Usage::default);
                add_usage(&mut total.input_tokens, reported.input_tokens);
                add_usage(
                    &mut total.input_tokens_no_cache,
                    reported.input_tokens_no_cache,
                );
                add_usage(
                    &mut total.input_tokens_cache_read,
                    reported.input_tokens_cache_read,
                );
                add_usage(
                    &mut total.input_tokens_cache_write,
                    reported.input_tokens_cache_write,
                );
                add_usage(&mut total.output_tokens, reported.output_tokens);
                add_usage(&mut total.output_tokens_text, reported.output_tokens_text);
                add_usage(
                    &mut total.output_tokens_reasoning,
                    reported.output_tokens_reasoning,
                );
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
        rename_records,
        log,
    })
}

fn add_usage(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or_default().saturating_add(value));
    }
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

fn write_project_cwd(project_dir: &Path, cwd: &Path) -> Result<(), SessionError> {
    let Ok(canonical) = cwd.canonicalize() else {
        return Ok(());
    };
    let bytes = canonical.as_os_str().as_bytes();
    let path = project_dir.join(PROJECT_CWD_FILE);
    if project_cwd_is_current(&path, bytes) {
        return Ok(());
    }

    let temporary = project_dir.join(format!(".{PROJECT_CWD_FILE}.{}.tmp", Uuid::now_v7()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)?;
        fs::File::open(project_dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|source| SessionError::Io { path, source })
}

fn project_cwd_is_current(path: &Path, expected: &[u8]) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    metadata.is_file()
        && metadata.mode() & 0o7777 == 0o600
        && fs::read(path).is_ok_and(|bytes| bytes == expected)
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        os::unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, PermissionsExt, symlink},
        },
        path::Path,
    };

    use super::{PROJECT_CWD_FILE, SessionStore};

    fn cwd_file(data_root: &Path, cwd: &Path) -> std::path::PathBuf {
        SessionStore::project_dir(data_root, cwd).join(PROJECT_CWD_FILE)
    }

    #[test]
    fn canonical_aliases_share_the_existing_project_and_record_canonical_bytes() {
        let temp = tempfile::tempdir().expect("temp");
        let real = temp.path().join("real");
        let alias = temp.path().join("alias");
        let data = temp.path().join("data");
        fs::create_dir(&real).expect("real cwd");
        symlink(&real, &alias).expect("alias");

        let real_store = SessionStore::open(&data, &real).expect("real store");
        let alias_store = SessionStore::open(&data, &alias).expect("alias store");
        assert_eq!(
            real_store.project_dir_path(),
            alias_store.project_dir_path()
        );
        assert_eq!(
            fs::read(cwd_file(&data, &alias)).expect("cwd bytes"),
            real.canonicalize()
                .expect("canonical")
                .as_os_str()
                .as_bytes()
        );
    }

    #[test]
    fn non_utf8_cwd_round_trips_exact_bytes() {
        let temp = tempfile::tempdir().expect("temp");
        let cwd = temp
            .path()
            .join(OsString::from_vec(b"project-\xfe\xff".to_vec()));
        let data = temp.path().join("data");
        fs::create_dir(&cwd).expect("cwd");

        SessionStore::open(&data, &cwd).expect("store");
        assert_eq!(
            fs::read(cwd_file(&data, &cwd)).expect("cwd bytes"),
            cwd.canonicalize()
                .expect("canonical")
                .as_os_str()
                .as_bytes()
        );
    }

    #[test]
    fn cwd_file_is_private_and_bad_mode_is_atomically_refreshed() {
        let temp = tempfile::tempdir().expect("temp");
        let data = temp.path().join("data");
        SessionStore::open(&data, temp.path()).expect("store");
        let path = cwd_file(&data, temp.path());
        assert_eq!(
            fs::metadata(&path).expect("metadata").mode() & 0o7777,
            0o600
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("loosen mode");
        SessionStore::open(&data, temp.path()).expect("reopen");
        assert_eq!(fs::metadata(path).expect("metadata").mode() & 0o7777, 0o600);
    }

    #[test]
    fn stale_cwd_file_is_replaced_atomically() {
        let temp = tempfile::tempdir().expect("temp");
        let data = temp.path().join("data");
        SessionStore::open(&data, temp.path()).expect("store");
        let path = cwd_file(&data, temp.path());
        fs::write(&path, b"stale project path").expect("stale file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        let stale_inode = fs::metadata(&path).expect("stale metadata").ino();

        SessionStore::open(&data, temp.path()).expect("refresh");
        assert_ne!(
            fs::metadata(&path).expect("new metadata").ino(),
            stale_inode
        );
        assert_eq!(
            fs::read(&path).expect("cwd bytes"),
            temp.path()
                .canonicalize()
                .expect("canonical")
                .as_os_str()
                .as_bytes()
        );
        let project = path.parent().expect("project");
        assert!(
            fs::read_dir(project)
                .expect("project entries")
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".cwd."))
        );
    }

    #[test]
    fn correct_cwd_file_is_retained_on_reopen() {
        let temp = tempfile::tempdir().expect("temp");
        let data = temp.path().join("data");
        SessionStore::open(&data, temp.path()).expect("store");
        let path = cwd_file(&data, temp.path());
        let inode = fs::metadata(&path).expect("metadata").ino();

        SessionStore::open(&data, temp.path()).expect("reopen");
        assert_eq!(fs::metadata(path).expect("metadata").ino(), inode);
    }

    #[test]
    fn existing_project_folder_gains_cwd_file_without_moving_children() {
        let temp = tempfile::tempdir().expect("temp");
        let data = temp.path().join("data");
        let project = SessionStore::project_dir(&data, temp.path());
        assert_eq!(
            project.file_name().expect("hash").to_string_lossy().len(),
            16
        );
        fs::create_dir_all(project.join("sessions")).expect("existing project");
        fs::write(project.join("sentinel"), b"keep").expect("sentinel");
        assert!(!project.join(PROJECT_CWD_FILE).exists());

        SessionStore::open(&data, temp.path()).expect("open existing project");
        assert_eq!(
            fs::read(project.join("sentinel")).expect("sentinel"),
            b"keep"
        );
        assert!(project.join("sessions").is_dir());
        assert!(project.join(PROJECT_CWD_FILE).is_file());
    }
}
