//! Session directories, projections, and rebuildable metadata caches.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(unix)]
use std::{
    fs::OpenOptions,
    os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    },
};

use cookie_agent_protocol::{
    AgentId, AgentSnapshot, ChildSummary, ClientRenameId, ClientRunId, EventPayload, RunId,
    RunSelection, SessionId, SessionMeta, SessionOrigin, SessionPermissionOverlay,
    SessionRenameRecord, SessionStatus, SessionTitle, SessionTitleChange, SessionTree, ToolCallId,
    Usage, UsageRollup,
};
use thiserror::Error;
use uuid::Uuid;

use crate::events::{EventLog, EventLogError, fsync_directory};
use crate::ownership::{
    HeldLock, SessionOwnership, WriteAuthority, WriteCapability, owner_lock_path, try_acquire,
};

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
    #[error("session {0} is owned by another cookie process")]
    SessionLocked(SessionId),
    #[error("session store is closed")]
    StoreClosed,
    #[error("sequence {through_seq} is not a valid event in session {session_id}")]
    InvalidSequence {
        session_id: SessionId,
        through_seq: u64,
    },
    #[error("invalid fork title: {0}")]
    InvalidForkTitle(String),
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
    pub usage_rollup: UsageRollup,
    pub agent_usage: BTreeMap<AgentId, UsageRollup>,
    pub runs: HashMap<RunId, RunProjection>,
    pub rename_records: HashMap<cookie_agent_protocol::ClientRenameId, SessionRenameRecord>,
    pub permission_overlay: SessionPermissionOverlay,
    pub log: Arc<EventLog>,
}

#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub meta: SessionMeta,
    pub usage: Option<Usage>,
    pub usage_rollup: UsageRollup,
    pub agent_usage: BTreeMap<AgentId, UsageRollup>,
}

impl SessionProjection {
    #[must_use]
    pub fn metadata(&self) -> SessionMeta {
        self.meta.clone()
    }
}

#[derive(Debug, Default)]
struct SessionResidency {
    resident: HashMap<SessionId, SessionProjection>,
    evicted: HashMap<SessionId, SessionSummary>,
}

#[derive(Debug)]
enum StoreOwnership {
    PendingPublish {
        authority: WriteAuthority,
    },
    Adopting {
        _lock: HeldLock,
        authority: WriteAuthority,
    },
    Owned {
        _lock: HeldLock,
        authority: WriteAuthority,
    },
    Foreign,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteOpen {
    AlreadyOwned,
    Adopting,
}

#[cfg(test)]
#[derive(Debug)]
struct EvictionTransitionHook {
    reached: Mutex<Option<tokio::sync::oneshot::Sender<SessionId>>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
#[derive(Debug)]
struct PublishHook {
    reached: std::sync::mpsc::Sender<SessionId>,
    release: std::sync::mpsc::Receiver<()>,
}

#[derive(Debug)]
pub struct SessionStore {
    project_dir: PathBuf,
    sessions_dir: PathBuf,
    cwd: PathBuf,
    residency: Mutex<SessionResidency>,
    ownership: Mutex<HashMap<SessionId, StoreOwnership>>,
    adoption_locks: Mutex<HashMap<SessionId, Arc<Mutex<()>>>>,
    mutation: Mutex<()>,
    closed: AtomicBool,
    #[cfg(test)]
    eviction_transition_hook: Mutex<Option<EvictionTransitionHook>>,
    #[cfg(test)]
    publish_hook: Mutex<Option<PublishHook>>,
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
        #[cfg(unix)]
        create_unix_session_directory_all(&sessions_dir)?;
        #[cfg(windows)]
        for path in [
            data_root.join("projects"),
            project_dir.clone(),
            sessions_dir.clone(),
        ] {
            create_windows_session_directory(&path)?;
        }
        write_project_cwd(&project_dir, cwd)?;
        let store = Arc::new(Self {
            project_dir,
            sessions_dir: sessions_dir.clone(),
            cwd: cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned()),
            residency: Mutex::new(SessionResidency::default()),
            ownership: Mutex::new(HashMap::new()),
            adoption_locks: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
            closed: AtomicBool::new(false),
            #[cfg(test)]
            eviction_transition_hook: Mutex::new(None),
            #[cfg(test)]
            publish_hook: Mutex::new(None),
        });
        store.refresh_discovered();
        Ok(store)
    }

    pub fn create(
        &self,
        session_id: SessionId,
        origin: cookie_agent_protocol::EventOrigin,
        creation: EventPayload,
    ) -> Result<Arc<EventLog>, SessionError> {
        self.create_with_status(session_id, origin, creation)
            .map(|(log, _)| log)
    }

    /// Creates a session atomically and reports whether this caller won creation.
    pub fn create_with_status(
        &self,
        session_id: SessionId,
        origin: cookie_agent_protocol::EventOrigin,
        creation: EventPayload,
    ) -> Result<(Arc<EventLog>, bool), SessionError> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_open()?;
        if let Some(existing) = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .get(&session_id)
            .cloned()
        {
            return Ok((existing.log, false));
        }
        let final_dir = self.sessions_dir.join(session_id.to_string());
        if final_dir.exists() {
            return Err(SessionError::SessionLocked(session_id));
        }
        let authority = WriteAuthority::new();
        let log = EventLog::create_buffered_owned(
            final_dir.join("events.jsonl"),
            session_id,
            origin,
            creation,
            authority.capability(),
        )?;
        let result = projection(log.clone())?;
        self.ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id, StoreOwnership::PendingPublish { authority });
        let mut residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        residency.resident.insert(session_id, result);
        residency.evicted.remove(&session_id);
        Ok((log, true))
    }

    pub fn get(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        if let Some(session) = self.get_resident(id) {
            return Ok(session);
        }
        if self.is_owned(id) {
            return self.reopen_owned(id);
        }
        self.open_snapshot(id)
    }

    #[must_use]
    pub fn get_resident(&self, id: SessionId) -> Option<SessionProjection> {
        self.residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .get(&id)
            .cloned()
    }

    fn reopen_owned(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .get(&id)
            .cloned()
        {
            return Ok(session);
        }
        let session_dir = self.sessions_dir.join(id.to_string());
        if !session_dir.is_dir() {
            return Err(SessionError::Missing(id));
        }
        let capability = self.write_capability(id, false)?;
        let log = EventLog::open_owned(session_dir.join("events.jsonl"), id, capability)?;
        let reopened = projection(log)?;
        let mut residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        residency.resident.insert(id, reopened.clone());
        residency.evicted.remove(&id);
        Ok(reopened)
    }

    fn open_snapshot(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        let session_dir = self.sessions_dir.join(id.to_string());
        if !session_dir.is_dir() {
            return Err(SessionError::Missing(id));
        }
        let snapshot = projection(EventLog::open_read_only(
            session_dir.join("events.jsonl"),
            id,
        )?)?;
        self.residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .evicted
            .insert(id, summary_from_projection(&snapshot));
        Ok(snapshot)
    }

    pub(crate) fn begin_write(&self, id: SessionId) -> Result<WriteOpen, SessionError> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_open()?;
        self.begin_write_locked(id)
    }

    #[cfg(test)]
    pub(crate) fn open_for_write(&self, id: SessionId) -> Result<SessionProjection, SessionError> {
        let adoption_lock = self.adoption_lock(id);
        let _adoption = adoption_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.begin_write(id)? == WriteOpen::Adopting {
            self.commit_adoption(id)?;
        }
        self.get(id)
    }

    fn begin_write_locked(&self, id: SessionId) -> Result<WriteOpen, SessionError> {
        if self.is_owned(id) {
            if let Some(session) = self.get_resident(id) {
                drop(session);
                return Ok(WriteOpen::AlreadyOwned);
            }
            let session_dir = self.sessions_dir.join(id.to_string());
            let capability = self.write_capability(id, false)?;
            let reopened = projection(EventLog::open_owned(
                session_dir.join("events.jsonl"),
                id,
                capability,
            )?)?;
            let mut residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            residency.resident.insert(id, reopened.clone());
            residency.evicted.remove(&id);
            return Ok(WriteOpen::AlreadyOwned);
        }
        let session_dir = self.sessions_dir.join(id.to_string());
        if !session_dir.is_dir() {
            return Err(SessionError::Missing(id));
        }
        let lock = match try_acquire(&session_dir) {
            Ok(SessionOwnership::Owned(lock)) => lock,
            Ok(SessionOwnership::Foreign) => {
                self.ownership
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id, StoreOwnership::Foreign);
                return Err(SessionError::SessionLocked(id));
            }
            Err(error) => {
                eprintln!("session {id} ownership classification failed: {error}");
                self.ownership
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id, StoreOwnership::Foreign);
                return Err(SessionError::SessionLocked(id));
            }
        };
        let authority = WriteAuthority::new();
        let opened =
            EventLog::open_owned(session_dir.join("events.jsonl"), id, authority.capability());
        let projection = match opened.and_then(|log| {
            projection(log).map_err(|error| match error {
                SessionError::Event(error) => error,
                _ => unreachable!("projection only returns event-log errors"),
            })
        }) {
            Ok(projection) => projection,
            Err(error) => {
                eprintln!("session {id} adoption failed closed: {error}");
                return Err(SessionError::SessionLocked(id));
            }
        };
        self.ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                id,
                StoreOwnership::Adopting {
                    _lock: lock,
                    authority,
                },
            );
        let mut residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        residency.resident.insert(id, projection.clone());
        residency.evicted.remove(&id);
        Ok(WriteOpen::Adopting)
    }

    pub(crate) fn commit_adoption(&self, id: SessionId) -> Result<(), SessionError> {
        self.ensure_open()?;
        let mut ownership = self
            .ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = ownership
            .remove(&id)
            .ok_or(SessionError::SessionLocked(id))?;
        match state {
            StoreOwnership::Adopting { _lock, authority } => {
                ownership.insert(id, StoreOwnership::Owned { _lock, authority });
                Ok(())
            }
            state => {
                ownership.insert(id, state);
                Err(SessionError::SessionLocked(id))
            }
        }
    }

    pub(crate) fn rollback_adoption(&self, id: SessionId) {
        let projection = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .remove(&id);
        if let Some(projection) = projection {
            let _ = projection.log.suspend_writer();
            self.residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .evicted
                .insert(id, summary_from_projection(&projection));
        }
        let removed = self
            .ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        debug_assert!(
            matches!(removed, Some(StoreOwnership::Adopting { .. }))
                || self.closed.load(Ordering::Acquire)
        );
    }

    pub(crate) fn adoption_lock(&self, id: SessionId) -> Arc<Mutex<()>> {
        self.adoption_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn write_capability(
        &self,
        id: SessionId,
        allow_adopting: bool,
    ) -> Result<WriteCapability, SessionError> {
        let ownership = self
            .ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match ownership.get(&id) {
            Some(
                StoreOwnership::PendingPublish { authority }
                | StoreOwnership::Owned { authority, .. },
            ) => Ok(authority.capability()),
            Some(StoreOwnership::Adopting { authority, .. }) if allow_adopting => {
                Ok(authority.capability())
            }
            _ => Err(SessionError::SessionLocked(id)),
        }
    }

    #[must_use]
    pub fn is_owned(&self, id: SessionId) -> bool {
        matches!(
            self.ownership
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&id),
            Some(StoreOwnership::PendingPublish { .. } | StoreOwnership::Owned { .. })
        )
    }

    pub fn evict(&self, id: SessionId) -> Result<bool, SessionError> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(session) = residency.resident.get(&id) else {
            return Ok(false);
        };
        if !session.log.is_persisted() {
            return Ok(false);
        }
        // Stream records may be published before their grouped sync. Flush before
        // removing the resident projection so eviction never outruns durability.
        session.log.flush()?;
        let summary = SessionSummary {
            meta: session.meta.clone(),
            usage: session.usage.clone(),
            usage_rollup: session.usage_rollup.clone(),
            agent_usage: session.agent_usage.clone(),
        };
        residency.evicted.insert(id, summary);
        #[cfg(test)]
        if let Some(hook) = self
            .eviction_transition_hook
            .lock()
            .expect("eviction transition hook lock poisoned")
            .take()
        {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("eviction transition reached lock poisoned")
                .take()
            {
                let _ = reached.send(id);
            }
            let _ = hook
                .release
                .lock()
                .expect("eviction transition release lock poisoned")
                .recv();
        }
        residency.resident.remove(&id);
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn install_eviction_transition_hook_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<SessionId>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        *self
            .eviction_transition_hook
            .lock()
            .expect("eviction transition hook lock poisoned") = Some(EvictionTransitionHook {
            reached: Mutex::new(Some(reached)),
            release: Mutex::new(release_receiver),
        });
        (receiver, release)
    }

    #[cfg(test)]
    fn install_publish_hook_for_test(
        &self,
    ) -> (
        std::sync::mpsc::Receiver<SessionId>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached, reached_receiver) = std::sync::mpsc::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        *self
            .publish_hook
            .lock()
            .expect("publish hook lock poisoned") = Some(PublishHook {
            reached,
            release: release_receiver,
        });
        (reached_receiver, release)
    }

    pub fn append(
        &self,
        id: SessionId,
        run: Option<RunId>,
        origin: cookie_agent_protocol::EventOrigin,
        event: EventPayload,
    ) -> Result<cookie_agent_protocol::StoredEvent, SessionError> {
        self.append_with_mode(id, run, origin, event, false)
    }

    pub(crate) fn append_recovery(
        &self,
        id: SessionId,
        run: Option<RunId>,
        origin: cookie_agent_protocol::EventOrigin,
        event: EventPayload,
    ) -> Result<cookie_agent_protocol::StoredEvent, SessionError> {
        self.append_with_mode(id, run, origin, event, true)
    }

    fn append_with_mode(
        &self,
        id: SessionId,
        run: Option<RunId>,
        origin: cookie_agent_protocol::EventOrigin,
        event: EventPayload,
        recovery: bool,
    ) -> Result<cookie_agent_protocol::StoredEvent, SessionError> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_open()?;
        let capability = self.write_capability(id, recovery)?;
        let current = self.get(id)?;
        let first_user_message = !current.log.is_persisted()
            && matches!(
                event,
                EventPayload::UserInputAdmitted { .. }
                    | EventPayload::UserInputSubmitted { .. }
                    | EventPayload::DelegatedContextSeeded { .. }
                    | EventPayload::SessionPermissionOverlaySet { .. }
                    | EventPayload::SkillLoaded { .. }
                    | EventPayload::AgentMdLoaded { .. }
            );
        let envelope = current.log.append_owned(&capability, run, origin, event)?;
        let rebuilt = projection(current.log.clone())?;
        if first_user_message {
            self.persist_buffered(id, &rebuilt)?;
        } else if current.log.is_persisted() {
            write_cache(
                &self.sessions_dir.join(id.to_string()).join("meta.json"),
                &rebuilt.meta,
            )?;
        }
        let mut residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        residency.resident.insert(id, rebuilt);
        residency.evicted.remove(&id);
        Ok(envelope)
    }

    pub fn fork(
        &self,
        source_id: SessionId,
        through_seq: u64,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<SessionId, SessionError> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_open()?;
        let source = self.get(source_id)?;
        if !source.log.is_persisted() {
            return Err(SessionError::InvalidSequence {
                session_id: source_id,
                through_seq,
            });
        }
        source.log.suspend_writer()?;
        let source_events = source.log.all_events();
        let prefix = source_events
            .iter()
            .filter(|event| event.seq <= through_seq)
            .cloned()
            .collect::<Vec<_>>();
        if through_seq == 0
            || through_seq > source_events.last().map_or(0, |event| event.seq)
            || !cookie_agent_protocol::visible_events(&prefix)
                .iter()
                .any(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
        {
            return Err(SessionError::InvalidSequence {
                session_id: source_id,
                through_seq,
            });
        }

        let session_id = SessionId::new_v7();
        let final_dir = self.sessions_dir.join(session_id.to_string());
        let temporary = self
            .sessions_dir
            .join(format!(".{session_id}.{}.tmp", SessionId::new_v7()));
        #[cfg(unix)]
        fs::create_dir(&temporary).map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        #[cfg(windows)]
        create_windows_session_directory(&temporary)?;
        #[cfg(unix)]
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700)).map_err(|source| {
            SessionError::Io {
                path: temporary.clone(),
                source,
            }
        })?;
        let authority = WriteAuthority::new();
        let capability = authority.capability();
        let result = (|| {
            let log_path = temporary.join("events.jsonl");
            #[cfg(windows)]
            create_windows_session_file(&log_path)?;
            for event in source_events
                .iter()
                .filter(|event| event.seq <= through_seq)
            {
                let mut copied = event.clone();
                copied.session_id = session_id;
                crate::events::append_copied_event_jsonl(&log_path, &copied)?;
            }
            #[cfg(unix)]
            fs::set_permissions(&log_path, fs::Permissions::from_mode(0o600)).map_err(
                |source| SessionError::Io {
                    path: log_path.clone(),
                    source,
                },
            )?;
            let log = EventLog::open_owned(log_path, session_id, capability.clone())?;
            log.append_owned(
                &capability,
                None,
                origin,
                EventPayload::SessionReverted { through_seq },
            )?;
            let prefix_projection = projection(log.clone())?;
            let title = fork_title(prefix_projection.meta.title.as_ref())?;
            log.append_owned(
                &capability,
                None,
                cookie_agent_protocol::EventOrigin::new("user")
                    .expect("static event origin is valid"),
                EventPayload::SessionTitleCommitted {
                    change: SessionTitleChange::UserSet {
                        title,
                        client_rename_id: ClientRenameId::new(format!("fork-{session_id}"))
                            .expect("fork rename ID is bounded"),
                    },
                    input_through_seq: through_seq,
                },
            )?;
            log.suspend_writer()?;
            let fork_projection = projection(log)?;
            write_cache(&temporary.join("meta.json"), &fork_projection.meta)?;
            #[cfg(unix)]
            let lock_session_dir = &temporary;
            #[cfg(windows)]
            let lock_session_dir = &final_dir;
            let lock = match try_acquire(lock_session_dir).map_err(|source| SessionError::Io {
                path: owner_lock_path(lock_session_dir),
                source,
            })? {
                SessionOwnership::Owned(lock) => lock,
                SessionOwnership::Foreign => return Err(SessionError::SessionLocked(session_id)),
            };
            #[cfg(test)]
            if let Some(hook) = self
                .publish_hook
                .lock()
                .expect("publish hook lock poisoned")
                .take()
            {
                let _ = hook.reached.send(session_id);
                let _ = hook.release.recv();
            }
            fsync_directory(&temporary)?;
            fs::rename(&temporary, &final_dir).map_err(|source| SessionError::Io {
                path: final_dir.clone(),
                source,
            })?;
            fsync_directory(&self.sessions_dir)?;
            self.ownership
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    session_id,
                    StoreOwnership::Owned {
                        _lock: lock,
                        authority,
                    },
                );
            let log = EventLog::open_owned(final_dir.join("events.jsonl"), session_id, capability)?;
            let fork_projection = projection(log)?;
            self.residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .resident
                .insert(session_id, fork_projection);
            Ok(session_id)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result
    }

    fn persist_buffered(
        &self,
        session_id: SessionId,
        projection: &SessionProjection,
    ) -> Result<(), SessionError> {
        self.write_capability(session_id, false)?;
        let final_dir = self.sessions_dir.join(session_id.to_string());
        let temporary = self
            .sessions_dir
            .join(format!(".{session_id}.{}.tmp", SessionId::new_v7()));
        #[cfg(unix)]
        create_unix_session_directory_all(&temporary)?;
        #[cfg(windows)]
        create_windows_session_directory(&temporary)?;
        let result = (|| {
            let log_path = temporary.join("events.jsonl");
            #[cfg(windows)]
            create_windows_session_file(&log_path)?;
            for event in projection.log.all_events() {
                crate::events::append_jsonl(&log_path, &event)?;
            }
            write_cache(&temporary.join("meta.json"), &projection.meta)?;
            #[cfg(unix)]
            let lock_session_dir = &temporary;
            #[cfg(windows)]
            let lock_session_dir = &final_dir;
            let lock = match try_acquire(lock_session_dir).map_err(|source| SessionError::Io {
                path: owner_lock_path(lock_session_dir),
                source,
            })? {
                SessionOwnership::Owned(lock) => lock,
                SessionOwnership::Foreign => return Err(SessionError::SessionLocked(session_id)),
            };
            #[cfg(test)]
            if let Some(hook) = self
                .publish_hook
                .lock()
                .expect("publish hook lock poisoned")
                .take()
            {
                let _ = hook.reached.send(session_id);
                let _ = hook.release.recv();
            }
            fsync_directory(&temporary)?;
            fs::rename(&temporary, &final_dir).map_err(|source| SessionError::Io {
                path: final_dir.clone(),
                source,
            })?;
            fsync_directory(&self.sessions_dir)?;
            let mut ownership = self
                .ownership
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let state = ownership
                .remove(&session_id)
                .ok_or(SessionError::SessionLocked(session_id))?;
            match state {
                StoreOwnership::PendingPublish { authority } => {
                    ownership.insert(
                        session_id,
                        StoreOwnership::Owned {
                            _lock: lock,
                            authority,
                        },
                    );
                }
                state => {
                    ownership.insert(session_id, state);
                    return Err(SessionError::SessionLocked(session_id));
                }
            }
            Ok::<(), SessionError>(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
            return result;
        }
        projection.log.mark_persisted();
        Ok(())
    }

    pub(crate) fn persist_buffered_session(&self, id: SessionId) -> Result<(), SessionError> {
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_open()?;
        let projection = self.get(id)?;
        if !projection.log.is_persisted() {
            self.persist_buffered(id, &projection)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn all(&self) -> Vec<SessionProjection> {
        self.residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .values()
            .cloned()
            .collect()
    }

    pub fn all_snapshots(&self) -> Vec<SessionProjection> {
        self.refresh_discovered();
        let ids = {
            let residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            residency
                .resident
                .keys()
                .chain(residency.evicted.keys())
                .copied()
                .collect::<HashSet<_>>()
        };
        ids.into_iter()
            .filter_map(|id| match self.get(id) {
                Ok(session) => Some(session),
                Err(error) => {
                    eprintln!("session {id} snapshot skipped: {error}");
                    None
                }
            })
            .collect()
    }
    #[must_use]
    pub fn all_summaries(&self) -> Vec<SessionSummary> {
        self.refresh_discovered();
        let residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut summaries = residency.evicted.clone();
        summaries.extend(residency.resident.iter().map(|(session_id, session)| {
            (
                *session_id,
                SessionSummary {
                    meta: session.meta.clone(),
                    usage: session.usage.clone(),
                    usage_rollup: session.usage_rollup.clone(),
                    agent_usage: session.agent_usage.clone(),
                },
            )
        }));
        summaries.into_values().collect()
    }

    pub fn summary(&self, id: SessionId) -> Result<SessionSummary, SessionError> {
        {
            let residency = self
                .residency
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(session) = residency.resident.get(&id) {
                return Ok(summary_from_projection(session));
            }
            if let Some(summary) = residency.evicted.get(&id) {
                return Ok(summary.clone());
            }
        }
        self.get(id)
            .map(|session| summary_from_projection(&session))
    }

    fn refresh_discovered(&self) {
        let entries = match fs::read_dir(&self.sessions_dir) {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("session discovery failed: {error}");
                return;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            if !entry.path().is_dir() {
                continue;
            }
            let Ok(id) = entry.file_name().to_string_lossy().parse::<SessionId>() else {
                continue;
            };
            let known = {
                let residency = self
                    .residency
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                residency.resident.contains_key(&id) || residency.evicted.contains_key(&id)
            };
            if known {
                continue;
            }
            // Invalid entries stay uncached so later discovery retries them and repeats the
            // diagnostic after callers have had a chance to repair the files.
            match read_cache(
                &entry.path().join("meta.json"),
                &entry.path().join("events.jsonl"),
            ) {
                Ok(meta) if meta.session_id == id => {
                    self.residency
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .evicted
                        .entry(id)
                        .or_insert(SessionSummary {
                            meta,
                            usage: None,
                            usage_rollup: UsageRollup::default(),
                            agent_usage: BTreeMap::new(),
                        });
                }
                Ok(_) => eprintln!("session {id} metadata ID does not match its directory"),
                Err(error) => eprintln!("session {id} metadata skipped: {error}"),
            }
        }
    }

    pub fn session_usage(
        &self,
        id: SessionId,
        pricing: &cookie_agent_config::PricingConfig,
        catalog: &BTreeMap<
            cookie_agent_protocol::ModelKey,
            cookie_agent_models::catalog::CatalogModelCost,
        >,
    ) -> Result<cookie_agent_protocol::SessionUsageResult, SessionError> {
        let usage = self.summary(id)?.usage_rollup;
        Ok(cookie_agent_protocol::SessionUsageResult {
            session_id: id,
            usage: crate::usage::with_pricing(usage, pricing, catalog),
        })
    }

    #[must_use]
    pub fn is_resident(&self, id: SessionId) -> bool {
        self.residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .contains_key(&id)
    }

    #[must_use]
    pub fn resident_subagent_count(&self) -> usize {
        self.residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resident
            .values()
            .filter(|session| matches!(session.meta.origin, SessionOrigin::Delegated { .. }))
            .count()
    }
    #[must_use]
    pub fn project_dir_path(&self) -> &Path {
        &self.project_dir
    }
    #[must_use]
    pub(crate) fn sessions_dir_path(&self) -> &Path {
        &self.sessions_dir
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
        let root = self.get(id)?;
        let mut sessions = self.all();
        if !sessions.iter().any(|session| session.meta.session_id == id) {
            sessions.push(root);
        }
        let mut metadata = HashMap::with_capacity(sessions.len());
        let mut children = HashMap::<SessionId, Vec<SessionId>>::new();
        for session in sessions {
            let meta = session.metadata();
            if let SessionOrigin::Delegated {
                parent_session_id, ..
            } = meta.origin
            {
                children
                    .entry(parent_session_id)
                    .or_default()
                    .push(meta.session_id);
            }
            metadata.insert(meta.session_id, meta);
        }

        fn build_tree(
            id: SessionId,
            metadata: &HashMap<SessionId, SessionMeta>,
            children: &HashMap<SessionId, Vec<SessionId>>,
        ) -> Result<SessionTree, SessionError> {
            Ok(SessionTree {
                session: metadata
                    .get(&id)
                    .cloned()
                    .ok_or(SessionError::Missing(id))?,
                children: children
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .map(|child| build_tree(*child, metadata, children))
                    .collect::<Result<Vec<_>, _>>()?,
            })
        }

        build_tree(id, &metadata, &children)
    }

    pub(crate) fn release_ownership(&self) {
        self.closed.store(true, Ordering::Release);
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let residency = self
            .residency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for session in residency.resident.values() {
            let _ = session.log.suspend_writer();
        }
        self.ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    fn ensure_open(&self) -> Result<(), SessionError> {
        if self.closed.load(Ordering::Acquire) {
            Err(SessionError::StoreClosed)
        } else {
            Ok(())
        }
    }
}

impl Drop for SessionStore {
    fn drop(&mut self) {
        self.release_ownership();
    }
}

pub(crate) fn projection(log: Arc<EventLog>) -> Result<SessionProjection, SessionError> {
    let events = log.event_snapshot();
    let physical_tip = log.last_event().expect("creation checked by EventLog");
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
        last_event_seq: log.physical_tip_seq(),
        last_activity: physical_tip.timestamp,
        status: SessionStatus::Idle,
        skipped_events: log
            .diagnostics()
            .iter()
            .filter(|diagnostic| diagnostic.skipped)
            .map(|diagnostic| cookie_agent_protocol::SkippedEvent {
                seq: diagnostic.seq,
                reason: diagnostic.reason.clone(),
            })
            .collect(),
    };
    let mut runs = HashMap::<RunId, RunProjection>::new();
    let mut status = SessionStatus::Idle;
    let mut usage = None;
    let mut usage_rollup = UsageRollup::default();
    let mut agent_usage = BTreeMap::<AgentId, UsageRollup>::new();
    let mut rename_records = HashMap::new();
    let mut permission_overlay = SessionPermissionOverlay::default();
    let mut automatic_title = None;
    let mut delegated_title = None;
    let mut user_title: Option<Option<cookie_agent_protocol::SessionTitle>> = None;
    let recorded_usage_turns = events
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::ModelUsageRecorded { model_turn_seq, .. } => Some(model_turn_seq),
            _ => None,
        })
        .collect::<HashSet<_>>();
    for envelope in events.iter() {
        if let EventPayload::SessionPermissionOverlaySet { overlay } = &envelope.payload {
            permission_overlay = overlay.clone();
        }
        if let EventPayload::SessionTitleCommitted { change, .. } = &envelope.payload {
            match change {
                SessionTitleChange::UserSet { title, .. } => {
                    user_title = Some(Some(title.clone()));
                }
                SessionTitleChange::UserClear { .. } => user_title = Some(None),
                SessionTitleChange::UserReset { .. } => user_title = None,
                SessionTitleChange::DelegatedSet { title, .. } => {
                    delegated_title = Some(title.clone());
                }
                SessionTitleChange::InternalAgentSet { title, .. }
                | SessionTitleChange::FallbackSet { title } => {
                    automatic_title = Some(title.clone());
                }
            }
            meta.title = user_title
                .clone()
                .unwrap_or_else(|| delegated_title.clone().or_else(|| automatic_title.clone()));
            meta.title_updated_seq = envelope.seq;
            if let Some(record) = change.user_rename_record() {
                rename_records.insert(record.client_rename_id.clone(), record);
            }
        }
        if let EventPayload::DelegateChildTerminated {
            status: terminal, ..
        } = &envelope.payload
        {
            status = *terminal;
            continue;
        }
        if matches!(envelope.payload, EventPayload::SessionReverted { .. }) {
            status = SessionStatus::Idle;
            for run in runs.values_mut() {
                if run.status == SessionStatus::Running {
                    run.status = SessionStatus::Interrupted;
                    run.pending_calls.clear();
                }
            }
            continue;
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
            EventPayload::ModelTurnCommitted {
                model_turn_seq,
                resolved_model,
                turn,
                ..
            } => {
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
                if !recorded_usage_turns.contains(model_turn_seq) {
                    crate::usage::record_stamped(&mut usage_rollup, resolved_model, reported, None);
                    if let Some(agent) = runs.get(&run_id).map(|run| run.agent.agent.clone()) {
                        crate::usage::record_stamped(
                            agent_usage.entry(agent).or_default(),
                            resolved_model,
                            reported,
                            None,
                        );
                    }
                }
            }
            EventPayload::ModelUsageRecorded {
                agent_id,
                resolved_model,
                usage: reported,
                estimated_cost_pico_usd,
                ..
            } => {
                crate::usage::record_stamped(
                    &mut usage_rollup,
                    resolved_model,
                    reported,
                    *estimated_cost_pico_usd,
                );
                crate::usage::record_stamped(
                    agent_usage.entry(agent_id.clone()).or_default(),
                    resolved_model,
                    reported,
                    *estimated_cost_pico_usd,
                );
            }
            EventPayload::InternalAgentUsageRecorded {
                agent_id,
                resolved_model,
                usage: reported,
                estimated_cost_pico_usd,
                ..
            } => {
                crate::usage::record_stamped(
                    &mut usage_rollup,
                    resolved_model,
                    reported,
                    *estimated_cost_pico_usd,
                );
                crate::usage::record_stamped(
                    agent_usage.entry(agent_id.clone()).or_default(),
                    resolved_model,
                    reported,
                    *estimated_cost_pico_usd,
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
        usage_rollup,
        agent_usage,
        runs,
        rename_records,
        permission_overlay,
        log,
    })
}

fn summary_from_projection(session: &SessionProjection) -> SessionSummary {
    SessionSummary {
        meta: session.meta.clone(),
        usage: session.usage.clone(),
        usage_rollup: session.usage_rollup.clone(),
        agent_usage: session.agent_usage.clone(),
    }
}

fn fork_title(title: Option<&SessionTitle>) -> Result<SessionTitle, SessionError> {
    const SUFFIX: &str = " (fork)";
    let base = title.map_or("Untitled", SessionTitle::as_str);
    let max_base = SessionTitle::MAX_BYTES.saturating_sub(SUFFIX.len());
    let mut boundary = base.len().min(max_base);
    while !base.is_char_boundary(boundary) {
        boundary -= 1;
    }
    SessionTitle::new(format!("{}{SUFFIX}", &base[..boundary]))
        .map_err(|error| SessionError::InvalidForkTitle(error.to_string()))
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
    events.iter().rev().find_map(|event| match &event.payload {
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
    let persisted = serde_json::to_value(cache).map_err(|source| SessionError::Json {
        path: path.to_owned(),
        source,
    })?;
    let bytes = serde_json::to_vec_pretty(&persisted).map_err(|source| SessionError::Json {
        path: path.to_owned(),
        source,
    })?;
    let parent = path.parent().expect("session cache has a parent");
    let temporary = parent.join(format!(".meta.json.{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<(), SessionError> {
        #[cfg(unix)]
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|source| SessionError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|source| SessionError::Io {
                    path: temporary.clone(),
                    source,
                })?;
            drop(file);
            fs::rename(&temporary, path).map_err(|source| SessionError::Io {
                path: path.to_owned(),
                source,
            })?;
            fsync_directory(parent)?;
        }
        #[cfg(windows)]
        {
            let mut file =
                cookie_agent_models::secure_store::create_windows_private_file(&temporary)
                    .map_err(|source| SessionError::Io {
                        path: temporary.clone(),
                        source,
                    })?;
            file.write_all(&bytes).map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
            file.sync_all().map_err(|source| SessionError::Io {
                path: temporary.clone(),
                source,
            })?;
            drop(file);
            replace_windows_path_with_retry(&temporary, path).map_err(|source| {
                SessionError::Io {
                    path: path.to_owned(),
                    source,
                }
            })?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn replace_windows_path_with_retry(source: &Path, target: &Path) -> std::io::Result<()> {
    const ATTEMPTS: usize = 50;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(25);

    for attempt in 0..ATTEMPTS {
        match cookie_agent_models::secure_store::replace_windows_path(source, target) {
            Ok(()) => return Ok(()),
            Err(error) if attempt + 1 < ATTEMPTS && windows_replace_is_contended(&error) => {
                std::thread::sleep(BACKOFF);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("path replacement attempts are nonzero")
}

#[cfg(windows)]
fn windows_replace_is_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(error.raw_os_error(), Some(5 | 32))
}

fn read_cache(path: &Path, events_path: &Path) -> Result<SessionMeta, SessionError> {
    let bytes = fs::read(path).map_err(|source| SessionError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut value = serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|source| {
        SessionError::Json {
            path: path.to_owned(),
            source,
        }
    })?;
    if value.get("last_activity").is_none() {
        let modified = fs::metadata(events_path)
            .and_then(|metadata| metadata.modified())
            .map_err(|source| SessionError::Io {
                path: events_path.to_owned(),
                source,
            })?;
        let timestamp =
            jiff::Timestamp::try_from(modified).unwrap_or_else(|_| jiff::Timestamp::now());
        value
            .as_object_mut()
            .ok_or_else(|| SessionError::Json {
                path: path.to_owned(),
                source: serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "session metadata is not an object",
                )),
            })?
            .insert(
                "last_activity".into(),
                serde_json::to_value(timestamp).expect("timestamp serializes"),
            );
    }
    serde_json::from_value(value).map_err(|source| SessionError::Json {
        path: path.to_owned(),
        source,
    })
}

#[cfg(unix)]
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

#[cfg(windows)]
fn write_project_cwd(project_dir: &Path, cwd: &Path) -> Result<(), SessionError> {
    let Ok(canonical) = cwd.canonicalize() else {
        return Ok(());
    };
    let bytes = canonical.as_os_str().as_encoded_bytes();
    let path = project_dir.join(PROJECT_CWD_FILE);
    if project_cwd_is_current(&path, bytes) {
        return Ok(());
    }
    let temporary = project_dir.join(format!(".{PROJECT_CWD_FILE}.{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<(), SessionError> {
        let mut file = cookie_agent_models::secure_store::create_windows_private_file(&temporary)
            .map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.write_all(bytes).map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
        drop(file);
        replace_windows_path_with_retry(&temporary, &path).map_err(|source| SessionError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn project_cwd_is_current(path: &Path, expected: &[u8]) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == expected)
}

#[cfg(windows)]
fn project_cwd_is_current(path: &Path, expected: &[u8]) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == expected)
}

#[cfg(unix)]
fn create_unix_session_directory_all(path: &Path) -> Result<(), SessionError> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path).map_err(|source| SessionError::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(windows)]
fn create_windows_session_directory(path: &Path) -> Result<(), SessionError> {
    cookie_agent_models::secure_store::SecureDirectory::open(path)
        .map(|_| ())
        .map_err(|error| match error {
            cookie_agent_models::secure_store::SecureStoreError::Io(source) => SessionError::Io {
                path: path.to_owned(),
                source,
            },
            error => SessionError::Io {
                path: path.to_owned(),
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, error),
            },
        })
}

#[cfg(windows)]
fn create_windows_session_file(path: &Path) -> Result<(), SessionError> {
    cookie_agent_models::secure_store::create_windows_private_file(path)
        .map(drop)
        .map_err(|source| SessionError::Io {
            path: path.to_owned(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::Path,
        sync::{Arc, mpsc},
        thread,
    };

    #[cfg(unix)]
    use std::{
        ffi::OsString,
        os::unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
        },
    };

    use cookie_agent_config::{ModelPricing, PicoUsdPerMillion, PricingConfig};
    use cookie_agent_protocol::{
        AgentId, AgentMode, AgentRevision, AttemptId, CatalogRevision, ClientRunId, EventPayload,
        InternalAgentBackend, InternalAgentFailure, InternalAgentInvocationId, InternalAgentKind,
        InternalAgentRunId, ModelFinishReason, ModelRevision, PersistedModelTurn,
        ProviderStateRevision, RecipeRegistryRevision, RunId, RuntimeRevision, SafeCode,
        SafeDisplayText, SafeErrorMessage, SafeInternalAgentCall, SafeInternalAgentResult,
        SessionId, SessionOrigin, Sha256Digest, Usage,
    };

    use crate::ownership::owner_lock_path;

    use super::{PROJECT_CWD_FILE, SessionError, SessionStore, projection};

    #[cfg(unix)]
    fn cwd_file(data_root: &Path, cwd: &Path) -> std::path::PathBuf {
        SessionStore::project_dir(data_root, cwd).join(PROJECT_CWD_FILE)
    }

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temporary root");
        #[cfg(unix)]
        {
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private temporary root");
        }
        #[cfg(windows)]
        {
            fs::remove_dir(directory.path()).expect("remove ordinary temp directory");
            cookie_agent_models::secure_store::SecureDirectory::open(directory.path())
                .expect("private temporary root");
        }
        directory
    }

    fn create_private_test_dir_all(path: &Path) {
        #[cfg(unix)]
        {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(path).expect("private test directory");
        }
        #[cfg(windows)]
        cookie_agent_models::secure_store::create_windows_private_dir_all(path)
            .expect("private test directory");
    }

    fn write_private_test_file(path: &Path, contents: impl AsRef<[u8]>) {
        #[cfg(unix)]
        {
            use std::io::Write as _;

            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .expect("private test file");
            file.write_all(contents.as_ref())
                .expect("write private test file");
        }
        #[cfg(windows)]
        {
            use std::io::Write as _;

            let mut file = cookie_agent_models::secure_store::create_windows_private_file(path)
                .expect("private test file");
            file.write_all(contents.as_ref())
                .expect("write private test file");
        }
    }

    fn persist_test_session(store: &SessionStore) -> SessionId {
        let session_id = SessionId::new_v7();
        let agent = crate::test_support::agent_snapshot("test", AgentMode::Primary);
        let selection = crate::test_support::run_selection("test");
        let binding = agent.fallback_chain[0].clone();
        let revision = |label: char| format!("sha256:{}", label.to_string().repeat(64));
        let runtime_revision = RuntimeRevision::new(revision('1')).unwrap();
        let catalog_revision = CatalogRevision::new(revision('2')).unwrap();
        let provider_state_revision = ProviderStateRevision::new(revision('3')).unwrap();
        let model_revision = ModelRevision::new(revision('4')).unwrap();
        let agent_revision = AgentRevision::new(revision('5')).unwrap();
        let recipe_registry_revision = RecipeRegistryRevision::new(revision('6')).unwrap();
        store
            .create(
                session_id,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionCreated {
                    origin: SessionOrigin::Root,
                    cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test")
                        .unwrap(),
                    creation_selection: selection.clone(),
                    creation_agent: Box::new(agent.clone()),
                    runtime_revision: runtime_revision.clone(),
                    catalog_revision: catalog_revision.clone(),
                    provider_state_revision: provider_state_revision.clone(),
                    model_revision: model_revision.clone(),
                    agent_revision: agent_revision.clone(),
                    recipe_registry_revision: recipe_registry_revision.clone(),
                    manifest_revision: binding.manifest_revision.clone(),
                },
            )
            .unwrap();
        let run_id = RunId::new_v7();
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::RunStarted {
                    client_run_id: ClientRunId::new("private-session-test").unwrap(),
                    selection,
                    agent: Box::new(agent),
                    runtime_revision,
                    catalog_revision,
                    provider_state_revision,
                    model_revision,
                    agent_revision,
                    recipe_registry_revision,
                    manifest_revision: binding.manifest_revision.clone(),
                    selected_suffix: vec![binding],
                    internal_agents: Vec::new(),
                    input_through_seq: 1,
                },
            )
            .unwrap();
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::UserInputSubmitted {
                    input: "persist me".into(),
                },
            )
            .unwrap();
        session_id
    }

    fn create_buffered_test_session(store: &SessionStore) -> SessionId {
        let session_id = SessionId::new_v7();
        let agent = crate::test_support::agent_snapshot("test", AgentMode::Primary);
        let selection = crate::test_support::run_selection("test");
        let binding = agent.fallback_chain[0].clone();
        let revision = |label: char| format!("sha256:{}", label.to_string().repeat(64));
        store
            .create(
                session_id,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionCreated {
                    origin: SessionOrigin::Root,
                    cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test")
                        .unwrap(),
                    creation_selection: selection,
                    creation_agent: Box::new(agent),
                    runtime_revision: RuntimeRevision::new(revision('1')).unwrap(),
                    catalog_revision: CatalogRevision::new(revision('2')).unwrap(),
                    provider_state_revision: ProviderStateRevision::new(revision('3')).unwrap(),
                    model_revision: ModelRevision::new(revision('4')).unwrap(),
                    agent_revision: AgentRevision::new(revision('5')).unwrap(),
                    recipe_registry_revision: RecipeRegistryRevision::new(revision('6')).unwrap(),
                    manifest_revision: binding.manifest_revision,
                },
            )
            .expect("create buffered session");
        session_id
    }

    #[test]
    fn ownership_is_acquired_on_write_open_and_released_with_the_store() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        assert!(owner_lock_path(&owner.session_dir(session_id)).is_file());
        let stale_log = owner.get(session_id).expect("owned projection").log;
        let (authorized, release_append) = stale_log.install_append_authorization_hook_for_test();
        let appending = thread::spawn(move || {
            stale_log.append(
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            )
        });
        authorized
            .recv()
            .expect("append passed initial authorization");

        assert_eq!(Arc::strong_count(&owner), 1);
        drop(owner);
        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        observer
            .open_for_write(session_id)
            .expect("adopt after owner drops");
        assert!(observer.is_owned(session_id));
        release_append.send(()).expect("release stale append");
        assert!(matches!(
            appending.join().expect("stale append thread"),
            Err(crate::events::EventLogError::ReadOnly(_))
        ));
    }

    #[test]
    fn ownership_release_does_not_wait_for_store_drop() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        let stale_log = owner.get(session_id).expect("owned projection").log;

        owner.release_ownership();
        owner.release_ownership();

        assert!(!owner.is_owned(session_id));
        assert!(matches!(
            owner.append(
                session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            ),
            Err(SessionError::StoreClosed)
        ));
        assert!(matches!(
            stale_log.append(
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            ),
            Err(crate::events::EventLogError::ReadOnly(_))
        ));
        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        observer
            .open_for_write(session_id)
            .expect("adopt after explicit ownership release");
    }

    #[test]
    fn ownership_release_waits_for_an_append_that_already_won_serialization() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        let log = owner.get(session_id).expect("owned projection").log;
        let (authorized, release_append) = log.install_append_authorization_hook_for_test();
        let append_store = Arc::clone(&owner);
        let appending = thread::spawn(move || {
            append_store.append(
                session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            )
        });
        authorized.recv().expect("append passed authorization");

        let release_store = Arc::clone(&owner);
        let (released, release_observed) = mpsc::channel();
        let releasing = thread::spawn(move || {
            release_store.release_ownership();
            released.send(()).expect("report ownership release");
        });
        assert!(matches!(
            release_observed.recv_timeout(std::time::Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        assert!(matches!(
            observer.open_for_write(session_id),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ));

        release_append.send(()).expect("release append");
        appending
            .join()
            .expect("append thread")
            .expect("append wins");
        release_observed
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("ownership releases after append");
        releasing.join().expect("release thread");
        observer
            .open_for_write(session_id)
            .expect("adopt after append and release");
    }

    #[test]
    fn eviction_retains_ownership_and_reopens_for_the_owner() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        assert!(owner.evict(session_id).expect("evict owned session"));
        assert!(!owner.is_resident(session_id));

        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        let snapshot = observer.get(session_id).expect("read-only snapshot");
        assert!(matches!(
            snapshot.log.append(
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            ),
            Err(crate::events::EventLogError::ReadOnly(_))
        ));
        assert!(matches!(
            observer.open_for_write(session_id),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ));
        owner
            .open_for_write(session_id)
            .expect("owner reopens after eviction");
    }

    #[test]
    fn foreign_snapshot_preserves_torn_tail_until_owned_adoption() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        let event_path = owner.session_dir(session_id).join("events.jsonl");
        let mut bytes = fs::read(&event_path).expect("read event log");
        bytes.extend_from_slice(b"{\"torn\"");
        fs::write(&event_path, &bytes).expect("write torn tail");

        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        observer.get(session_id).expect("read foreign snapshot");
        assert_eq!(fs::read(&event_path).expect("tail remains"), bytes);
        assert!(matches!(
            observer.open_for_write(session_id),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ));

        drop(owner);
        observer.open_for_write(session_id).expect("adopt torn log");
        assert_ne!(fs::read(&event_path).expect("tail truncated"), bytes);
        assert!(
            fs::read(&event_path)
                .expect("read repaired log")
                .ends_with(b"\n")
        );
    }

    #[test]
    fn failed_adoption_is_unobservable_and_retryable() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        assert_eq!(Arc::strong_count(&owner), 1);
        drop(owner);

        let first = SessionStore::open(&data, &cwd).expect("first adopter");
        assert_eq!(
            first.begin_write(session_id).unwrap(),
            super::WriteOpen::Adopting
        );
        assert!(!first.is_owned(session_id));
        assert!(matches!(
            first.append(
                session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionReverted { through_seq: 1 },
            ),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ));
        first.rollback_adoption(session_id);

        let second = SessionStore::open(&data, &cwd).expect("second adopter");
        assert_eq!(
            second.begin_write(session_id).unwrap(),
            super::WriteOpen::Adopting
        );
        second.commit_adoption(session_id).expect("commit retry");
        assert!(second.is_owned(session_id));
    }

    #[test]
    fn concurrent_adoption_has_one_winner() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        drop(owner);
        let stores = [
            SessionStore::open(&data, &cwd).expect("first contender"),
            SessionStore::open(&data, &cwd).expect("second contender"),
        ];
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let threads = stores.clone().map(|store| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                match store.begin_write(session_id) {
                    Ok(super::WriteOpen::Adopting) => {
                        store.commit_adoption(session_id).expect("commit winner");
                        true
                    }
                    Err(SessionError::SessionLocked(id)) if id == session_id => false,
                    result => panic!("unexpected adoption result: {result:?}"),
                }
            })
        });
        barrier.wait();
        let winners = threads
            .into_iter()
            .map(|thread| usize::from(thread.join().expect("adoption contender")))
            .sum::<usize>();
        assert_eq!(winners, 1);
    }

    #[test]
    fn buffered_publish_is_locked_before_the_directory_becomes_visible() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let creator = SessionStore::open(&data, &cwd).expect("creator store");
        let session_id = create_buffered_test_session(&creator);
        let (reached, release) = creator.install_publish_hook_for_test();
        let publishing = {
            let creator = Arc::clone(&creator);
            thread::spawn(move || creator.persist_buffered_session(session_id))
        };
        assert_eq!(
            reached.recv().expect("publisher acquired ownership lock"),
            session_id
        );

        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        assert!(
            matches!(observer.get(session_id), Err(SessionError::Missing(id)) if id == session_id)
        );
        release.send(()).expect("release publisher");
        publishing
            .join()
            .expect("publisher thread")
            .expect("publish session");
        assert!(matches!(
            observer.open_for_write(session_id),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ));
    }

    #[test]
    fn fork_publish_is_locked_before_the_directory_becomes_visible() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let creator = SessionStore::open(&data, &cwd).expect("creator store");
        let source_id = persist_test_session(&creator);
        let through_seq = creator
            .get(source_id)
            .expect("source projection")
            .log
            .events()
            .into_iter()
            .find(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
            .expect("source user input")
            .seq;
        let (reached, release) = creator.install_publish_hook_for_test();
        let publishing = {
            let creator = Arc::clone(&creator);
            thread::spawn(move || {
                creator.fork(
                    source_id,
                    through_seq,
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                )
            })
        };
        let fork_id = reached.recv().expect("fork acquired ownership lock");

        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        assert!(matches!(observer.get(fork_id), Err(SessionError::Missing(id)) if id == fork_id));
        release.send(()).expect("release fork publisher");
        assert_eq!(
            publishing
                .join()
                .expect("fork publisher thread")
                .expect("publish fork"),
            fork_id
        );
        assert!(matches!(
            observer.open_for_write(fork_id),
            Err(SessionError::SessionLocked(id)) if id == fork_id
        ));
    }

    #[test]
    fn metadata_cache_reads_never_observe_partial_replacements() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).expect("session store");
        let session_id = persist_test_session(&store);
        let session_dir = store.session_dir(session_id);
        let cache_path = session_dir.join("meta.json");
        let event_path = session_dir.join("events.jsonl");
        let meta = store.get(session_id).expect("projection").meta;
        let writer = thread::spawn({
            let cache_path = cache_path.clone();
            let meta = meta.clone();
            move || {
                for _ in 0..100 {
                    super::write_cache(&cache_path, &meta).expect("replace metadata cache");
                }
            }
        });
        for _ in 0..100 {
            let read = super::read_cache(&cache_path, &event_path).expect("read complete cache");
            assert_eq!(read.session_id, session_id);
        }
        writer.join().expect("metadata writer");
    }

    #[test]
    fn discovery_does_not_reread_known_evicted_metadata() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let owner = SessionStore::open(&data, &cwd).expect("owner store");
        let session_id = persist_test_session(&owner);
        drop(owner);

        let observer = SessionStore::open(&data, &cwd).expect("observer store");
        let cached = observer.summary(session_id).expect("cached summary");
        assert!(!observer.is_resident(session_id));
        let mut replacement = cached.meta.clone();
        replacement.title = Some(
            cookie_agent_protocol::SessionTitle::new("changed on disk").expect("replacement title"),
        );
        super::write_cache(
            &observer.session_dir(session_id).join("meta.json"),
            &replacement,
        )
        .expect("replace metadata cache");

        let rediscovered = observer
            .all_summaries()
            .into_iter()
            .find(|summary| summary.meta.session_id == session_id)
            .expect("rediscovered summary");
        assert_eq!(rediscovered.meta.title, cached.meta.title);
    }

    fn append_pending_test_delta(
        store: &SessionStore,
        session_id: SessionId,
        text: &str,
    ) -> (
        Arc<crate::events::EventLog>,
        cookie_agent_protocol::StoredEvent,
    ) {
        let projection = store.get(session_id).expect("session projection");
        let (run_id, resolved_model, prompt_fingerprint) = projection
            .log
            .events()
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::RunStarted {
                    agent,
                    selected_suffix,
                    ..
                } => Some((
                    event.run_id.expect("run id"),
                    crate::model_history::wire_model(
                        selected_suffix.first().expect("selected model"),
                    ),
                    agent.prompt_fingerprint.clone(),
                )),
                _ => None,
            })
            .expect("run event");
        let attempt_id = AttemptId::new_v7();
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::ModelAttemptStarted {
                    attempt_id,
                    attempt_ordinal: 1,
                    fallback_index: 0,
                    retry_ordinal: 0,
                    resolved_model,
                    prompt_fingerprint,
                },
            )
            .expect("start attempt");
        let log = store.get(session_id).expect("session projection").log;
        log.pause_background_sync_for_test();
        let delta = store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::TextDelta {
                    attempt_id,
                    text: text.into(),
                },
            )
            .expect("append buffered delta");
        (log, delta)
    }

    #[test]
    fn eviction_waits_for_pending_stream_records_to_sync() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let store = SessionStore::open(&temporary.path().join("data"), &cwd).unwrap();
        let session_id = persist_test_session(&store);
        let (log, _) = append_pending_test_delta(&store, session_id, "durable before eviction");
        let (sync_reached, release_sync) = log.install_sync_hook_for_test();
        let (eviction_done, eviction_result) = mpsc::channel();
        let evicting = {
            let store = store.clone();
            thread::spawn(move || {
                eviction_done
                    .send(store.evict(session_id))
                    .expect("report eviction result");
            })
        };

        sync_reached.recv().expect("eviction reached pending sync");
        assert!(matches!(
            eviction_result.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release_sync.send(()).expect("release eviction sync");
        assert!(
            eviction_result
                .recv()
                .expect("receive eviction result")
                .expect("evict session")
        );
        evicting.join().expect("eviction thread");

        let durable = crate::events::load_jsonl::<cookie_agent_protocol::StoredEvent>(
            &store.session_dir(session_id).join("events.jsonl"),
        )
        .expect("read evicted event log");
        assert!(durable.iter().any(|event| matches!(
            &event.payload,
            EventPayload::TextDelta { text, .. } if text == "durable before eviction"
        )));
    }

    #[test]
    fn fork_flushes_pending_source_records_before_copying() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let store = SessionStore::open(&temporary.path().join("data"), &cwd).unwrap();
        let session_id = persist_test_session(&store);
        let (log, delta) = append_pending_test_delta(&store, session_id, "copied after sync");
        assert!(log.writer_is_open_for_test());
        let delta_seq = delta.seq;
        let run_id = delta.run_id.expect("delta run id");
        let EventPayload::TextDelta { attempt_id, .. } = delta.payload else {
            panic!("pending event is a text delta")
        };
        let (sync_reached, release_sync) = log.install_sync_hook_for_test();
        let (fork_done, fork_result) = mpsc::channel();
        let forking = {
            let store = store.clone();
            thread::spawn(move || {
                fork_done
                    .send(store.fork(
                        session_id,
                        delta_seq,
                        cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
                    ))
                    .expect("report fork result");
            })
        };

        sync_reached.recv().expect("fork reached source sync");
        assert!(matches!(
            fork_result.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release_sync.send(()).expect("release fork sync");
        let fork_id = fork_result
            .recv()
            .expect("receive fork result")
            .expect("fork session");
        forking.join().expect("fork thread");
        assert!(!log.writer_is_open_for_test());

        let copied = store.get(fork_id).expect("fork projection").log.events();
        assert!(copied.iter().any(|event| matches!(
            &event.payload,
            EventPayload::TextDelta { text, .. } if text == "copied after sync"
        )));
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::ReasoningDelta {
                    attempt_id,
                    text: "reopened after fork".into(),
                },
            )
            .expect("append after suspended fork source");
        assert!(log.writer_is_open_for_test());
        log.flush().expect("flush reopened source writer");
    }

    #[cfg(unix)]
    #[test]
    fn unix_buffered_session_is_private_at_creation() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).unwrap();
        let session_id = persist_test_session(&store);
        let project = store.project_dir_path();
        let session = store.session_dir(session_id);

        for path in [
            data.clone(),
            data.join("projects"),
            project.to_owned(),
            project.join("sessions"),
            session.clone(),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for path in [
            project.join(PROJECT_CWD_FILE),
            session.join("events.jsonl"),
            session.join("meta.json"),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_session_reuses_preexisting_loose_modes() {
        let temporary = private_tempdir();
        let cwd = temporary.path().join("workspace");
        create_private_test_dir_all(&cwd);
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).unwrap();
        let session_id = persist_test_session(&store);
        let project = store.project_dir_path().to_owned();
        let session = store.session_dir(session_id);
        let directories = [
            data.clone(),
            data.join("projects"),
            project.clone(),
            project.join("sessions"),
            session.clone(),
        ];
        let files = [
            project.join(PROJECT_CWD_FILE),
            session.join("events.jsonl"),
            session.join("meta.json"),
        ];
        for path in &directories {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for path in &files {
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        drop(store);

        let reopened = SessionStore::open(&data, &cwd).unwrap();
        reopened.get(session_id).expect("loose existing session");
        for path in directories {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        for path in files {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }
    }

    // Requires Unix symlink semantics and exact raw path bytes.
    #[cfg(unix)]
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

    // Requires constructing and comparing non-UTF8 Unix path bytes.
    #[cfg(unix)]
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

    // Existing state is reused without permission repair.
    #[cfg(unix)]
    #[test]
    fn cwd_file_is_private_at_creation_and_loose_mode_is_reused() {
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
        assert_eq!(fs::metadata(path).expect("metadata").mode() & 0o7777, 0o644);
    }

    // Verifies atomic replacement using Unix inode identity and mode bits.
    #[cfg(unix)]
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

    // Verifies retention using Unix inode identity.
    #[cfg(unix)]
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
        let temp = private_tempdir();
        let data = temp.path().join("data");
        let project = SessionStore::project_dir(&data, temp.path());
        assert_eq!(
            project.file_name().expect("hash").to_string_lossy().len(),
            16
        );
        create_private_test_dir_all(&project.join("sessions"));
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

    #[test]
    fn replayed_stamps_keep_footer_and_session_cost_equal_across_pricing_changes() {
        let temp = private_tempdir();
        let path = temp.path().join("events.jsonl");
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let attempt_id = AttemptId::new_v7();
        let agent = crate::test_support::agent_snapshot("test", AgentMode::Primary);
        let selection = crate::test_support::run_selection("test");
        let binding = agent.fallback_chain[0].clone();
        let resolved_model = crate::model_history::wire_model(&binding);
        let model_key = resolved_model.selection.model.clone();
        let runtime_revision = RuntimeRevision::new(format!("sha256:{}", "1".repeat(64))).unwrap();
        let catalog_revision = CatalogRevision::new(format!("sha256:{}", "2".repeat(64))).unwrap();
        let provider_state_revision =
            ProviderStateRevision::new(format!("sha256:{}", "3".repeat(64))).unwrap();
        let model_revision = ModelRevision::new(format!("sha256:{}", "4".repeat(64))).unwrap();
        let agent_revision = AgentRevision::new(format!("sha256:{}", "5".repeat(64))).unwrap();
        let recipe_registry_revision =
            RecipeRegistryRevision::new(format!("sha256:{}", "6".repeat(64))).unwrap();
        let log = crate::events::EventLog::create(
            path.clone(),
            session_id,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionCreated {
                origin: SessionOrigin::Root,
                cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test").unwrap(),
                creation_selection: selection.clone(),
                creation_agent: Box::new(agent.clone()),
                runtime_revision: runtime_revision.clone(),
                catalog_revision: catalog_revision.clone(),
                provider_state_revision: provider_state_revision.clone(),
                model_revision: model_revision.clone(),
                agent_revision: agent_revision.clone(),
                recipe_registry_revision: recipe_registry_revision.clone(),
                manifest_revision: binding.manifest_revision.clone(),
            },
        )
        .unwrap();
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::RunStarted {
                client_run_id: ClientRunId::new("usage-replay").unwrap(),
                selection,
                agent: Box::new(agent.clone()),
                runtime_revision,
                catalog_revision,
                provider_state_revision,
                model_revision,
                agent_revision,
                recipe_registry_revision,
                manifest_revision: binding.manifest_revision.clone(),
                selected_suffix: vec![binding],
                internal_agents: Vec::new(),
                input_through_seq: 1,
            },
        )
        .unwrap();
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::UserInputSubmitted {
                input: "question".into(),
            },
        )
        .unwrap();
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_model.clone(),
                prompt_fingerprint: agent.prompt_fingerprint.clone(),
            },
        )
        .unwrap();
        let usage = Usage {
            input_tokens: Some(120),
            input_tokens_no_cache: Some(70),
            input_tokens_cache_read: Some(40),
            input_tokens_cache_write: Some(10),
            output_tokens: Some(30),
            ..Usage::default()
        };
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ModelTurnCommitted {
                attempt_id,
                model_turn_seq: 1,
                resolved_model: resolved_model.clone(),
                input_through_seq: 1,
                turn: PersistedModelTurn {
                    content: Vec::new(),
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::Stop,
                    usage: usage.clone(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        )
        .unwrap();
        let usage_event = log
            .append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::ModelUsageRecorded {
                    model_turn_seq: 1,
                    agent_id: agent.agent.clone(),
                    resolved_model,
                    usage,
                    estimated_cost_pico_usd: Some(123_456_789_000),
                },
            )
            .unwrap();
        let through_seq = usage_event.seq;
        drop(log);

        let source_json = fs::read_to_string(&path).unwrap();
        let rewrite = |session_id: SessionId, stamp: Option<u64>| {
            source_json
                .lines()
                .map(|line| {
                    let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
                    value["session_id"] = serde_json::json!(session_id);
                    if value["payload"]["type"] == "model_usage_recorded" {
                        let payload = value["payload"].as_object_mut().unwrap();
                        payload.insert(
                            "estimated_cost_pico_usd".into(),
                            serde_json::to_value(stamp).unwrap(),
                        );
                    }
                    serde_json::to_string(&value).unwrap()
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        };
        let reopened = crate::events::EventLog::open(path, session_id).unwrap();
        // The TUI footer reducer sums these same durable pico-USD stamps.
        let footer_pico_usd = reopened
            .events()
            .iter()
            .filter_map(|event| match event.payload {
                EventPayload::ModelUsageRecorded {
                    estimated_cost_pico_usd,
                    ..
                } => estimated_cost_pico_usd,
                _ => None,
            })
            .sum::<u64>();
        let rebuilt = projection(reopened).unwrap();
        assert_eq!(rebuilt.usage_rollup.request_count, 1);
        assert_eq!(rebuilt.usage_rollup.input_tokens, 120);
        assert_eq!(rebuilt.usage_rollup.output_tokens, 30);
        assert_eq!(rebuilt.usage_rollup.cache_read_tokens, 40);
        assert_eq!(rebuilt.usage_rollup.cache_write_tokens, 10);
        assert_eq!(rebuilt.agent_usage[&agent.agent].request_count, 1);
        let changed_pricing = PricingConfig {
            models: BTreeMap::from([(
                model_key.clone(),
                ModelPricing {
                    input_per_million_usd: Some(
                        PicoUsdPerMillion::from_decimal_str("999").unwrap(),
                    ),
                    output_per_million_usd: Some(
                        PicoUsdPerMillion::from_decimal_str("999").unwrap(),
                    ),
                    ..ModelPricing::default()
                },
            )]),
        };
        let expected = Some(footer_pico_usd as f64 / 1_000_000_000_000.0);
        assert_eq!(
            crate::usage::with_pricing(
                rebuilt.usage_rollup.clone(),
                &PricingConfig::default(),
                &BTreeMap::new(),
            )
            .estimated_cost_usd,
            expected
        );

        let cwd = temp.path().join("fork-cwd");
        let data = temp.path().join("fork-data");
        create_private_test_dir_all(&cwd);
        let seed = SessionStore::open(&data, &cwd).unwrap();
        let sessions_dir = seed.sessions_dir.clone();
        drop(seed);
        let stamped_source = SessionId::new_v7();
        let unpriced_source = SessionId::new_v7();
        for (source_id, contents) in [
            (
                stamped_source,
                rewrite(stamped_source, Some(123_456_789_000)),
            ),
            (unpriced_source, rewrite(unpriced_source, None)),
        ] {
            let directory = sessions_dir.join(source_id.to_string());
            create_private_test_dir_all(&directory);
            write_private_test_file(&directory.join("events.jsonl"), contents);
        }
        let store = SessionStore::open(&data, &cwd).unwrap();
        let stamped_fork = store
            .fork(
                stamped_source,
                through_seq,
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .unwrap();
        let unpriced_fork = store
            .fork(
                unpriced_source,
                through_seq,
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .unwrap();
        assert_eq!(
            crate::usage::with_pricing(
                store.get(stamped_fork).unwrap().usage_rollup,
                &changed_pricing,
                &BTreeMap::new(),
            )
            .estimated_cost_usd,
            expected
        );
        assert_eq!(
            crate::usage::with_pricing(
                store.get(unpriced_fork).unwrap().usage_rollup,
                &changed_pricing,
                &BTreeMap::new(),
            )
            .estimated_cost_usd,
            None
        );
        assert_eq!(
            crate::usage::with_pricing(rebuilt.usage_rollup, &changed_pricing, &BTreeMap::new(),)
                .estimated_cost_usd,
            expected
        );
    }

    #[test]
    fn internal_usage_is_once_per_fallback_phase_and_all_kinds_survive_reopen() {
        let temp = private_tempdir();
        let path = temp.path().join("internal-usage-events.jsonl");
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let owner = crate::test_support::agent_snapshot("test", AgentMode::Primary);
        let selection = crate::test_support::run_selection("test");
        let binding = owner.fallback_chain[0].clone();
        let resolved_model = crate::model_history::wire_model(&binding);
        let fallback_model = crate::model_history::wire_model(
            &crate::test_support::model_binding_named("fallback-one"),
        );
        let revision = |value: char| format!("sha256:{}", value.to_string().repeat(64));
        let runtime_revision = RuntimeRevision::new(revision('1')).unwrap();
        let catalog_revision = CatalogRevision::new(revision('2')).unwrap();
        let provider_state_revision = ProviderStateRevision::new(revision('3')).unwrap();
        let model_revision = ModelRevision::new(revision('4')).unwrap();
        let agent_revision = AgentRevision::new(revision('5')).unwrap();
        let recipe_registry_revision = RecipeRegistryRevision::new(revision('6')).unwrap();
        let log = crate::events::EventLog::create(
            path.clone(),
            session_id,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionCreated {
                origin: SessionOrigin::Root,
                cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test").unwrap(),
                creation_selection: selection.clone(),
                creation_agent: Box::new(owner.clone()),
                runtime_revision: runtime_revision.clone(),
                catalog_revision: catalog_revision.clone(),
                provider_state_revision: provider_state_revision.clone(),
                model_revision: model_revision.clone(),
                agent_revision: agent_revision.clone(),
                recipe_registry_revision: recipe_registry_revision.clone(),
                manifest_revision: binding.manifest_revision.clone(),
            },
        )
        .unwrap();
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::RunStarted {
                client_run_id: ClientRunId::new("internal-usage-replay").unwrap(),
                selection,
                agent: Box::new(owner),
                runtime_revision,
                catalog_revision,
                provider_state_revision,
                model_revision,
                agent_revision,
                recipe_registry_revision,
                manifest_revision: binding.manifest_revision.clone(),
                selected_suffix: vec![binding],
                internal_agents: Vec::new(),
                input_through_seq: 1,
            },
        )
        .unwrap();

        let kinds = [
            (
                InternalAgentKind::Approval,
                cookie_agent_config::BUILT_IN_APPROVAL_AGENT_ID,
            ),
            (
                InternalAgentKind::ContextCompaction,
                cookie_agent_config::BUILT_IN_COMPACTION_AGENT_ID,
            ),
            (
                InternalAgentKind::SessionTitle,
                cookie_agent_config::BUILT_IN_TITLE_AGENT_ID,
            ),
        ];
        for (index, (kind, agent_name)) in kinds.into_iter().enumerate() {
            let invocation_id = InternalAgentInvocationId::new_v7();
            let internal_run_id = InternalAgentRunId::new_v7();
            let agent_id = AgentId::new(agent_name).unwrap();
            log.append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::InternalAgentStarted {
                    invocation_id,
                    internal_run_id,
                    kind,
                    backend: InternalAgentBackend::Model {
                        resolved_model: resolved_model.clone(),
                    },
                    call: SafeInternalAgentCall {
                        name: SafeCode::new("internal").unwrap(),
                        input_summary: SafeDisplayText::new("bounded input").unwrap(),
                        input_digest: Sha256Digest::of_bytes(b"input"),
                    },
                },
            )
            .unwrap();
            let usage = Usage {
                input_tokens: Some(100 + index as u64),
                input_tokens_cache_read: Some(0),
                output_tokens: Some(10),
                output_tokens_reasoning: Some(0),
                ..Usage::default()
            };
            log.append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::InternalAgentUsageRecorded {
                    internal_run_id,
                    kind,
                    agent_id: agent_id.clone(),
                    resolved_model: resolved_model.clone(),
                    usage: usage.clone(),
                    estimated_cost_pico_usd: None,
                },
            )
            .unwrap();
            if index == 0 {
                assert!(
                    log.append(
                        Some(run_id),
                        cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                        EventPayload::InternalAgentUsageRecorded {
                            internal_run_id,
                            kind,
                            agent_id: agent_id.clone(),
                            resolved_model: resolved_model.clone(),
                            usage: usage.clone(),
                            estimated_cost_pico_usd: None,
                        },
                    )
                    .is_err()
                );
                let failure = || InternalAgentFailure {
                    code: SafeCode::new("fallback").unwrap(),
                    message: SafeErrorMessage::new("test fallback").unwrap(),
                    retryable: true,
                    model_error: None,
                };
                log.append(
                    Some(run_id),
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::InternalAgentFallback {
                        invocation_id,
                        internal_run_id,
                        kind,
                        from: InternalAgentBackend::Model {
                            resolved_model: resolved_model.clone(),
                        },
                        to: InternalAgentBackend::Model {
                            resolved_model: fallback_model.clone(),
                        },
                        failure: failure(),
                        attempts: 1,
                    },
                )
                .unwrap();
                log.append(
                    Some(run_id),
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::InternalAgentUsageRecorded {
                        internal_run_id,
                        kind,
                        agent_id: agent_id.clone(),
                        resolved_model: fallback_model.clone(),
                        usage: usage.clone(),
                        estimated_cost_pico_usd: None,
                    },
                )
                .unwrap();
                log.append(
                    Some(run_id),
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::InternalAgentFallback {
                        invocation_id,
                        internal_run_id,
                        kind,
                        from: InternalAgentBackend::Model {
                            resolved_model: fallback_model.clone(),
                        },
                        to: InternalAgentBackend::Model {
                            resolved_model: resolved_model.clone(),
                        },
                        failure: failure(),
                        attempts: 2,
                    },
                )
                .unwrap();
                log.append(
                    Some(run_id),
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::InternalAgentUsageRecorded {
                        internal_run_id,
                        kind,
                        agent_id: agent_id.clone(),
                        resolved_model: resolved_model.clone(),
                        usage,
                        estimated_cost_pico_usd: None,
                    },
                )
                .unwrap();
            }
            log.append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::InternalAgentCompleted {
                    invocation_id,
                    internal_run_id,
                    kind,
                    result: SafeInternalAgentResult {
                        output_summary: SafeDisplayText::new("validated output").unwrap(),
                        output_digest: Sha256Digest::of_bytes(b"output"),
                    },
                },
            )
            .unwrap();
        }
        drop(log);

        let raw = fs::read_to_string(&path).unwrap();
        assert_eq!(
            raw.lines()
                .filter(|line| {
                    let value: serde_json::Value = serde_json::from_str(line).unwrap();
                    value["payload"]["type"] == "internal_agent_usage_recorded"
                        && value["payload"]["estimated_cost_pico_usd"].is_null()
                })
                .count(),
            5
        );
        let reopened = crate::events::EventLog::open(path, session_id).unwrap();
        let rebuilt = projection(reopened).unwrap();
        assert_eq!(rebuilt.usage_rollup.request_count, 5);
        assert_eq!(rebuilt.usage_rollup.input_tokens, 503);
        for (kind, agent_name) in kinds {
            assert_eq!(
                rebuilt.agent_usage[&AgentId::new(agent_name).unwrap()].request_count,
                if kind == InternalAgentKind::Approval {
                    3
                } else {
                    1
                }
            );
        }
        let rate = PicoUsdPerMillion::from_decimal_str("1").unwrap();
        let pricing = PricingConfig {
            models: BTreeMap::from([
                (
                    resolved_model.selection.model,
                    ModelPricing {
                        input_per_million_usd: Some(rate),
                        output_per_million_usd: Some(rate),
                        ..ModelPricing::default()
                    },
                ),
                (
                    fallback_model.selection.model,
                    ModelPricing {
                        input_per_million_usd: Some(rate),
                        output_per_million_usd: Some(rate),
                        ..ModelPricing::default()
                    },
                ),
            ]),
        };
        assert_eq!(
            crate::usage::with_pricing(rebuilt.usage_rollup, &pricing, &BTreeMap::new())
                .estimated_cost_usd,
            None
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use cookie_agent_protocol::{
        AgentMode, AgentRevision, CatalogRevision, ClientRunId, CwdIdentity, EventPayload,
        ModelRevision, ProviderStateRevision, RecipeRegistryRevision, RunId, RuntimeRevision,
        SessionId, SessionOrigin,
    };

    use crate::ownership::owner_lock_path;

    use super::{PROJECT_CWD_FILE, SessionStore};

    fn revision(label: char) -> String {
        format!("sha256:{}", label.to_string().repeat(64))
    }

    #[test]
    fn windows_session_store_applies_private_acls_before_use() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let cwd = temporary.path().join("workspace");
        std::fs::create_dir(&cwd).expect("workspace");
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).unwrap_or_else(|error| {
            panic!("Windows session store open failed for data={data:?}, cwd={cwd:?}: {error:?}")
        });
        let project = store.project_dir_path();
        for path in [
            project.to_owned(),
            project.join("sessions"),
            project.join(PROJECT_CWD_FILE),
        ] {
            cookie_agent_models::secure_store::verify_windows_private_creation(&path)
                .unwrap_or_else(|error| {
                    panic!("private ACL validation failed for {path:?}: {error:?}")
                });
        }
    }

    #[test]
    fn windows_session_store_uses_preexisting_untrusted_project_acl() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let cwd = temporary.path().join("workspace");
        std::fs::create_dir(&cwd).expect("workspace");
        let data = temporary.path().join("data");
        let project = SessionStore::project_dir(&data, &cwd);
        std::fs::create_dir_all(project.join("sessions")).expect("ordinary project");
        SessionStore::open(&data, &cwd).expect("ordinary existing project");
    }

    #[test]
    fn windows_buffered_session_persists_private_files_before_writing() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let cwd = temporary.path().join("workspace");
        std::fs::create_dir(&cwd).expect("workspace");
        let data = temporary.path().join("data");
        let store = SessionStore::open(&data, &cwd).expect("session store");
        let session_id = SessionId::new_v7();
        let agent = crate::test_support::agent_snapshot("test", AgentMode::Primary);
        let selection = crate::test_support::run_selection("test");
        let binding = agent.fallback_chain[0].clone();
        let runtime_revision = RuntimeRevision::new(revision('1')).unwrap();
        let catalog_revision = CatalogRevision::new(revision('2')).unwrap();
        let provider_state_revision = ProviderStateRevision::new(revision('3')).unwrap();
        let model_revision = ModelRevision::new(revision('4')).unwrap();
        let agent_revision = AgentRevision::new(revision('5')).unwrap();
        let recipe_registry_revision = RecipeRegistryRevision::new(revision('6')).unwrap();
        store
            .create(
                session_id,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::SessionCreated {
                    origin: SessionOrigin::Root,
                    cwd_identity: CwdIdentity::new("workspace:test").unwrap(),
                    creation_selection: selection.clone(),
                    creation_agent: Box::new(agent.clone()),
                    runtime_revision: runtime_revision.clone(),
                    catalog_revision: catalog_revision.clone(),
                    provider_state_revision: provider_state_revision.clone(),
                    model_revision: model_revision.clone(),
                    agent_revision: agent_revision.clone(),
                    recipe_registry_revision: recipe_registry_revision.clone(),
                    manifest_revision: binding.manifest_revision.clone(),
                },
            )
            .expect("buffered session");
        let run_id = RunId::new_v7();
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::RunStarted {
                    client_run_id: ClientRunId::new("windows-buffered-session").unwrap(),
                    selection,
                    agent: Box::new(agent),
                    runtime_revision,
                    catalog_revision,
                    provider_state_revision,
                    model_revision,
                    agent_revision,
                    recipe_registry_revision,
                    manifest_revision: binding.manifest_revision.clone(),
                    selected_suffix: vec![binding],
                    internal_agents: Vec::new(),
                    input_through_seq: 1,
                },
            )
            .expect("start buffered run");
        store
            .append(
                session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::UserInputSubmitted {
                    input: "persist me".into(),
                },
            )
            .expect("persist first input");

        let session_dir = store
            .project_dir_path()
            .join("sessions")
            .join(session_id.to_string());
        for path in [
            session_dir.clone(),
            session_dir.join("events.jsonl"),
            session_dir.join("meta.json"),
            owner_lock_path(&session_dir),
        ] {
            cookie_agent_models::secure_store::verify_windows_private_creation(&path)
                .unwrap_or_else(|error| {
                    panic!("private ACL validation failed for {path:?}: {error}")
                });
        }
    }
}
