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

use cookie_agent_protocol::{
    AgentSnapshot, ChildSummary, ClientRunId, EventPayload, RunId, RunSelection, SessionId,
    SessionMeta, SessionMetaSchemaVersion, SessionOrigin, SessionRenameRecord, SessionStatus,
    SessionTitleChange, SessionTree, ToolCallId, Usage,
};
use serde::Deserialize;
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

#[derive(Clone, Debug)]
pub struct RunProjection {
    pub id: RunId,
    pub client_run_id: ClientRunId,
    pub input: String,
    pub selection: RunSelection,
    pub agent: AgentSnapshot,
    pub status: SessionStatus,
    pub final_text: Option<String>,
    pub pending_calls: HashMap<ToolCallId, String>,
}

#[derive(Clone, Debug)]
pub struct SessionProjection {
    pub meta: SessionMeta,
    pub creation_agent: AgentSnapshot,
    pub status: SessionStatus,
    pub usage: Option<Usage>,
    pub runs: HashMap<RunId, RunProjection>,
    pub rename_records: HashMap<cookie_agent_protocol::ClientRenameId, SessionRenameRecord>,
    pub log: Arc<EventLog>,
}

impl SessionProjection {
    #[must_use]
    pub fn metadata(&self) -> SessionMeta {
        let mut meta = self.meta.clone();
        let latest = self
            .log
            .last_event()
            .expect("session event log always has SessionCreated");
        meta.last_activity = latest.timestamp;
        meta
    }
}

#[derive(Deserialize)]
struct PersistedSessionMetaVersion {
    meta_schema_version: SessionMetaSchemaVersion,
}

#[derive(Debug)]
pub struct SessionStore {
    project_dir: PathBuf,
    sessions_dir: PathBuf,
    cwd: PathBuf,
    sessions: Mutex<HashMap<SessionId, SessionProjection>>,
    mutation: Mutex<()>,
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
            cwd: cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned()),
            sessions: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
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
            read_cache_version(&entry.path().join("meta.json"))?;
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
        session_id: SessionId,
        creation: EventPayload,
    ) -> Result<Arc<EventLog>, SessionError> {
        self.create_with_status(session_id, creation)
            .map(|(log, _)| log)
    }

    /// Creates a session atomically and reports whether this caller won creation.
    pub fn create_with_status(
        &self,
        session_id: SessionId,
        creation: EventPayload,
    ) -> Result<(Arc<EventLog>, bool), SessionError> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session_id)
            .cloned()
        {
            return Ok((existing.log, false));
        }
        let final_dir = self.sessions_dir.join(session_id.to_string());
        if final_dir.exists() {
            read_cache_version(&final_dir.join("meta.json"))?;
            let log = EventLog::open(final_dir.join("events.jsonl"), session_id)?;
            let existing = projection(log.clone())?;
            self.sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(session_id, existing);
            return Ok((log, false));
        }
        let log = EventLog::create_buffered(final_dir.join("events.jsonl"), session_id, creation)?;
        let result = projection(log.clone())?;
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id, result);
        Ok((log, true))
    }

    pub fn get(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id)
            .cloned()
            .ok_or(SessionError::Missing(id))
    }

    pub fn append(
        &self,
        id: SessionId,
        run: Option<RunId>,
        event: EventPayload,
    ) -> Result<cookie_agent_protocol::StoredEvent, SessionError> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.get(id)?;
        let first_user_message =
            !current.log.is_persisted() && matches!(event, EventPayload::UserInputSubmitted { .. });
        let envelope = current.log.append(run, event)?;
        let rebuilt = projection(current.log.clone())?;
        if first_user_message {
            self.persist_buffered(id, &rebuilt)?;
        } else if current.log.is_persisted() {
            write_cache(
                &self.sessions_dir.join(id.to_string()).join("meta.json"),
                &rebuilt.meta,
            )?;
        }
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, rebuilt);
        Ok(envelope)
    }

    fn persist_buffered(
        &self,
        session_id: SessionId,
        projection: &SessionProjection,
    ) -> Result<(), SessionError> {
        let final_dir = self.sessions_dir.join(session_id.to_string());
        let temporary = self
            .sessions_dir
            .join(format!(".{session_id}.{}.tmp", SessionId::new_v7()));
        fs::create_dir_all(&temporary).map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        let result = (|| {
            let log_path = temporary.join("events.jsonl");
            for event in projection.log.events() {
                crate::events::append_jsonl(&log_path, &event)?;
            }
            write_cache(&temporary.join("meta.json"), &projection.meta)?;
            fsync_directory(&temporary)?;
            fs::rename(&temporary, &final_dir).map_err(|source| SessionError::Io {
                path: final_dir.clone(),
                source,
            })?;
            fsync_directory(&self.sessions_dir)?;
            Ok::<(), SessionError>(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
            return result;
        }
        projection.log.mark_persisted();
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
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    #[must_use]
    pub fn session_dir(&self, id: SessionId) -> PathBuf {
        self.sessions_dir.join(id.to_string())
    }

    pub fn is_persisted(&self, id: SessionId) -> Result<bool, SessionError> {
        Ok(self.get(id)?.log.is_persisted())
    }

    pub fn children(&self, parent: SessionId) -> Vec<ChildSummary> {
        self.all()
            .into_iter()
            .filter_map(|child| match child.meta.origin {
                SessionOrigin::Delegated {
                    parent_session_id, ..
                } if parent_session_id == parent => Some(ChildSummary {
                    session_id: child.meta.session_id,
                    agent: child.meta.creation_selection.agent.clone(),
                    title: child.meta.title.clone(),
                    title_updated_seq: child.meta.title_updated_seq,
                    status: child.status,
                    usage: child.usage,
                }),
                _ => None,
            })
            .collect()
    }

    pub fn tree(&self, id: SessionId) -> Result<SessionTree, SessionError> {
        let session = self.get(id)?.metadata();
        Ok(SessionTree {
            session,
            children: self
                .children(id)
                .into_iter()
                .map(|child| self.tree(child.session_id))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

fn projection(log: Arc<EventLog>) -> Result<SessionProjection, SessionError> {
    let events = log.events();
    let (
        origin,
        cwd_identity,
        creation_selection,
        creation_agent,
        runtime_revision,
        catalog_revision,
        provider_state_revision,
        model_revision,
        agent_revision,
        recipe_registry_revision,
        manifest_revision,
    ) = match &events
        .first()
        .expect("creation checked by EventLog")
        .payload
    {
        EventPayload::SessionCreated {
            origin,
            cwd_identity,
            creation_selection,
            creation_agent,
            runtime_revision,
            catalog_revision,
            provider_state_revision,
            model_revision,
            agent_revision,
            recipe_registry_revision,
            manifest_revision,
        } => (
            origin.clone(),
            cwd_identity.clone(),
            creation_selection.clone(),
            creation_agent.as_ref().clone(),
            runtime_revision.clone(),
            catalog_revision.clone(),
            provider_state_revision.clone(),
            model_revision.clone(),
            agent_revision.clone(),
            recipe_registry_revision.clone(),
            manifest_revision.clone(),
        ),
        _ => unreachable!(),
    };
    let mut meta = SessionMeta {
        meta_schema_version: cookie_agent_protocol::SessionMetaSchemaVersion::current(),
        session_id: events[0].session_id,
        origin,
        cwd_identity,
        creation_selection,
        runtime_revision,
        catalog_revision,
        provider_state_revision,
        model_revision,
        agent_revision,
        recipe_registry_revision,
        manifest_revision,
        title: None,
        title_updated_seq: 0,
        last_event_seq: events.last().map_or(1, |event| event.seq),
        last_activity: events
            .last()
            .expect("creation checked by EventLog")
            .timestamp,
        status: SessionStatus::Idle,
    };
    let mut runs = HashMap::new();
    let mut status = SessionStatus::Idle;
    let mut usage = None;
    let mut rename_records = HashMap::new();
    let mut automatic_title = None;
    let mut user_title: Option<Option<cookie_agent_protocol::SessionTitle>> = None;
    for envelope in &events {
        if let EventPayload::SessionTitleCommitted { change, .. } = &envelope.payload {
            match change {
                SessionTitleChange::UserSet { title, .. } => {
                    user_title = Some(Some(title.clone()));
                }
                SessionTitleChange::UserClear { .. } => user_title = Some(None),
                SessionTitleChange::UserReset { .. } => user_title = None,
                SessionTitleChange::InternalAgentSet { title, .. }
                | SessionTitleChange::FallbackSet { title } => {
                    automatic_title = Some(title.clone());
                }
            }
            meta.title = user_title
                .clone()
                .unwrap_or_else(|| automatic_title.clone());
            meta.title_updated_seq = envelope.seq;
            if let Some(record) = change.user_rename_record() {
                rename_records.insert(record.client_rename_id.clone(), record);
            }
        }
        let Some(run_id) = envelope.run_id else {
            continue;
        };
        match &envelope.payload {
            EventPayload::RunStarted {
                client_run_id,
                selection,
                agent,
                ..
            } => {
                status = SessionStatus::Running;
                runs.insert(
                    run_id,
                    RunProjection {
                        id: run_id,
                        client_run_id: client_run_id.clone(),
                        input: String::new(),
                        selection: selection.clone(),
                        agent: agent.as_ref().clone(),
                        status: SessionStatus::Running,
                        final_text: None,
                        pending_calls: HashMap::new(),
                    },
                );
            }
            // User input is prompt history, not a lifecycle transition.
            EventPayload::UserInputSubmitted { input } => {
                if let Some(run) = runs.get_mut(&run_id)
                    && run.input.is_empty()
                {
                    run.input = input.clone();
                }
            }
            EventPayload::UserInputApplied { .. } => {}
            EventPayload::RunCompleted { final_text } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Completed;
                    run.final_text = final_text.clone();
                    status = SessionStatus::Completed;
                }
            }
            EventPayload::RunFailed { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Failed;
                    status = SessionStatus::Failed;
                }
            }
            EventPayload::RunCancelled { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Cancelled;
                    status = SessionStatus::Cancelled;
                }
            }
            EventPayload::RunInterrupted { .. } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = SessionStatus::Interrupted;
                    status = SessionStatus::Interrupted;
                }
            }
            EventPayload::ToolCallStarted { start } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    let tool = turns_tool_name(&events, &start.owner).unwrap_or_default();
                    run.pending_calls.insert(start.tool_call_id, tool);
                }
            }
            EventPayload::ToolCallTerminated { termination } => {
                if let Some(run) = runs.get_mut(&run_id) {
                    run.pending_calls.remove(&termination.tool_call_id);
                }
            }
            EventPayload::ModelTurnCommitted { turn, .. } => {
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
    meta.status = status;
    Ok(SessionProjection {
        meta,
        creation_agent,
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

fn turns_tool_name(
    events: &[cookie_agent_protocol::StoredEvent],
    owner: &cookie_agent_protocol::AssistantToolCallRef,
) -> Option<String> {
    events.iter().find_map(|event| match &event.payload {
        EventPayload::ModelTurnCommitted {
            model_turn_seq,
            turn,
            ..
        } if *model_turn_seq == owner.model_turn_seq => {
            match turn.content.get(owner.content_index as usize) {
                Some(cookie_agent_protocol::PersistedAssistantPart::ToolCall { name, .. }) => {
                    Some(name.as_str().to_owned())
                }
                _ => None,
            }
        }
        _ => None,
    })
}

fn write_cache(path: &Path, cache: &SessionMeta) -> Result<(), SessionError> {
    let mut persisted = serde_json::to_value(cache).map_err(|source| SessionError::Json {
        path: path.to_owned(),
        source,
    })?;
    persisted
        .as_object_mut()
        .expect("SessionMeta serializes as an object")
        .remove("last_activity");
    let bytes = serde_json::to_vec_pretty(&persisted).map_err(|source| SessionError::Json {
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

fn read_cache_version(path: &Path) -> Result<(), SessionError> {
    let bytes = fs::read(path).map_err(|source| SessionError::Io {
        path: path.to_owned(),
        source,
    })?;
    let version: PersistedSessionMetaVersion =
        serde_json::from_slice(&bytes).map_err(|source| SessionError::Json {
            path: path.to_owned(),
            source,
        })?;
    let _ = version.meta_schema_version;
    Ok(())
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
