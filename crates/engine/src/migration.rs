//! One-shot migration from the legacy flat project store (v1) to the
//! hierarchical per-work-dir layout (v2) — spec §6.
//!
//! [`SessionStore::open`] runs [`run_if_needed`] before it constructs anything, so
//! a store is never served from a half-migrated layout. Every step is a rename
//! guarded by an existence check, which makes the procedure resumable: a crash
//! leaves `.migrating` in the legacy project directory and the next open picks
//! the job back up. Migration only runs while no session in the work dir is
//! owned, which also rules out interrupted appends.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use cookie_agent_protocol::{SessionId, SessionMeta, SessionOrigin};
use fs2::FileExt as _;

use crate::{
    events::fsync_directory,
    ownership::{SessionOwnership, owner_lock_path, try_acquire},
    runtime::artifacts::{
        ARTIFACTS_DIR, CROSS_REFS_FILE, SHARED_ARTIFACTS_DIR, is_digest_name_common,
        scan_artifact_references_in_log, write_cross_ref_ledger,
    },
    session::{
        EVENTS_FILE, LAYOUT_MARKER_FILE, LAYOUT_VERSION, LEGACY_SESSION_META_FILE,
        PROJECT_CWD_FILE, SESSION_META_FILE, SUBAGENTS_DIR, SessionError, SessionStore,
    },
};

#[cfg(unix)]
use crate::session::create_unix_session_directory_all as ensure_private_dir;
#[cfg(windows)]
use crate::session::create_windows_session_directory as ensure_private_dir;

/// Journal kept in the legacy project dir while a migration is in flight.
const JOURNAL_FILE: &str = ".migrating";
/// Completion marker kept in the legacy project dir.
const MIGRATED_MARKER_FILE: &str = ".migrated";
/// Pointer left where the flat project lived so old builds find the new home.
const TOMBSTONE_FILE: &str = "MIGRATED";
/// Exclusion lock so two processes never migrate one work dir.
const MIGRATION_LOCK_FILE: &str = "migration.lock";
/// v1 keeps every session flat under `projects/<hash>/sessions`.
const LEGACY_SESSIONS_DIR: &str = "sessions";
const GRANT_INVALIDATIONS_FILE: &str = "grant-invalidations.jsonl";
const RUNTIME_REVISIONS_FILE: &str = "runtime-revisions-v8.jsonl";
/// v1 project-level files that move next to the v2 root sessions.
const PROJECT_FILES: [&str; 3] = [
    PROJECT_CWD_FILE,
    GRANT_INVALIDATIONS_FILE,
    RUNTIME_REVISIONS_FILE,
];
/// `layout.json` migration field values (§6.3).
const MIGRATION_IN_PROGRESS: &str = "in-progress";
const MIGRATION_COMPLETE: &str = "complete";

/// Default progress sink: one line per phase, visible in CLI and daemon logs.
pub(crate) fn stderr_progress(line: &str) {
    eprintln!("cookie-agent: {line}");
}

/// What one completed migration moved.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Outcome {
    pub(crate) roots: usize,
    pub(crate) children: usize,
    pub(crate) artifacts: usize,
    pub(crate) shared_artifacts: usize,
    pub(crate) cross_refs: usize,
}

pub(crate) fn run_if_needed(
    data_root: &Path,
    cwd: &Path,
    progress: &dyn Fn(&str),
) -> Result<Option<Outcome>, SessionError> {
    let legacy_root = SessionStore::project_dir(data_root, cwd);
    let legacy_sessions = legacy_root.join(LEGACY_SESSIONS_DIR);
    if !legacy_sessions.is_dir() {
        return Ok(None);
    }
    let target = SessionStore::resolve_workdir_dir(data_root, cwd);
    if migration_state(&target).as_deref() == Some(MIGRATION_COMPLETE) {
        // §6.1: the v2 store is live and the flat directory is a leftover. Point
        // old binaries at the new home and leave the remnant alone.
        ensure_tombstone(&legacy_root, &target)?;
        Journal::clear(&legacy_root)?;
        return Ok(None);
    }
    let _lock = MigrationLock::acquire(&legacy_root)?;
    reject_live_owners(&legacy_sessions)?;
    let plan = Plan::discover(&legacy_sessions, &target)?;
    progress(&format!(
        "migrating session store… {} sessions in {}",
        plan.pending_total(),
        legacy_root.display()
    ));
    let mut journal = Journal::load(&legacy_root);
    let outcome = migrate(&legacy_root, &target, &plan, &mut journal, progress)?;
    progress(&format!(
        "migrating session store… done ({} roots, {} children, {} artifacts)",
        outcome.roots, outcome.children, outcome.artifacts
    ));
    Ok(Some(outcome))
}

/// Steps 2 through 6 of §6.3, resumable at any point.
fn migrate(
    legacy_root: &Path,
    target: &Path,
    plan: &Plan,
    journal: &mut Journal,
    progress: &dyn Fn(&str),
) -> Result<Outcome, SessionError> {
    let legacy_sessions = legacy_root.join(LEGACY_SESSIONS_DIR);
    let legacy_artifacts = legacy_root.join(ARTIFACTS_DIR);
    let expected_digests = file_names(&legacy_artifacts)?.len();

    // 2. SCAFFOLD: reserve the v2 work dir and move the project-level files.
    write_layout_marker(target, MIGRATION_IN_PROGRESS)?;
    for file in PROJECT_FILES {
        let source = legacy_root.join(file);
        if source.is_file() {
            let destination = target.join(file);
            if !destination.exists() {
                rename(&source, &destination, target)?;
            }
        }
    }
    journal.store(legacy_root)?;
    crash_after("scaffold")?;

    // 3. ROOTS first so every child has a `subagents/` parent to land in.
    for root in plan.pending_roots() {
        let destination = target.join(root.to_string());
        let source = legacy_sessions.join(root.to_string());
        journal.consistent(&source, root)?;
        move_session(&source, &destination)?;
        ensure_private_dir(&destination.join(ARTIFACTS_DIR))?;
        ensure_private_dir(&destination.join(SUBAGENTS_DIR))?;
        journal.done(legacy_root, "roots", root)?;
    }
    progress(&format!(
        "migrating session store… {} roots moved",
        plan.pending_roots().count()
    ));
    crash_after("roots")?;

    // 4. CHILDREN under their discovered root.
    for (child, root) in plan.pending_children() {
        let destination = target
            .join(root.to_string())
            .join(SUBAGENTS_DIR)
            .join(child.to_string());
        let source = legacy_sessions.join(child.to_string());
        journal.consistent(&source, child)?;
        move_session(&source, &destination)?;
        journal.done(legacy_root, "children", child)?;
    }
    progress(&format!(
        "migrating session store… {} child sessions moved",
        plan.pending_children().count()
    ));
    crash_after("children")?;

    // 5. ARTIFACTS: place each digest in the tree that references it, shared
    // otherwise (§6.3 step 5, §6.5).
    let references = tree_references(target, plan)?;
    let mut outcome = Outcome {
        roots: plan.roots.len(),
        children: plan.children.len(),
        ..Outcome::default()
    };
    let mut ledger = BTreeSet::new();
    let mut touched: BTreeSet<PathBuf> = BTreeSet::new();
    for name in file_names(&legacy_artifacts)? {
        let source = legacy_artifacts.join(&name);
        // Torn or temporary names are not digests: they belong to the shared
        // store, where startup cleanup and the grace window handle them (§6.5).
        let placement = is_digest_name_common(&name)
            .then(|| placement_root(&references, &name))
            .flatten();
        let destination = match placement {
            Some(root) => {
                outcome.artifacts += 1;
                let trees = references.get(&name).into_iter().flatten();
                for tree in trees.filter(|tree| **tree != root) {
                    ledger.insert((name.clone(), *tree));
                }
                target
                    .join(root.to_string())
                    .join(ARTIFACTS_DIR)
                    .join(&name)
            }
            None => {
                outcome.shared_artifacts += 1;
                target.join(SHARED_ARTIFACTS_DIR).join(&name)
            }
        };
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(target);
        if !parent.is_dir() {
            ensure_private_dir(parent)?;
        }
        match fs::rename(&source, &destination) {
            Ok(()) => {
                touched.insert(parent.to_path_buf());
            }
            // Lost the race with a resumed run: already placed.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source_error) => {
                return Err(SessionError::Io {
                    path: destination,
                    source: source_error,
                });
            }
        }
    }
    // Renames are cheap; one sync per directory is enough to make the batch
    // durable (§6.3 step 5), which a sync per blob would not be.
    for directory in touched {
        fsync_directory(&directory)?;
    }
    if !ledger.is_empty() {
        write_cross_ref_ledger(&target.join(CROSS_REFS_FILE), &ledger).map_err(|source| {
            SessionError::Io {
                path: target.join(CROSS_REFS_FILE),
                source,
            }
        })?;
        outcome.cross_refs = ledger.len();
    }
    progress(&format!(
        "migrating session store… {} artifacts placed, {} shared",
        outcome.artifacts, outcome.shared_artifacts
    ));
    crash_after("artifacts")?;

    // 6. VERIFY, then mark the job complete and retire the flat project (§6.6).
    verify(target, &legacy_sessions, plan, expected_digests, &outcome)?;
    // The journal is dropped as soon as the moves are known good, so a crash in
    // what follows resumes from a completed plan rather than a stale journal.
    Journal::clear(legacy_root)?;
    write_layout_marker(target, MIGRATION_COMPLETE)?;
    fsync_directory(target)?;
    finish(legacy_root, target, &legacy_sessions, &legacy_artifacts)?;
    crash_after("verify")?;
    Ok(outcome)
}

/// Which root owns a digest's bytes.
fn placement_root(
    references: &BTreeMap<String, BTreeSet<SessionId>>,
    digest: &str,
) -> Option<SessionId> {
    references
        .get(digest)?
        .iter()
        .min_by_key(|tree| tree.to_string())
        .copied()
}

/// Digest → set of trees whose logs reference it, from the freshly moved logs.
fn tree_references(
    target: &Path,
    plan: &Plan,
) -> Result<BTreeMap<String, BTreeSet<SessionId>>, SessionError> {
    let mut references: BTreeMap<String, BTreeSet<SessionId>> = BTreeMap::new();
    for root in &plan.roots {
        let directory = target.join(root.to_string());
        let mut live = HashSet::new();
        absorb(
            scan_artifact_references_in_log(&directory.join(EVENTS_FILE)),
            &mut live,
        );
        for child in plan.children_of(*root) {
            absorb(
                scan_artifact_references_in_log(
                    &directory
                        .join(SUBAGENTS_DIR)
                        .join(child.to_string())
                        .join(EVENTS_FILE),
                ),
                &mut live,
            );
        }
        for digest in live {
            references.entry(digest).or_default().insert(*root);
        }
    }
    Ok(references)
}

fn absorb(result: std::io::Result<HashSet<String>>, into: &mut HashSet<String>) {
    match result {
        Ok(found) => into.extend(found),
        // A log that cannot be read contributes nothing: its digests then look
        // unreferenced and land in the shared store, which the router still
        // searches (§6.5).
        Err(error) => eprintln!("migration: artifact scan skipped: {error}"),
    }
}

/// §6.6. Any mismatch aborts before `.migrated` is written, leaving `.migrating`
/// for the next open to resume.
fn verify(
    target: &Path,
    legacy_sessions: &Path,
    plan: &Plan,
    expected_digests: usize,
    outcome: &Outcome,
) -> Result<(), SessionError> {
    let stranded: Vec<String> = directory_names(legacy_sessions)?
        .into_iter()
        .filter(|name| name.parse::<SessionId>().is_ok())
        .collect();
    if !stranded.is_empty() {
        return Err(SessionError::Migration(format!(
            "{} legacy sessions were not moved",
            stranded.len()
        )));
    }
    let mut placed_children: HashMap<SessionId, usize> = HashMap::new();
    for root in &plan.roots {
        let directory = target.join(root.to_string());
        check_metadata(&directory, *root, plan)?;
        if !directory.join(ARTIFACTS_DIR).is_dir() || !directory.join(SUBAGENTS_DIR).is_dir() {
            return Err(SessionError::Migration(format!(
                "{} is missing artifacts/ or subagents/",
                directory.display()
            )));
        }
        for name in directory_names(&directory.join(SUBAGENTS_DIR))? {
            let Ok(child) = name.parse::<SessionId>() else {
                continue;
            };
            *placed_children.entry(child).or_default() += 1;
            check_metadata(&directory.join(SUBAGENTS_DIR).join(&name), child, plan)?;
        }
    }
    for (child, root) in &plan.children {
        match placed_children.get(child) {
            Some(1) => Ok(()),
            Some(count) => Err(SessionError::Migration(format!(
                "child {child} landed in {count} roots, expected one"
            ))),
            None => Err(SessionError::Migration(format!(
                "child {child} is not under root {root}",
            ))),
        }?;
    }
    let moved = outcome.artifacts + outcome.shared_artifacts;
    if moved != expected_digests {
        return Err(SessionError::Migration(format!(
            "artifact count changed: {expected_digests} legacy, {moved} placed"
        )));
    }
    Ok(())
}

fn check_metadata(directory: &Path, id: SessionId, plan: &Plan) -> Result<(), SessionError> {
    let Some(meta) = read_metadata(directory) else {
        if plan.opaque.contains(&id) {
            // Its cache was already unreadable before the move; the store
            // rebuilds it from the event log on first use.
            return Ok(());
        }
        return Err(SessionError::Migration(format!(
            "{} has no readable metadata after the move",
            directory.display()
        )));
    };
    if meta.session_id != id {
        return Err(SessionError::Migration(format!(
            "{} declares session {}",
            directory.display(),
            meta.session_id
        )));
    }
    Ok(())
}

/// Write the tombstone, drop the emptied legacy directories, release nothing —
/// the caller still holds `migration.lock`.
fn finish(
    legacy_root: &Path,
    target: &Path,
    legacy_sessions: &Path,
    legacy_artifacts: &Path,
) -> Result<(), SessionError> {
    write_atomically(
        &legacy_root.join(MIGRATED_MARKER_FILE),
        format!("{}\n", target.display()).as_bytes(),
        legacy_root,
    )?;
    write_tombstone(legacy_root, target)?;
    // Only ever empty at this point: a leftover means verification was too
    // generous, and keeping it costs nothing but disk.
    for directory in [legacy_sessions, legacy_artifacts] {
        if directory.is_dir() && directory_is_empty(directory) {
            let _ = fs::remove_dir(directory);
        }
    }
    fsync_directory(legacy_root).map_err(SessionError::Event)
}

fn ensure_tombstone(legacy_root: &Path, target: &Path) -> Result<(), SessionError> {
    if legacy_root.join(TOMBSTONE_FILE).is_file() || !legacy_root.join(LEGACY_SESSIONS_DIR).is_dir()
    {
        return Ok(());
    }
    write_tombstone(legacy_root, target)
}

fn write_tombstone(legacy_root: &Path, target: &Path) -> Result<(), SessionError> {
    write_atomically(
        &legacy_root.join(TOMBSTONE_FILE),
        format!(
            "This work dir's sessions moved to {}\n\
             This cookie build reads the hierarchical session store.\n",
            target.display()
        )
        .as_bytes(),
        legacy_root,
    )
}

/// The plan: which legacy sessions are roots, and where each child lands.
#[derive(Debug, Default)]
struct Plan {
    roots: Vec<SessionId>,
    children: BTreeMap<SessionId, SessionId>,
    /// Sessions whose metadata cache could not be parsed. They become roots and
    /// their artifacts are treated as unreferenced (§6.5).
    opaque: BTreeSet<SessionId>,
    /// Sessions still sitting in the flat layout. A resumed run plans over the
    /// whole tree but only moves these.
    pending: BTreeSet<SessionId>,
}

impl Plan {
    fn pending_total(&self) -> usize {
        self.pending.len()
    }

    fn pending_roots(&self) -> impl Iterator<Item = SessionId> + '_ {
        self.roots
            .iter()
            .copied()
            .filter(|root| self.pending.contains(root))
    }

    fn pending_children(&self) -> impl Iterator<Item = (SessionId, SessionId)> + '_ {
        self.children
            .iter()
            .filter(|(child, _)| self.pending.contains(child))
            .map(|(child, root)| (*child, *root))
    }

    fn children_of(&self, root: SessionId) -> impl Iterator<Item = SessionId> + '_ {
        self.children
            .iter()
            .filter(move |(_, parent)| **parent == root)
            .map(|(child, _)| *child)
    }

    /// Plans over the legacy directories *and* whatever a previous, interrupted
    /// run already moved, so a resume places children under the same roots.
    fn discover(legacy_sessions: &Path, target: &Path) -> Result<Self, SessionError> {
        let mut sessions: BTreeMap<SessionId, Option<SessionMeta>> = BTreeMap::new();
        let mut pending: BTreeSet<SessionId> = BTreeSet::new();
        for (directory, is_pending) in [(legacy_sessions, true), (target, false)] {
            for name in directory_names(directory)? {
                let Ok(id) = name.parse::<SessionId>() else {
                    continue;
                };
                let meta = read_metadata(&directory.join(&name));
                if meta.is_none() && is_pending {
                    eprintln!("migration: session {id} has no readable metadata cache");
                }
                sessions.entry(id).or_insert_with(|| meta.clone());
                if is_pending {
                    pending.insert(id);
                }
                // Children a previous run already filed under their root count
                // as known placements, so a resume does not orphan them.
                if !is_pending {
                    let subagents = directory.join(&name).join(SUBAGENTS_DIR);
                    for child in directory_names(&subagents)? {
                        let Ok(child) = child.parse::<SessionId>() else {
                            continue;
                        };
                        let meta = read_metadata(&subagents.join(child.to_string()));
                        sessions.entry(child).or_insert_with(|| meta.clone());
                    }
                }
            }
        }
        let mut plan = Plan {
            pending,
            ..Plan::default()
        };
        for (id, meta) in &sessions {
            if meta.is_none() {
                plan.opaque.insert(*id);
            }
            match delegated_root(meta.as_ref()).filter(|claimed| *claimed != *id) {
                // A child lands in the directory of its root, following the
                // origin chain to a session that is itself a root.
                Some(claimed) if sessions.contains_key(&claimed) => {
                    let root = resolve_root(&sessions, claimed);
                    plan.children.insert(*id, root);
                }
                // A root, or an orphan whose root session is gone: a deleted
                // parent must not strand its transcript (§6.3 step 1).
                _ => plan.roots.push(*id),
            }
        }
        Ok(plan)
    }
}

/// `root_session_id` of a delegated session (§6.3 step 1).
fn delegated_root(meta: Option<&SessionMeta>) -> Option<SessionId> {
    match meta?.origin {
        SessionOrigin::Root => None,
        SessionOrigin::Delegated {
            root_session_id, ..
        } => Some(root_session_id),
    }
}

/// Follow `root_session_id` links until a session that is not itself delegated
/// is reached, so nested children still land in one tree. Cycles promote.
fn resolve_root(
    sessions: &BTreeMap<SessionId, Option<SessionMeta>>,
    start: SessionId,
) -> SessionId {
    let mut candidate = start;
    let mut seen: BTreeSet<SessionId> = BTreeSet::from([start]);
    while let Some(claimed) = sessions
        .get(&candidate)
        .and_then(|meta| delegated_root(meta.as_ref()))
    {
        if claimed == candidate || !sessions.contains_key(&claimed) || !seen.insert(claimed) {
            break;
        }
        candidate = claimed;
    }
    candidate
}

/// Journal state: `{phase, done: [session ids]}`, fsynced after every move.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
struct Journal {
    phase: String,
    done: BTreeSet<String>,
}

impl Journal {
    fn load(legacy_root: &Path) -> Self {
        match fs::read(legacy_root.join(JOURNAL_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    fn done(&mut self, legacy_root: &Path, phase: &str, id: SessionId) -> Result<(), SessionError> {
        self.phase = phase.to_owned();
        self.done.insert(id.to_string());
        self.store(legacy_root)
    }

    fn store(&self, legacy_root: &Path) -> Result<(), SessionError> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|source| SessionError::Json {
            path: legacy_root.join(JOURNAL_FILE),
            source,
        })?;
        write_atomically(&legacy_root.join(JOURNAL_FILE), &bytes, legacy_root)
    }

    /// A resume must not find a session the journal already finished still
    /// sitting in the flat layout: that means two migrations raced.
    fn consistent(&self, source: &Path, id: SessionId) -> Result<(), SessionError> {
        if self.done.contains(&id.to_string()) && source.is_dir() {
            return Err(SessionError::Migration(format!(
                "journal records {id} as moved, but {} still exists",
                source.display()
            )));
        }
        Ok(())
    }

    fn clear(legacy_root: &Path) -> Result<(), SessionError> {
        match fs::remove_file(legacy_root.join(JOURNAL_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(SessionError::Io {
                path: legacy_root.join(JOURNAL_FILE),
                source,
            }),
        }
    }
}

/// Test seam: abort the migration right after `phase` completes, leaving the
/// disk and journal exactly as a crash would (§8.2 test 2).
#[cfg(test)]
fn crash_after(phase: &str) -> Result<(), SessionError> {
    if injected_crash_matches(phase) {
        return Err(SessionError::Migration(format!(
            "injected crash after {phase}"
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn set_crash_after(phase: Option<&'static str>) {
    let mut slot = CRASH_AFTER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = phase;
}

#[cfg(test)]
fn injected_crash_matches(phase: &str) -> bool {
    let slot = CRASH_AFTER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    slot.is_some_and(|crash| crash == phase)
}

#[cfg(test)]
static CRASH_AFTER: std::sync::Mutex<Option<&'static str>> = std::sync::Mutex::new(None);

#[cfg(not(test))]
fn crash_after(_phase: &str) -> Result<(), SessionError> {
    Ok(())
}

/// Exclusion lock for the whole procedure (§6.3 step 0).
struct MigrationLock {
    file: fs::File,
}

impl MigrationLock {
    fn acquire(legacy_root: &Path) -> Result<Self, SessionError> {
        let path = legacy_root.join(MIGRATION_LOCK_FILE);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // The lock file's contents are never read; an existing one stays.
            .truncate(false)
            .open(&path)
            .map_err(|source| SessionError::Io {
                path: path.clone(),
                source,
            })?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { file }),
            Err(contention) => Err(SessionError::Migration(format!(
                "another cookie process is migrating {}; quit it and retry ({contention})",
                legacy_root.display()
            ))),
        }
    }
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        // Explicit `fs2` call: the inherent `File::unlock` needs a newer MSRV.
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// Migration only runs with no live owner anywhere in the store (§6.3 step 0),
/// which is also what guarantees there are no in-flight appends.
fn reject_live_owners(legacy_sessions: &Path) -> Result<(), SessionError> {
    for name in directory_names(legacy_sessions)? {
        let Ok(id) = name.parse::<SessionId>() else {
            continue;
        };
        let directory = legacy_sessions.join(&name);
        match try_acquire(&directory) {
            // A stale lock is ours to take and drop; it moves with its session.
            Ok(SessionOwnership::Owned(_held)) => {}
            Ok(SessionOwnership::Foreign) => {
                return Err(SessionError::Migration(format!(
                    "session {id} is owned by a live process; quit cookie and retry"
                )));
            }
            Err(source) => {
                return Err(SessionError::Io {
                    path: owner_lock_path(&directory),
                    source,
                });
            }
        }
    }
    Ok(())
}

/// Move one session directory and rename its metadata cache. Existence-checked,
/// so re-running a completed item is a no-op.
fn move_session(source: &Path, destination: &Path) -> Result<(), SessionError> {
    if destination.is_dir() || !source.is_dir() {
        // Already moved. Nothing left to do for this id.
        if !destination.is_dir() && !source.is_dir() {
            return Err(SessionError::Migration(format!(
                "session directory {} vanished mid-migration",
                source.display()
            )));
        }
        return Ok(());
    }
    let source_lock = owner_lock_path(source);
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(destination);
    ensure_private_dir(parent)?;
    rename(source, destination, parent)?;
    let legacy_meta = destination.join(LEGACY_SESSION_META_FILE);
    if legacy_meta.is_file() {
        rename(
            &legacy_meta,
            &destination.join(SESSION_META_FILE),
            destination,
        )?;
    }
    // Windows keeps the ownership lock beside the directory; unix moves it along.
    if source_lock.is_file() {
        rename(&source_lock, &owner_lock_path(destination), parent)?;
    }
    Ok(())
}

fn rename(source: &Path, destination: &Path, sync_dir: &Path) -> Result<(), SessionError> {
    match fs::rename(source, destination) {
        Ok(()) => {
            fsync_directory(sync_dir)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source_error) => Err(SessionError::Io {
            path: destination.to_owned(),
            source: source_error,
        }),
    }
}

/// `{"version":2,"migration":"<state>"}`; written atomically and never clobbered
/// by the plain marker the store writes for a fresh v2 work dir.
fn write_layout_marker(target: &Path, state: &str) -> Result<(), SessionError> {
    // The parent is the `sessions` root under the data directory, not a
    // directory inside the work dir.
    if let Some(parent) = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        ensure_private_dir(parent)?;
    }
    ensure_private_dir(target)?;
    let marker = serde_json::json!({ "version": LAYOUT_VERSION, "migration": state });
    let bytes = serde_json::to_vec_pretty(&marker).map_err(|source| SessionError::Json {
        path: target.join(LAYOUT_MARKER_FILE),
        source,
    })?;
    write_atomically(&target.join(LAYOUT_MARKER_FILE), &bytes, target)
}

fn migration_state(target: &Path) -> Option<String> {
    let text = fs::read_to_string(target.join(LAYOUT_MARKER_FILE)).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("migration")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn write_atomically(path: &Path, bytes: &[u8], parent: &Path) -> Result<(), SessionError> {
    let temporary = path.with_extension("migration-tmp");
    let mut file = fs::File::options()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
    drop(file);
    match fs::rename(&temporary, path) {
        Ok(()) => {}
        Err(source) => {
            let _ = fs::remove_file(&temporary);
            return Err(SessionError::Io {
                path: path.to_owned(),
                source,
            });
        }
    }
    fsync_directory(parent)?;
    Ok(())
}

fn read_metadata(directory: &Path) -> Option<SessionMeta> {
    for file in [SESSION_META_FILE, LEGACY_SESSION_META_FILE] {
        let path = directory.join(file);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(meta) = serde_json::from_str::<SessionMeta>(&text) {
            return Some(meta);
        }
    }
    None
}

/// Names of the sub-directories of `directory`, sorted, empty when absent.
fn directory_names(directory: &Path) -> Result<Vec<String>, SessionError> {
    named(directory, true)
}

/// Names of the plain files of `directory`, sorted, empty when absent.
fn file_names(directory: &Path) -> Result<Vec<String>, SessionError> {
    named(directory, false)
}

fn named(directory: &Path, want_directory: bool) -> Result<Vec<String>, SessionError> {
    let mut names = Vec::new();
    let mut entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(names),
        Err(source) => {
            return Err(SessionError::Io {
                path: directory.to_owned(),
                source,
            });
        }
    };
    while let Some(entry) = entries
        .next()
        .transpose()
        .map_err(|source| SessionError::Io {
            path: directory.to_owned(),
            source,
        })?
    {
        let is_dir = entry
            .file_type()
            .map(|file_type| file_type.is_dir())
            .unwrap_or(false);
        if is_dir == want_directory {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn directory_is_empty(directory: &Path) -> bool {
    match fs::read_dir(directory) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::Mutex};

    use cookie_agent_protocol::{
        AgentMode, AgentRevision, CatalogRevision, ClientRunId, EventPayload, InvocationId,
        ModelRevision, ProviderStateRevision, RecipeRegistryRevision, RunId, RuntimeRevision,
        SessionId, SessionOrigin, ToolCallId,
    };
    use sha2::{Digest as _, Sha256};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::{runtime::artifacts::ArtifactRouter, test_support};

    /// The crash seam is process-wide, so migration tests take this in turn.
    static SERIAL: Mutex<()> = Mutex::new(());

    struct Fixture {
        _temp: tempfile::TempDir,
        data_root: PathBuf,
        cwd: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("temporary root");
            #[cfg(unix)]
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
                .expect("private temp");
            let cwd = temp.path().join("workspace");
            fs::create_dir_all(&cwd).expect("workspace");
            #[cfg(unix)]
            fs::set_permissions(&cwd, fs::Permissions::from_mode(0o700))
                .expect("private workspace");
            let data_root = temp.path().join("data");
            Self {
                _temp: temp,
                data_root,
                cwd,
            }
        }

        fn legacy(&self) -> PathBuf {
            SessionStore::project_dir(&self.data_root, &self.cwd)
        }

        fn legacy_sessions(&self) -> PathBuf {
            self.legacy().join(LEGACY_SESSIONS_DIR)
        }

        fn legacy_artifacts(&self) -> PathBuf {
            self.legacy().join(ARTIFACTS_DIR)
        }

        fn v2(&self) -> PathBuf {
            SessionStore::resolve_workdir_dir(&self.data_root, &self.cwd)
        }
    }

    fn event_origin() -> cookie_agent_protocol::EventOrigin {
        cookie_agent_protocol::EventOrigin::new("engine:migration-test").expect("event origin")
    }

    fn delegated(root: SessionId, parent: SessionId, depth: u32) -> SessionOrigin {
        SessionOrigin::Delegated {
            root_session_id: root,
            parent_session_id: parent,
            parent_run_id: RunId::new_v7(),
            parent_tool_call_id: ToolCallId::new_v7(),
            invocation_id: InvocationId::new_v7(),
            depth,
        }
    }

    /// Creates a published session in whichever layout `store` serves.
    fn session(store: &SessionStore, origin: SessionOrigin) -> SessionId {
        let id = SessionId::new_v7();
        let agent = test_support::agent_snapshot("test", AgentMode::Primary);
        let selection = test_support::run_selection("test");
        let binding = agent.fallback_chain[0].clone();
        let revision = |label: char| format!("sha256:{}", label.to_string().repeat(64));
        store
            .create(
                id,
                event_origin(),
                EventPayload::SessionCreated {
                    origin,
                    cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test")
                        .expect("cwd identity"),
                    creation_selection: selection.clone(),
                    creation_agent: Box::new(agent.clone()),
                    runtime_revision: RuntimeRevision::new(revision('1'))
                        .expect("runtime revision"),
                    catalog_revision: CatalogRevision::new(revision('2'))
                        .expect("catalog revision"),
                    provider_state_revision: ProviderStateRevision::new(revision('3'))
                        .expect("provider revision"),
                    model_revision: ModelRevision::new(revision('4')).expect("model revision"),
                    agent_revision: AgentRevision::new(revision('5')).expect("agent revision"),
                    recipe_registry_revision: RecipeRegistryRevision::new(revision('6'))
                        .expect("recipe revision"),
                    manifest_revision: binding.manifest_revision.clone(),
                },
            )
            .expect("create session");
        let run_id = RunId::new_v7();
        store
            .append(
                id,
                Some(run_id),
                event_origin(),
                EventPayload::RunStarted {
                    client_run_id: ClientRunId::new("migration-test").expect("client run id"),
                    selection: selection.clone(),
                    agent: Box::new(agent),
                    runtime_revision: RuntimeRevision::new(revision('1'))
                        .expect("runtime revision"),
                    catalog_revision: CatalogRevision::new(revision('2'))
                        .expect("catalog revision"),
                    provider_state_revision: ProviderStateRevision::new(revision('3'))
                        .expect("provider revision"),
                    model_revision: ModelRevision::new(revision('4')).expect("model revision"),
                    agent_revision: AgentRevision::new(revision('5')).expect("agent revision"),
                    recipe_registry_revision: RecipeRegistryRevision::new(revision('6'))
                        .expect("recipe revision"),
                    manifest_revision: binding.manifest_revision.clone(),
                    selected_suffix: vec![binding],
                    internal_agents: Vec::new(),
                    input_through_seq: 1,
                },
            )
            .expect("start run");
        // A second write publishes the buffered directory into the store.
        store
            .append(
                id,
                Some(run_id),
                event_origin(),
                EventPayload::UserInputSubmitted {
                    input: "seed".to_owned(),
                },
            )
            .expect("submit input");
        store.persist_buffered_session(id).expect("publish session");
        id
    }

    /// Records a durable reference to `digest` in `session`'s event log.
    fn reference(store: &SessionStore, session: SessionId, digest: &str) {
        let run = store
            .get(session)
            .expect("session")
            .runs
            .keys()
            .next()
            .copied()
            .expect("open run");
        store
            .append(
                session,
                Some(run),
                event_origin(),
                EventPayload::UserInputSubmitted {
                    input: format!("artifact://sha256/{digest}"),
                },
            )
            .expect("reference artifact");
    }

    /// Writes a blob into the legacy project-wide artifact directory.
    fn legacy_artifact(fixture: &Fixture, content: &[u8]) -> String {
        fs::create_dir_all(fixture.legacy_artifacts()).expect("legacy artifacts");
        let digest = digest_of(content);
        fs::write(fixture.legacy_artifacts().join(&digest), content).expect("legacy artifact");
        digest
    }

    fn digest_of(content: &[u8]) -> String {
        Sha256::digest(content)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Two roots with a child and a grandchild under one of them, an artifact per
    /// tree, one referenced by both, one orphaned and one torn temporary file.
    struct Scenario {
        fixture: Fixture,
        roots: [SessionId; 2],
        children: [SessionId; 3],
        only_first: String,
        only_second: String,
        shared_between_trees: String,
        unreferenced: String,
        torn: String,
    }

    fn scenario() -> Scenario {
        let fixture = Fixture::new();
        let store = SessionStore::open_flat_for_test(&fixture.data_root, &fixture.cwd)
            .expect("legacy store");
        let [first, second] = [
            session(&store, SessionOrigin::Root),
            session(&store, SessionOrigin::Root),
        ];
        let child = session(&store, delegated(first, first, 1));
        let grandchild = session(&store, delegated(first, child, 2));
        let other_child = session(&store, delegated(second, second, 1));
        let only_first = legacy_artifact(&fixture, b"referenced by the first tree only");
        let only_second = legacy_artifact(&fixture, b"referenced by the second tree only");
        let shared_between_trees = legacy_artifact(&fixture, b"referenced by two trees");
        let unreferenced = legacy_artifact(&fixture, b"referenced by nobody");
        fs::create_dir_all(fixture.legacy_artifacts()).expect("legacy artifacts");
        let torn = format!("{}.tmp", digest_of(b"torn publication"));
        fs::write(fixture.legacy_artifacts().join(&torn), b"torn publication")
            .expect("torn artifact");
        reference(&store, child, &only_first);
        reference(&store, grandchild, &shared_between_trees);
        reference(&store, other_child, &shared_between_trees);
        reference(&store, second, &only_second);
        drop(store);
        // Closing the store leaves its ownership locks behind as stale
        // artifacts, which migrate with their session directories (§6.4).
        assert!(owner_lock_path(&fixture.legacy_sessions().join(child.to_string())).is_file());
        Scenario {
            fixture,
            roots: [first, second],
            children: [child, grandchild, other_child],
            only_first,
            only_second,
            shared_between_trees,
            unreferenced,
            torn,
        }
    }

    fn all_ids(scenario: &Scenario) -> [SessionId; 5] {
        let [first, second] = scenario.roots;
        let [child, grandchild, other_child] = scenario.children;
        [first, second, child, grandchild, other_child]
    }

    /// The root that must own a cross-tree digest: the lexicographically first.
    fn cross_owner(scenario: &Scenario) -> SessionId {
        let [first, second] = scenario.roots;
        [first, second]
            .into_iter()
            .min_by_key(|id| id.to_string())
            .expect("root")
    }

    fn ledger_entries(workdir: &Path) -> Vec<(String, SessionId)> {
        let text = fs::read_to_string(workdir.join(CROSS_REFS_FILE)).expect("cross-ref ledger");
        text.lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .map(|value| {
                (
                    value["digest"].as_str().expect("digest").to_owned(),
                    value["tree"]
                        .as_str()
                        .expect("tree")
                        .parse::<SessionId>()
                        .expect("tree id"),
                )
            })
            .collect()
    }

    fn assert_migrated(scenario: &Scenario, store: &SessionStore) {
        let fixture = &scenario.fixture;
        let v2 = fixture.v2();
        let [first, second] = scenario.roots;
        let [child, grandchild, other_child] = scenario.children;
        let marker: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(v2.join(LAYOUT_MARKER_FILE)).expect("layout marker"),
        )
        .expect("layout marker json");
        assert_eq!(marker["version"], serde_json::json!(LAYOUT_VERSION));
        assert_eq!(marker["migration"], serde_json::json!(MIGRATION_COMPLETE));
        for root in [first, second] {
            let directory = v2.join(root.to_string());
            assert!(
                directory.join(SESSION_META_FILE).is_file(),
                "{root} metadata"
            );
            assert!(
                !directory.join(LEGACY_SESSION_META_FILE).exists(),
                "{root} kept meta.json"
            );
            assert!(directory.join(EVENTS_FILE).is_file(), "{root} event log");
            assert!(
                directory.join(ARTIFACTS_DIR).is_dir(),
                "{root} artifact store"
            );
            assert!(directory.join(SUBAGENTS_DIR).is_dir(), "{root} subagents");
        }
        for (parent, id) in [(first, child), (first, grandchild), (second, other_child)] {
            let directory = v2
                .join(parent.to_string())
                .join(SUBAGENTS_DIR)
                .join(id.to_string());
            assert!(
                directory.join(SESSION_META_FILE).is_file(),
                "{id} under {parent}"
            );
            assert!(
                owner_lock_path(&directory).is_file(),
                "stale lock for {id} did not move"
            );
            assert!(directory.join(EVENTS_FILE).is_file(), "{id} event log");
            assert!(
                !v2.join(id.to_string()).exists(),
                "{id} left at the root level"
            );
        }
        // Artifacts follow the tree that reads them; the rest is shared (§6.3).
        assert!(
            v2.join(first.to_string())
                .join(ARTIFACTS_DIR)
                .join(&scenario.only_first)
                .is_file()
        );
        assert!(
            v2.join(second.to_string())
                .join(ARTIFACTS_DIR)
                .join(&scenario.only_second)
                .is_file()
        );
        assert!(
            v2.join(cross_owner(scenario).to_string())
                .join(ARTIFACTS_DIR)
                .join(&scenario.shared_between_trees)
                .is_file()
        );
        assert!(
            v2.join(SHARED_ARTIFACTS_DIR)
                .join(&scenario.unreferenced)
                .is_file()
        );
        assert!(v2.join(SHARED_ARTIFACTS_DIR).join(&scenario.torn).is_file());
        assert_eq!(
            ledger_entries(&v2),
            vec![(
                scenario.shared_between_trees.clone(),
                if cross_owner(scenario) == first {
                    second
                } else {
                    first
                }
            )]
        );
        // The flat project is retired with a pointer for older builds (§6.3 step 6).
        assert!(!fixture.legacy_sessions().exists(), "legacy sessions dir");
        assert!(!fixture.legacy_artifacts().exists(), "legacy artifact dir");
        assert!(
            !fixture.legacy().join(JOURNAL_FILE).exists(),
            "journal left behind"
        );
        assert!(
            fixture.legacy().join(MIGRATED_MARKER_FILE).is_file(),
            "migrated marker"
        );
        assert!(fixture.legacy().join(TOMBSTONE_FILE).is_file(), "tombstone");
        // Project-level files move with the scaffold (§6.3 step 2).
        assert!(v2.join(PROJECT_CWD_FILE).is_file(), "cwd file");
        assert!(
            !fixture.legacy().join(PROJECT_CWD_FILE).exists(),
            "cwd left behind"
        );
        // Nothing but the v2 vocabulary ends up inside the work dir.
        let mut stray = directory_names(&v2)
            .expect("work dir entries")
            .into_iter()
            .filter(|name| name.parse::<SessionId>().is_err())
            .collect::<Vec<_>>();
        stray.sort();
        assert_eq!(
            stray,
            vec![SHARED_ARTIFACTS_DIR.to_owned()],
            "unexpected directories in the work dir"
        );
        // Every session is still addressable, and no child sits at the top level.
        for id in all_ids(scenario) {
            assert_eq!(store.summary(id).expect("summary").meta.session_id, id);
        }
        assert!(!store.is_flat_layout());
    }

    #[test]
    fn migration_moves_sessions_artifacts_and_retires_the_flat_project() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let scenario = scenario();
        let legacy_sessions = scenario.fixture.legacy_sessions();
        let before = directory_names(&legacy_sessions).expect("legacy sessions");
        assert_eq!(before.len(), 5, "legacy fixture dirs: {before:?}");

        let store = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd)
            .expect("migrated store opens");
        assert_migrated(&scenario, &store);

        // Artifact URIs keep resolving through the routed store (§6.3 step 5).
        let router = ArtifactRouter::open(scenario.fixture.v2()).expect("artifact router");
        for (digest, content) in [
            (
                &scenario.only_first,
                b"referenced by the first tree only".as_slice(),
            ),
            (
                &scenario.only_second,
                b"referenced by the second tree only".as_slice(),
            ),
            (
                &scenario.shared_between_trees,
                b"referenced by two trees".as_slice(),
            ),
            (&scenario.unreferenced, b"referenced by nobody".as_slice()),
        ] {
            let page = router.read_paged(digest, 0, 10).expect("artifact page");
            assert_eq!(page.content, String::from_utf8_lossy(content));
        }

        // A second open sees the completed marker and does no work at all.
        drop(store);
        let reopened = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd)
            .expect("reopened store");
        for id in all_ids(&scenario) {
            assert_eq!(reopened.summary(id).expect("summary").meta.session_id, id);
        }
        assert!(!reopened.is_flat_layout());
        // Nothing was duplicated by the second open.
        assert_eq!(
            directory_names(&scenario.fixture.v2())
                .expect("v2 directories")
                .iter()
                .filter(|name| name.parse::<SessionId>().is_ok())
                .count(),
            2
        );
    }

    #[test]
    fn migration_resumes_from_the_journal_after_any_phase() {
        for phase in ["scaffold", "roots", "children", "artifacts"] {
            let _serial = SERIAL
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let scenario = scenario();
            set_crash_after(Some(phase));
            let aborted = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd);
            set_crash_after(None);
            let error = match aborted {
                Ok(_) => panic!("migration must stop after {phase}"),
                Err(error) => error,
            };
            assert!(
                matches!(&error, SessionError::Migration(message) if message.contains("injected crash")),
                "unexpected abort for {phase}: {error}"
            );
            assert!(
                scenario.fixture.legacy().join(JOURNAL_FILE).is_file(),
                "no journal to resume from after {phase}"
            );

            // Forget the journal's progress: every move is existence-checked, so
            // the resume still completes without touching what already moved.
            let journal_path = scenario.fixture.legacy().join(JOURNAL_FILE);
            let mut journal = Journal::load(&scenario.fixture.legacy());
            let moved = journal.done.len();
            journal.done.clear();
            journal
                .store(&scenario.fixture.legacy())
                .expect("rewrite journal");
            let store = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd)
                .expect("resumed migration");
            assert!(moved > 0 || phase == "scaffold", "no progress to forget");
            assert!(!journal_path.exists(), "journal outlived the migration");
            assert_migrated(&scenario, &store);
        }
    }

    /// End-to-end check against a copy of a real store (§8.2 #1 at scale). Run
    /// with `COOKIE_SMOKE_DATA=<data root> COOKIE_SMOKE_CWD=<work dir> cargo test
    /// -p cookie_agent_engine --lib -- --ignored`.
    #[test]
    #[ignore = "needs COOKIE_SMOKE_DATA/COOKIE_SMOKE_CWD pointing at a copied store"]
    fn migration_moves_a_copied_real_store() {
        use std::time::Instant;

        use crate::session::SessionSummary;

        let _serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let data = PathBuf::from(std::env::var("COOKIE_SMOKE_DATA").expect("COOKIE_SMOKE_DATA"));
        let cwd = PathBuf::from(std::env::var("COOKIE_SMOKE_CWD").expect("COOKIE_SMOKE_CWD"));
        let fixture = Fixture {
            _temp: tempfile::TempDir::new().expect("scratch"),
            data_root: data,
            cwd,
        };

        let before = SessionStore::open_flat_for_test(&fixture.data_root, &fixture.cwd)
            .expect("legacy store");
        let summaries: BTreeMap<SessionId, SessionSummary> = before
            .all_summaries()
            .into_iter()
            .map(|summary| (summary.meta.session_id, summary))
            .collect();
        let digests = artifact_inventory(&fixture.legacy_artifacts());
        let delegated = summaries
            .values()
            .filter(|summary| matches!(summary.meta.origin, SessionOrigin::Delegated { .. }))
            .count();
        eprintln!(
            "smoke pre-state: {} sessions, {} roots in metadata",
            summaries.len(),
            summaries
                .values()
                .filter(|summary| matches!(summary.meta.origin, SessionOrigin::Root))
                .count()
        );
        drop(before);

        let started = Instant::now();
        let store = SessionStore::open(&fixture.data_root, &fixture.cwd).expect("migrated store");
        let elapsed = started.elapsed();
        let v2 = fixture.v2();

        // The top level holds exactly the roots; each child sits in one root's
        // `subagents/` directory, matching its origin metadata.
        let placed_roots = session_ids_in(&v2);
        let placed_children: BTreeSet<SessionId> = placed_roots
            .iter()
            .flat_map(|root| session_ids_in(&v2.join(root.to_string()).join(SUBAGENTS_DIR)))
            .collect();
        let expected_roots: BTreeSet<SessionId> = summaries
            .iter()
            .filter(|(_, summary)| matches!(summary.meta.origin, SessionOrigin::Root))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(placed_roots, expected_roots);
        assert_eq!(
            placed_children,
            summaries
                .keys()
                .copied()
                .collect::<BTreeSet<_>>()
                .difference(&expected_roots)
                .copied()
                .collect::<BTreeSet<_>>()
        );
        // Child summaries come from each tree's load, which a freshly migrated
        // store has not performed yet: pull every root in before comparing.
        for root in &placed_roots {
            store.get(*root).expect("tree load");
        }
        for (id, summary) in &summaries {
            let after = store.summary(*id).expect("summary");
            assert_eq!(after.meta, summary.meta, "session {id} metadata");
            // A v1 metadata cache can lag its own log (it is rebuilt on fold),
            // so the migrated store may report more: it must never report less.
            assert!(
                after.usage_rollup.input_tokens >= summary.usage_rollup.input_tokens
                    && after.usage_rollup.output_tokens >= summary.usage_rollup.output_tokens
                    && after.usage_rollup.request_count >= summary.usage_rollup.request_count,
                "session {id} lost usage: {:?} -> {:?}",
                summary.usage_rollup,
                after.usage_rollup
            );
        }
        let placed: BTreeSet<String> = artifact_inventory(&v2.join(SHARED_ARTIFACTS_DIR))
            .into_iter()
            .chain(placed_roots.iter().flat_map(|root| {
                artifact_inventory(&v2.join(root.to_string()).join(ARTIFACTS_DIR))
            }))
            .collect();
        assert_eq!(
            placed,
            digests.iter().cloned().collect::<BTreeSet<String>>(),
            "artifact digests not conserved"
        );
        for (digest, tree) in ledger_entries(&v2) {
            assert!(placed.contains(&digest), "ledger names a missing digest");
            assert!(
                summaries.contains_key(&tree),
                "ledger names an unknown tree"
            );
        }

        // Every moved blob still hashes to its own name through the router.
        let router = ArtifactRouter::open(v2.clone()).expect("router");
        let mut verified = 0;
        for digest in &digests {
            let file = router
                .open_existing(digest)
                .expect("open artifact")
                .unwrap_or_else(|| panic!("{digest} lost"));
            let bytes = io_read_to_end(file);
            assert_eq!(digest_of(&bytes), *digest, "bytes changed for {digest}");
            verified += 1;
        }
        eprintln!(
            "smoke: {} sessions ({delegated} delegated), {} artifacts verified in {:?}",
            summaries.len(),
            verified,
            elapsed
        );
    }

    /// The session ids whose directories sit directly in `directory`.
    fn session_ids_in(directory: &Path) -> BTreeSet<SessionId> {
        directory_names(directory)
            .expect("session directories")
            .into_iter()
            .filter_map(|name| name.parse::<SessionId>().ok())
            .collect()
    }

    /// Digest-named blobs in one artifact directory, sorted.
    fn artifact_inventory(directory: &Path) -> Vec<String> {
        file_names(directory)
            .expect("artifact inventory")
            .into_iter()
            .filter(|name| is_digest_name_common(name))
            .collect()
    }

    fn io_read_to_end(mut file: fs::File) -> Vec<u8> {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes).expect("read artifact");
        bytes
    }

    #[test]
    fn migration_refuses_to_run_while_a_session_is_owned() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let scenario = scenario();
        let owned = scenario
            .fixture
            .legacy_sessions()
            .join(scenario.children[2].to_string());
        let held = match try_acquire(&owned).expect("acquire fixture lock") {
            SessionOwnership::Owned(held) => held,
            SessionOwnership::Foreign => panic!("fixture lock should be free"),
        };

        let error = match SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd) {
            Ok(_) => panic!("migration must refuse while a session is owned"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, SessionError::Migration(message) if message.contains("owned by a live process")),
            "unexpected refusal: {error}"
        );
        // The flat store is untouched and still serves the legacy layout.
        assert!(owned.join(LEGACY_SESSION_META_FILE).is_file());
        assert!(!scenario.fixture.v2().join(LAYOUT_MARKER_FILE).exists());

        drop(held);
        let store = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd)
            .expect("migration runs once the owner is gone");
        assert_migrated(&scenario, &store);
    }
}
