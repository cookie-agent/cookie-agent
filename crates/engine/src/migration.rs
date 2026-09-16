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
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::{
    events::{EventLog, fsync_directory},
    ownership::{HeldLock, SessionOwnership, owner_lock_path, try_acquire},
    runtime::artifacts::{
        ARTIFACTS_DIR, CROSS_REFS_FILE, SHARED_ARTIFACTS_DIR, is_digest_name_common,
        scan_artifact_references_in_log, write_cross_ref_ledger,
    },
    session::{
        EVENTS_FILE, LAYOUT_MARKER_FILE, LAYOUT_VERSION, LEGACY_SESSION_META_FILE,
        PROJECT_CWD_FILE, SESSION_META_FILE, SUBAGENTS_DIR, SessionError, SessionStore, meta_path,
        projection,
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
    let legacy_artifacts = legacy_root.join(ARTIFACTS_DIR);
    let target = SessionStore::resolve_workdir_dir(data_root, cwd);
    if !legacy_sessions.is_dir() {
        // Nothing is left to move. The finishing window is the one place where
        // the work can be done while the marker still says `in-progress`:
        // `.migrated` is written before the flat directories are retired, so it
        // proves verification passed. Claim completion instead of leaving the
        // store in-progress forever.
        finish_claimed_completion(&legacy_root, &target, &legacy_sessions, &legacy_artifacts)?;
        return Ok(None);
    }
    if migration_state(&target).as_deref() == Some(MIGRATION_COMPLETE)
        && !has_remnants(&legacy_sessions)?
        && !has_remnants(&legacy_artifacts)?
    {
        // §6.1: the v2 store is live and the flat directory is a leftover. Point
        // old binaries at the new home and leave the remnant alone.
        ensure_tombstone(&legacy_root, &target)?;
        Journal::clear(&legacy_root)?;
        return Ok(None);
    }
    let _lock = MigrationLock::acquire(&legacy_root)?;
    // Nothing else writes here while the lock is held, so a temporary left by an
    // interrupted atomic write is litter; under it the flat project still counts
    // as unfinished and the store never gets to claim its migration is done.
    sweep_litter(&legacy_root, &target)?;
    // Plan first, then lock: knowing where each directory lands is what lets the
    // same process hold its owner lock across the rename (M1) instead of probing
    // and releasing.
    let mut plan = Plan::discover(&legacy_sessions, &target)?;
    let mut journal = Journal::load(&legacy_root);
    let resumed = journal.is_resumed();
    // Which caches were already unreadable before anything moved. Recorded so a
    // resume reports those sessions as repaired rather than as new damage; the
    // completion check itself rebuilds them from the event logs (§6.6).
    plan.opaque.extend(journal.opaque());
    // Every legacy owner lock, held until the last session directory has moved.
    let owners = OwnerLocks::acquire(&plan, &legacy_sessions, &target)?;
    journal.ensure_plan(&plan, &legacy_artifacts)?;
    progress(&format!(
        "{} session store migration… {} sessions in {}",
        if resumed { "resuming" } else { "starting" },
        plan.pending_total(),
        legacy_root.display()
    ));
    let outcome = migrate(&legacy_root, &target, &plan, &mut journal, progress, owners)?;
    progress(&format!(
        "migrating session store… done ({} roots, {} children, {} artifacts)",
        outcome.roots, outcome.children, outcome.artifacts
    ));
    Ok(Some(outcome))
}

/// Anything the completed job left behind in a legacy directory: a stray file
/// means finalization is not finished, so the marker must not claim it is (B1).
fn has_remnants(directory: &Path) -> Result<bool, SessionError> {
    if !directory.exists() {
        return Ok(false);
    }
    Ok(!directory_is_empty(directory)?)
}

/// Close the one finishing window that can outlive its work: verification
/// passed, `.migrated` and the tombstone are down, the flat directories are
/// gone, but a crash landed before `layout.json` was flipped to `complete`.
/// Claims the completion the store already earned instead of stranding it in
/// `in-progress` with no legacy directory left to resume from.
fn finish_claimed_completion(
    legacy_root: &Path,
    target: &Path,
    legacy_sessions: &Path,
    legacy_artifacts: &Path,
) -> Result<(), SessionError> {
    if migration_state(target).as_deref() != Some(MIGRATION_IN_PROGRESS) {
        return Ok(());
    }
    // Every finalization step has to have landed: `.migrated` is written only
    // after verification, and the retirement of both flat directories is what
    // `finish` reports as retired.
    if !legacy_root.join(MIGRATED_MARKER_FILE).is_file()
        || !legacy_root.join(TOMBSTONE_FILE).is_file()
        || legacy_sessions.exists()
        || legacy_artifacts.exists()
    {
        return Ok(());
    }
    sweep_litter(legacy_root, target)?;
    write_layout_marker(target, MIGRATION_COMPLETE)?;
    fsync_directory_tolerant(target)
}

/// Steps 2 through 6 of §6.3, resumable at any point.
fn migrate(
    legacy_root: &Path,
    target: &Path,
    plan: &Plan,
    journal: &mut Journal,
    progress: &dyn Fn(&str),
    owners: OwnerLocks,
) -> Result<Outcome, SessionError> {
    let legacy_sessions = legacy_root.join(LEGACY_SESSIONS_DIR);
    let legacy_artifacts = legacy_root.join(ARTIFACTS_DIR);

    // 2. SCAFFOLD: reserve the v2 work dir and move the project-level files.
    write_layout_marker(target, MIGRATION_IN_PROGRESS)?;
    for file in PROJECT_FILES {
        let source = legacy_root.join(file);
        if source.is_file() && !target.join(file).exists() {
            rename(&source, &target.join(file))?;
        }
        crash_after("project-file", Some(file))?;
    }
    crash_after("scaffold", None)?;

    // 3. ROOTS first so every child has a `subagents/` parent to land in.
    //    Every planned root goes through `move_session`, not only the still-flat
    //    ones: the call also reconciles components a crash left half-moved (B2).
    for root in &plan.roots {
        let source = legacy_sessions.join(root.to_string());
        let destination = target.join(root.to_string());
        journal.consistent(&source, "roots", *root)?;
        move_session(&source, &destination)?;
        ensure_private_dir(&destination.join(ARTIFACTS_DIR))?;
        ensure_private_dir(&destination.join(SUBAGENTS_DIR))?;
        journal.done("roots", *root)?;
        crash_after("root", Some(&root.to_string()))?;
    }
    progress(&format!(
        "migrating session store… {} roots moved",
        plan.pending_roots().count()
    ));
    crash_after("roots", None)?;

    // 4. CHILDREN under their discovered root.
    for (child, root) in plan.all_children() {
        let source = legacy_sessions.join(child.to_string());
        let destination = target
            .join(root.to_string())
            .join(SUBAGENTS_DIR)
            .join(child.to_string());
        journal.consistent(&source, "children", child)?;
        move_session(&source, &destination)?;
        journal.done("children", child)?;
        crash_after("child", Some(&child.to_string()))?;
    }
    progress(&format!(
        "migrating session store… {} child sessions moved",
        plan.pending_children().count()
    ));
    // No session directory moves after this point, so the owner locks can go and
    // the sidecar files that could not be renamed while held can.
    owners.release()?;
    crash_after("children", None)?;

    // 5. ARTIFACTS: place each digest in the tree that references it, shared
    // otherwise (§6.3 step 5, §6.5). The inventory is the durable plan record
    // plus whatever the flat store holds now, so a resumed run reconciles blobs
    // already moved as well as those still to place (B3).
    let references = tree_references(target, plan)?;
    let mut outcome = Outcome {
        roots: plan.roots.len(),
        children: plan.children.len(),
        ..Outcome::default()
    };
    let mut ledger = BTreeSet::new();
    let mut touched: BTreeSet<PathBuf> = BTreeSet::new();
    // The flat artifact directory is the source parent of every rename below;
    // one sync at the end of the batch makes the removals durable (M2).
    touched.insert(legacy_artifacts.clone());
    let mut names = journal.artifact_inventory()?;
    names.extend(file_names(&legacy_artifacts)?);
    for name in &names {
        let placement = is_digest_name_common(name)
            .then(|| placement_root(&references, name))
            .flatten();
        let destination = match placement {
            Some(root) => {
                outcome.artifacts += 1;
                let trees = references.get(name.as_str()).into_iter().flatten();
                for tree in trees.filter(|tree| **tree != root) {
                    ledger.insert((name.clone(), *tree));
                }
                target.join(root.to_string()).join(ARTIFACTS_DIR).join(name)
            }
            None => {
                outcome.shared_artifacts += 1;
                target.join(SHARED_ARTIFACTS_DIR).join(name)
            }
        };
        let source = legacy_artifacts.join(name);
        place_blob(&source, &destination, &mut touched)?;
        crash_after("artifact", Some(name))?;
    }
    // Renames are cheap; one sync per touched directory is enough to make the
    // batch durable (§6.3 step 5), which a sync per blob would not be.
    for directory in &touched {
        // A flat artifact directory that never existed — a store that retained no
        // output — has nothing to make durable, and neither has its parent.
        fsync_directory_tolerant(directory)?;
    }
    if !ledger.is_empty() {
        write_cross_ref_ledger(&target.join(CROSS_REFS_FILE), &ledger).map_err(|source| {
            SessionError::Io {
                path: target.join(CROSS_REFS_FILE),
                source,
            }
        })?;
        fsync_directory(target)?;
        outcome.cross_refs = ledger.len();
    }
    progress(&format!(
        "migrating session store… {} artifacts placed, {} shared",
        outcome.artifacts, outcome.shared_artifacts
    ));
    crash_after("artifacts", None)?;

    // 6. VERIFY against the durable plan, retire the flat project, and only then
    // let the marker claim completion (B1). `before_verify` is the last crash
    // point with a journal behind a fully moved store: everything after it is
    // the finishing window, which the `retire`/`complete` points exercise.
    crash_after("before_verify", None)?;
    verify(target, &legacy_sessions, &legacy_artifacts, plan, journal)?;
    // The journal is dropped as soon as the moves are known good, so a crash in
    // what follows resumes from a verified plan rather than a stale record.
    Journal::clear(legacy_root)?;
    let retired = finish(legacy_root, target, &legacy_sessions, &legacy_artifacts)?;
    crash_after("retire", None)?;
    if retired {
        write_layout_marker(target, MIGRATION_COMPLETE)?;
        fsync_directory(target)?;
    } else {
        // Something an old binary dropped is still in the flat project. The
        // marker stays in-progress so the next open finishes it: an incomplete
        // finalization must never become authoritative (B1).
        write_layout_marker(target, MIGRATION_IN_PROGRESS)?;
        fsync_directory(target)?;
        progress(
            "session store migration… legacy project still holds files, finishing on next open",
        );
    }
    // The marker writes above leave their own temporaries behind when a crash
    // lands between the sync and the rename.
    sweep_litter(legacy_root, target)?;
    crash_after("complete", None)?;
    Ok(outcome)
}

/// Move one artifact into its destination store, reconciling whatever a previous,
/// interrupted run left behind (B3).
fn place_blob(
    source: &Path,
    destination: &Path,
    touched: &mut BTreeSet<PathBuf>,
) -> Result<(), SessionError> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(destination);
    if !parent.is_dir() {
        ensure_private_dir(parent)?;
    }
    match (source.is_file(), destination.is_file()) {
        (true, true) => {
            // Both exist: the rename survived and its source-side removal did
            // not. Keep the placed copy, and drop the duplicate only when the
            // bytes agree — a mismatch is corruption, not a resume.
            if read_digest(source)? == read_digest(destination)? {
                fs::remove_file(source).map_err(|source_error| SessionError::Io {
                    path: source.to_owned(),
                    source: source_error,
                })?;
            } else {
                return Err(SessionError::Migration(format!(
                    "{} and {} hold different bytes for the same artifact",
                    source.display(),
                    destination.display()
                )));
            }
        }
        (true, false) => {
            rename(source, destination)?;
            touched.insert(parent.to_path_buf());
        }
        // Already placed by the run that crashed.
        (false, true) => {}
        // The blob is in neither store. It was there when the plan was recorded,
        // so this is a loss, not a skip: refuse to call the store migrated.
        (false, false) => {
            return Err(SessionError::Migration(format!(
                "artifact {} vanished from both {} and {}",
                destination
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                source.display(),
                destination.display()
            )));
        }
    }
    Ok(())
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
///
/// Fail-closed: an unreadable log would make its blobs look unreferenced and
/// send them to the shared store while the migration still claimed success, so
/// the scan error aborts the job and leaves `.migrating` for the next open.
fn tree_references(
    target: &Path,
    plan: &Plan,
) -> Result<BTreeMap<String, BTreeSet<SessionId>>, SessionError> {
    let mut references: BTreeMap<String, BTreeSet<SessionId>> = BTreeMap::new();
    for root in &plan.roots {
        let directory = target.join(root.to_string());
        let mut live = HashSet::new();
        let root_log = directory.join(EVENTS_FILE);
        absorb(
            scan_artifact_references_in_log(&root_log),
            &root_log,
            &mut live,
        )?;
        for child in plan.children_of(*root) {
            let child_log = directory
                .join(SUBAGENTS_DIR)
                .join(child.to_string())
                .join(EVENTS_FILE);
            absorb(
                scan_artifact_references_in_log(&child_log),
                &child_log,
                &mut live,
            )?;
        }
        for digest in live {
            references.entry(digest).or_default().insert(*root);
        }
    }
    Ok(references)
}

/// Fold one log's references into the set. A planned session must have a
/// readable event log: the low-level event reader can represent a missing file
/// as an empty byte stream, but `EventLog::open_read_only` rejects that stream
/// because it has no `SessionCreated` record. Migration therefore fails closed
/// rather than silently treating a missing session history as empty.
fn absorb(
    result: std::io::Result<HashSet<String>>,
    path: &Path,
    into: &mut HashSet<String>,
) -> Result<(), SessionError> {
    match result {
        Ok(found) => {
            into.extend(found);
            Ok(())
        }
        Err(source) => Err(SessionError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// §6.6. Any mismatch aborts before the completion marker is written, leaving
/// `.migrating` and an in-progress marker for the next open to resume.
fn verify(
    target: &Path,
    legacy_sessions: &Path,
    legacy_artifacts: &Path,
    plan: &Plan,
    journal: &Journal,
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
        check_placed(&directory, *root, plan)?;
        if !directory.join(ARTIFACTS_DIR).is_dir() || !directory.join(SUBAGENTS_DIR).is_dir() {
            return Err(SessionError::Migration(format!(
                "{} is missing artifacts/ or subagents/",
                directory.display()
            )));
        }
        let subagents = directory.join(SUBAGENTS_DIR);
        for name in directory_names(&subagents)? {
            let Ok(child) = name.parse::<SessionId>() else {
                continue;
            };
            *placed_children.entry(child).or_default() += 1;
            check_placed(&subagents.join(&name), child, plan)?;
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
    // Artifact inventory: every digest the flat store held when the plan was
    // recorded must now sit in exactly one place, with bytes matching its name.
    // Counting this invocation's moves would let a lost blob pass (§6.6, B3).
    let inventory = journal.artifact_inventory()?;
    let placed = placement_inventory(target, plan)?;
    let still_flat = file_names(legacy_artifacts)?;
    let still_flat: BTreeSet<&str> = still_flat.iter().map(String::as_str).collect();
    for name in &inventory {
        match placed.get(name).map(Vec::as_slice) {
            None if still_flat.contains(&name.as_str()) => {
                return Err(SessionError::Migration(format!(
                    "artifact {name} was never placed and is still in the flat store"
                )));
            }
            None => {
                return Err(SessionError::Migration(format!(
                    "artifact {name} is in neither the flat store nor its destination"
                )));
            }
            Some([path]) => {
                if is_digest_name_common(name) && read_digest(path)? != name.as_str() {
                    return Err(SessionError::Migration(format!(
                        "{} does not hash to its name",
                        path.display()
                    )));
                }
            }
            Some(paths) => {
                return Err(SessionError::Migration(format!(
                    "artifact {name} sits in {} places",
                    paths.len()
                )));
            }
        }
    }
    if let Some(leftover) = still_flat.iter().find(|name| inventory.contains(**name)) {
        return Err(SessionError::Migration(format!(
            "artifact {leftover} is still in the flat store"
        )));
    }
    Ok(())
}

/// Every artifact placement the migrated store can serve: the shared store plus
/// each tree's own store, keyed by file name.
fn placement_inventory(
    target: &Path,
    plan: &Plan,
) -> Result<BTreeMap<String, Vec<PathBuf>>, SessionError> {
    let mut placed: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut stores = vec![target.join(SHARED_ARTIFACTS_DIR)];
    for root in &plan.roots {
        stores.push(target.join(root.to_string()).join(ARTIFACTS_DIR));
    }
    for store in stores {
        for name in file_names(&store)? {
            placed
                .entry(name.clone())
                .or_default()
                .push(store.join(&name));
        }
    }
    Ok(placed)
}

/// One placed session (§6.6 check 2): scaffold present, and a metadata cache
/// that parses and names this session.
///
/// A cache that was already unreadable in the flat store grants no exemption:
/// the event log is authoritative, so an unusable cache is rebuilt from the log
/// and revalidated. A session that neither has a usable cache nor can rebuild
/// one aborts the migration rather than being declared complete.
fn check_placed(directory: &Path, id: SessionId, plan: &Plan) -> Result<(), SessionError> {
    if !directory.is_dir() {
        return Err(SessionError::Migration(format!(
            "{} is missing after the move",
            directory.display()
        )));
    }
    if metadata_is_current(directory, id)? {
        return Ok(());
    }
    if plan.opaque.contains(&id) {
        eprintln!(
            "migration: {} arrived without a usable cache; rebuilding it from the event log",
            directory.display()
        );
    }
    rebuild_metadata(directory, id)?;
    if metadata_is_current(directory, id)? {
        return Ok(());
    }
    Err(SessionError::Migration(format!(
        "{} still has no metadata cache naming {id} after rebuilding from its event log",
        directory.display()
    )))
}

/// Whether the cache in `directory` parses and declares `id`.
fn metadata_is_current(directory: &Path, id: SessionId) -> Result<bool, SessionError> {
    let path = meta_path(directory);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(SessionError::Io { path, source });
        }
    };
    // An unparseable cache is a hole in the cache, not in the session: the log
    // below rebuilds it.
    match serde_json::from_str::<SessionMeta>(&text) {
        Ok(meta) => Ok(meta.session_id == id),
        Err(_) => Ok(false),
    }
}

/// Replace the cache with one folded from the session's own event log, durably
/// and before anything is allowed to call the store migrated.
fn rebuild_metadata(directory: &Path, id: SessionId) -> Result<(), SessionError> {
    let cache = directory.join(SESSION_META_FILE);
    let events = directory.join(EVENTS_FILE);
    if !events.is_file() {
        return Err(SessionError::Migration(format!(
            "{} has no usable metadata cache and no {} to rebuild one from",
            directory.display(),
            EVENTS_FILE
        )));
    }
    let meta = projection(EventLog::open_read_only(events.clone(), id)?)?.meta;
    if meta.session_id != id {
        return Err(SessionError::Migration(format!(
            "{} describes session {}, not {id}",
            directory.display(),
            meta.session_id
        )));
    }
    let bytes = serde_json::to_vec_pretty(&meta).map_err(|source| SessionError::Json {
        path: cache.clone(),
        source,
    })?;
    write_atomically(&cache, &bytes, directory)?;
    if cache != directory.join(LEGACY_SESSION_META_FILE) {
        remove_file(&directory.join(LEGACY_SESSION_META_FILE))?;
    }
    Ok(())
}

/// Write the tombstone and drop the emptied legacy directories. `false` means
/// something is still in the flat project, so completion must not be claimed (B1).
fn finish(
    legacy_root: &Path,
    target: &Path,
    legacy_sessions: &Path,
    legacy_artifacts: &Path,
) -> Result<bool, SessionError> {
    write_atomically(
        &legacy_root.join(MIGRATED_MARKER_FILE),
        format!("{}\n", target.display()).as_bytes(),
        legacy_root,
    )?;
    write_tombstone(legacy_root, target)?;
    let mut retired = true;
    for directory in [legacy_sessions, legacy_artifacts] {
        if !directory.is_dir() {
            continue;
        }
        if directory_is_empty(directory)? {
            match fs::remove_dir(directory) {
                Ok(()) => {}
                // A racing removal is the outcome we wanted; anything else is not.
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    retired = false;
                }
                Err(_) => {}
            }
        } else {
            retired = false;
        }
    }
    fsync_directory(legacy_root).map_err(SessionError::Event)?;
    Ok(retired)
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

    /// Where every planned session belongs in the v2 layout, whether or not it
    /// has already moved.
    fn placements(&self, target: &Path) -> Vec<(SessionId, PathBuf)> {
        self.roots
            .iter()
            .map(|root| (*root, target.join(root.to_string())))
            .chain(self.children.iter().map(|(child, root)| {
                (
                    *child,
                    target
                        .join(root.to_string())
                        .join(SUBAGENTS_DIR)
                        .join(child.to_string()),
                )
            }))
            .collect()
    }

    /// Every planned child, pending or already moved: `move_session` reconciles
    /// components a crash left half-moved, so it must run for all of them.
    fn all_children(&self) -> impl Iterator<Item = (SessionId, SessionId)> + '_ {
        self.children.iter().map(|(child, root)| (*child, *root))
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

/// Journal state: one plan record followed by one line per moved session, each
/// fsynced before the next step starts.
#[derive(Debug, Default)]
struct Journal {
    legacy_root: PathBuf,
    plan: Option<PlanRecord>,
    moved: BTreeSet<String>,
}

/// What the store looked like before the first rename, recorded durably so a
/// resumed run can reconcile and verify against the *original* state (B3, W3).
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct PlanRecord {
    /// Every legacy session directory, by id.
    #[serde(default)]
    roots: Vec<String>,
    /// `(child, root)` placements as discovered before any move.
    #[serde(default)]
    children: Vec<(String, String)>,
    /// Sessions whose metadata cache was already unreadable (W3).
    #[serde(default)]
    opaque: Vec<String>,
    /// Every file name in the flat artifact store (B3).
    #[serde(default)]
    artifacts: Vec<String>,
}

/// One journal line.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
enum Record {
    Plan(PlanRecord),
    Moved { phase: String, id: String },
}

impl Journal {
    fn load(legacy_root: &Path) -> Self {
        let mut journal = Self {
            legacy_root: legacy_root.to_path_buf(),
            ..Self::default()
        };
        let Ok(bytes) = fs::read(legacy_root.join(JOURNAL_FILE)) else {
            return journal;
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            // A torn final line is a crash mid-append: the record it describes
            // never completed, so skipping it is the resume.
            let Ok(record) = serde_json::from_str::<Record>(line) else {
                continue;
            };
            match record {
                Record::Plan(plan) => journal.plan = Some(plan),
                Record::Moved { phase, id } => {
                    journal.moved.insert(format!("{phase}:{id}"));
                }
            }
        }
        journal
    }

    /// Whether a previous, interrupted run left a plan behind.
    fn is_resumed(&self) -> bool {
        self.plan.is_some()
    }

    /// Append the pre-move snapshot once, durably, before anything moves.
    fn ensure_plan(&mut self, plan: &Plan, legacy_artifacts: &Path) -> Result<(), SessionError> {
        if self.plan.is_some() {
            return Ok(());
        }
        let record = PlanRecord {
            roots: plan.roots.iter().map(ToString::to_string).collect(),
            children: plan
                .children
                .iter()
                .map(|(child, root)| (child.to_string(), root.to_string()))
                .collect(),
            opaque: plan.opaque.iter().map(ToString::to_string).collect(),
            artifacts: file_names(legacy_artifacts)?,
        };
        self.append(&Record::Plan(record.clone()))?;
        self.plan = Some(record);
        Ok(())
    }

    /// The original flat artifact inventory.
    fn artifact_inventory(&self) -> Result<BTreeSet<String>, SessionError> {
        Ok(self
            .plan
            .as_ref()
            .map(|record| record.artifacts.iter().cloned().collect())
            .unwrap_or_default())
    }

    /// Sessions recorded as having unreadable metadata before any move.
    fn opaque(&self) -> BTreeSet<SessionId> {
        self.plan
            .as_ref()
            .map(|record| {
                record
                    .opaque
                    .iter()
                    .filter_map(|name| name.parse::<SessionId>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Rewind to the recorded plan: keep the plan, drop every move record.
    ///
    /// Verification clears the journal, so a surviving record describes a phase
    /// that did not finish. Once that phase's work has landed the record is stale,
    /// and `reconcile_move` has nothing left to trust: the phase has to run again
    /// because something put the directory back where the plan says it already
    /// moved (an interrupted `rename_metadata`, a file an old binary dropped into
    /// the retired flat store). Rewinding makes a correction after the move
    /// resumable instead of a permanent refusal — `move_session` is idempotent and
    /// the durable plan still backs the artifact check. Returns how many records
    /// were dropped.
    fn forget_moves(&mut self) -> Result<usize, SessionError> {
        let path = self.legacy_root.join(JOURNAL_FILE);
        let Ok(text) = fs::read_to_string(&path) else {
            self.moved.clear();
            return Ok(0);
        };
        let mut kept = String::new();
        let mut dropped = 0;
        for line in text.lines() {
            let parsed = serde_json::from_str::<serde_json::Value>(line).ok();
            if matches!(&parsed, Some(value) if value["t"] == "moved") {
                dropped += 1;
                continue;
            }
            kept.push_str(line);
            kept.push('\n');
        }
        if dropped > 0 {
            write_atomically(&path, kept.as_bytes(), &self.legacy_root)?;
            self.moved.clear();
        }
        Ok(dropped)
    }

    /// Whether the journal's own record of a move contradicts the store.
    fn contradicts(&self, source: &Path, phase: &str, id: SessionId) -> bool {
        self.moved.contains(&format!("{phase}:{id}")) && source.is_dir()
    }

    fn done(&mut self, phase: &str, id: SessionId) -> Result<(), SessionError> {
        self.append(&Record::Moved {
            phase: phase.to_owned(),
            id: id.to_string(),
        })?;
        self.moved.insert(format!("{phase}:{id}"));
        Ok(())
    }

    fn append(&self, record: &Record) -> Result<(), SessionError> {
        let path = self.legacy_root.join(JOURNAL_FILE);
        let mut line = serde_json::to_vec(record).map_err(|source| SessionError::Json {
            path: path.clone(),
            source,
        })?;
        line.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|source| SessionError::Io {
                path: path.clone(),
                source,
            })?;
        file.write_all(&line)
            .and_then(|()| file.sync_all())
            .map_err(|source| SessionError::Io {
                path: path.clone(),
                source,
            })?;
        drop(file);
        fsync_directory(&self.legacy_root)?;
        Ok(())
    }

    /// A resume must not find a session the journal already finished still
    /// sitting in the flat layout: that means two migrations raced.
    fn consistent(
        &mut self,
        source: &Path,
        phase: &str,
        id: SessionId,
    ) -> Result<(), SessionError> {
        if self.contradicts(source, phase, id) {
            // The move record is history that stopped being true; rewind to the
            // plan and let this phase run again over what is really there.
            if self.forget_moves()? == 0 || self.contradicts(source, phase, id) {
                return Err(SessionError::Migration(format!(
                    "journal records {id} as moved, but {} still exists",
                    source.display()
                )));
            }
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

/// Every legacy session's owner lock, held from the first rename until the last
/// session directory has landed, so no live process can slip in behind the probe
/// (§6.3 step 0, M1).
struct OwnerLocks {
    held: Vec<HeldLock>,
    /// Windows files `owner.lock` *beside* its session directory; the rename
    /// cannot happen while the lock is held, so it is deferred to `release`.
    sidecars: Vec<(PathBuf, PathBuf)>,
}

impl OwnerLocks {
    fn acquire(plan: &Plan, legacy_sessions: &Path, target: &Path) -> Result<Self, SessionError> {
        let mut locks = Self {
            held: Vec::new(),
            sidecars: Vec::new(),
        };
        for (id, destination) in plan.placements(target) {
            let flat = legacy_sessions.join(id.to_string());
            let directory = if flat.is_dir() { &flat } else { &destination };
            match try_acquire(directory) {
                // A stale lock is ours to take; it moves with its session.
                Ok(SessionOwnership::Owned(held)) => {
                    locks.held.push(held);
                    let source_lock = owner_lock_path(directory);
                    if source_lock.parent() != Some(directory.as_ref()) && source_lock.is_file() {
                        locks
                            .sidecars
                            .push((source_lock, owner_lock_path(&destination)));
                    }
                }
                Ok(SessionOwnership::Foreign) => {
                    return Err(SessionError::Migration(format!(
                        "session {id} is owned by a live process; quit cookie and retry"
                    )));
                }
                Err(source) => {
                    return Err(SessionError::Io {
                        path: owner_lock_path(directory),
                        source,
                    });
                }
            }
        }
        Ok(locks)
    }

    /// Drop every lock, then move the sidecar files that could not move with it.
    fn release(self) -> Result<(), SessionError> {
        let Self { held, sidecars, .. } = self;
        drop(held);
        for (source, destination) in sidecars {
            if source.is_file() && !destination.is_file() {
                rename(&source, &destination)?;
            }
        }
        Ok(())
    }
}

/// Test seam: abort the migration right after `phase` completes, leaving the
/// disk and journal exactly as a crash would (§8.2 test 2).
/// `item` names the individual rename, capture or finalization step so a test can
/// stop anywhere, not only at phase boundaries (W2).
#[cfg(test)]
fn crash_after(phase: &str, item: Option<&str>) -> Result<(), SessionError> {
    if injected_crash_matches(phase, item) {
        let suffix = item.unwrap_or("");
        return Err(SessionError::Migration(format!(
            "injected crash after {phase}{suffix}"
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn set_crash_after(phase: Option<&str>, item: Option<&str>) {
    let mut slot = CRASH_AFTER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = phase.map(|phase| (phase.to_owned(), item.map(str::to_owned)));
}

#[cfg(test)]
fn injected_crash_matches(phase: &str, item: Option<&str>) -> bool {
    let slot = CRASH_AFTER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some((crash, crash_item)) = slot.as_ref() else {
        return false;
    };
    crash == phase
        && crash_item
            .as_deref()
            .is_none_or(|want| want == item.unwrap_or(""))
}

#[cfg(test)]
static CRASH_AFTER: std::sync::Mutex<Option<(String, Option<String>)>> =
    std::sync::Mutex::new(None);

#[cfg(not(test))]
fn crash_after(_phase: &str, _item: Option<&str>) -> Result<(), SessionError> {
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

/// Move one session directory, then reconcile each of its components
/// independently. A crash can land between the directory rename and the metadata
/// rename, so re-running this repairs whatever the destination is missing (B2).
fn move_session(source: &Path, destination: &Path) -> Result<(), SessionError> {
    if !destination.is_dir() {
        if !source.is_dir() {
            return Err(SessionError::Migration(format!(
                "session directory {} vanished mid-migration",
                source.display()
            )));
        }
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(destination);
        ensure_private_dir(parent)?;
        rename(source, destination)?;
    }
    rename_metadata(destination)?;
    // Windows keeps the ownership lock beside the directory; unix moves it along
    // inside it. `OwnerLocks::release` relocates the sidecars once held.
    let source_lock = owner_lock_path(source);
    if source_lock.parent() != Some(source) && source_lock.is_file() {
        rename(&source_lock, &owner_lock_path(destination))?;
    }
    Ok(())
}

/// `meta.json` → `metadata`, in either layout position, without ever losing a
/// cache: two copies are only acceptable when they are the same bytes.
fn rename_metadata(directory: &Path) -> Result<(), SessionError> {
    let legacy = directory.join(LEGACY_SESSION_META_FILE);
    if !legacy.is_file() {
        return Ok(());
    }
    let current = directory.join(SESSION_META_FILE);
    if current.is_file() {
        if read_digest(&legacy)? != read_digest(&current)? {
            return Err(SessionError::Migration(format!(
                "{} has two different metadata caches",
                directory.display()
            )));
        }
        return remove_file(&legacy);
    }
    rename(&legacy, &current)
}

fn remove_file(path: &Path) -> Result<(), SessionError> {
    match fs::remove_file(path) {
        Ok(()) => {
            fsync_directory(&parent_of(path))?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SessionError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn parent_of(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(path)
        .to_path_buf()
}

/// Rename and make *both* parents durable: syncing only the destination can roll
/// the removal of the source entry back, leaving the item in neither place (M2).
fn rename(source: &Path, destination: &Path) -> Result<(), SessionError> {
    match fs::rename(source, destination) {
        Ok(()) => {
            fsync_directory(&parent_of(destination))?;
            let source_parent = parent_of(source);
            if source_parent != parent_of(destination) {
                fsync_directory_tolerant(&source_parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source_error) => Err(SessionError::Io {
            path: destination.to_owned(),
            source: source_error,
        }),
    }
}

/// A parent that no longer exists has nothing left to sync.
fn fsync_directory_tolerant(directory: &Path) -> Result<(), SessionError> {
    match fsync_directory(directory) {
        Ok(()) => Ok(()),
        // A parent that no longer exists has nothing left to sync.
        Err(crate::events::EventLogError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(())
        }
        Err(source) => Err(source.into()),
    }
}

/// The sha256 of a file's bytes, which for a placed artifact is its name.
fn read_digest(path: &Path) -> Result<String, SessionError> {
    let bytes = fs::read(path).map_err(|source| SessionError::Io {
        path: path.to_owned(),
        source,
    })?;
    Ok(digest_of(&bytes))
}

/// Content hash, matching the `artifact://sha256/<hex>` naming.
fn digest_of(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

/// Write `path` through a temporary in its own directory, then fsync.
///
/// The temporary name is unique per call and stale temporaries for this target
/// are removed first: a crash between the sync and the rename used to leave a
/// fixed-name temporary behind, whose `create_new` then failed for every later
/// attempt, wedging the migration permanently.
fn write_atomically(path: &Path, bytes: &[u8], parent: &Path) -> Result<(), SessionError> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = parent.join(format!(".{name}.{}.migration-tmp", Uuid::now_v7()));
    remove_stale_temparies(parent, &name)?;
    let mut file = fs::File::options()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        })?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| SessionError::Io {
            path: temporary.clone(),
            source,
        });
    drop(file);
    if let Err(failure) = written {
        let _ = fs::remove_file(&temporary);
        return Err(failure);
    }
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

/// Sweep every atomic-write temporary the migration procedure can leave in the
/// legacy project directory or beside the v2 layout marker.
///
/// A crash between [`write_atomically`]'s sync and its rename leaves a
/// `.<name>.<uuid>.migration-tmp` behind; the uuid makes the name one no other
/// writer can hold, so a temporary that outlived its process cannot belong to a
/// live write. Left in place it counts as a remnant of the flat project, and the
/// store never gets to claim its migration is done.
fn sweep_litter(legacy_root: &Path, target: &Path) -> Result<(), SessionError> {
    remove_stale_temparies(legacy_root, JOURNAL_FILE)?;
    remove_stale_temparies(legacy_root, MIGRATED_MARKER_FILE)?;
    remove_stale_temparies(legacy_root, TOMBSTONE_FILE)?;
    remove_stale_temparies(target, LAYOUT_MARKER_FILE)?;
    Ok(())
}

/// Drop temporaries an interrupted write left behind for this same target.
fn remove_stale_temparies(parent: &Path, name: &str) -> Result<(), SessionError> {
    let prefix = format!(".{name}.");
    let Ok(entries) = fs::read_dir(parent) else {
        return Ok(());
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            // An unlistable entry is not a temporary we can reason about; the
            // write below does not depend on it.
            Err(_) => continue,
        };
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name.starts_with(&prefix) && file_name.ends_with(".migration-tmp") {
            match fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(SessionError::Io {
                        path: entry.path(),
                        source,
                    });
                }
            }
        }
    }
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

fn directory_is_empty(directory: &Path) -> Result<bool, SessionError> {
    let mut entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(source) => {
            return Err(SessionError::Io {
                path: directory.to_owned(),
                source,
            });
        }
    };
    // An entry that cannot be read means emptiness is unprovable, which is not
    // the same as empty: claiming otherwise would finalize an unknown state.
    match entries.next() {
        None => Ok(true),
        Some(Ok(_)) => Ok(false),
        Some(Err(source)) => Err(SessionError::Io {
            path: directory.to_owned(),
            source,
        }),
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

    /// §8.2 test 2, at every injected point: crash, forget what the journal
    /// claims it finished, and require an idempotent completion in the identical
    /// final state. `before_verify`, `retire` and `complete` cover the finishing
    /// window, where the journal is already gone and the marker has not landed.
    #[test]
    fn migration_resumes_from_the_journal_after_any_phase() {
        for phase in [
            "project-file",
            "scaffold",
            "root",
            "roots",
            "child",
            "children",
            "artifact",
            "artifacts",
            "before_verify",
            "retire",
            "complete",
        ] {
            let _serial = SERIAL
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let scenario = scenario();
            set_crash_after(Some(phase), None);
            let aborted = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd);
            set_crash_after(None, None);
            let error = match aborted {
                Ok(_) => panic!("migration must stop after {phase}"),
                Err(error) => error,
            };
            assert!(
                matches!(&error, SessionError::Migration(message) if message.contains("injected crash")),
                "unexpected abort for {phase}: {error}"
            );
            let journal_path = scenario.fixture.legacy().join(JOURNAL_FILE);
            // Verification clears the journal, so only the crashes after it
            // resume without one.
            assert_eq!(
                journal_path.is_file(),
                !matches!(phase, "retire" | "complete"),
                "journal presence after a crash at {phase}"
            );

            // Forget the journal's progress: every move is existence-checked and
            // the durable plan record stays, so the resume still completes and
            // still verifies the original artifact inventory.
            let forgotten = Journal::load(&scenario.fixture.legacy())
                .forget_moves()
                .expect("rewinding the journal is durable");
            assert!(
                forgotten > 0
                    || matches!(phase, "project-file" | "scaffold" | "retire" | "complete"),
                "no progress to forget after {phase}"
            );
            let store = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd)
                .expect("resumed migration");
            assert!(!journal_path.exists(), "journal outlived the migration");
            assert_migrated(&scenario, &store);
        }
    }

    /// §6.3: the marker, journal and tombstone writes go through one temporary
    /// file. A crash between its sync and its rename must not wedge every later
    /// resume with `AlreadyExists`.
    #[test]
    fn a_stale_migration_temporary_does_not_wedge_the_resume() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let scenario = scenario();
        set_crash_after(Some("roots"), None);
        let aborted = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd);
        set_crash_after(None, None);
        assert!(aborted.is_err(), "injected crash after roots");

        // The exact litter a crash leaves: every marker helper has a temporary
        // synced next to its target but never renamed.
        let legacy = scenario.fixture.legacy();
        let v2 = scenario.fixture.v2();
        let stale = [
            legacy.join(format!(".{JOURNAL_FILE}.stale.migration-tmp")),
            legacy.join(format!(".{MIGRATED_MARKER_FILE}.stale.migration-tmp")),
            legacy.join(format!(".{TOMBSTONE_FILE}.stale.migration-tmp")),
            v2.join(format!(".{LAYOUT_MARKER_FILE}.stale.migration-tmp")),
        ];
        for path in &stale {
            fs::write(path, b"half-written marker").expect("drop stale temporary");
        }

        let store = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd)
            .expect("the resume must survive a stale temporary");
        assert_migrated(&scenario, &store);
        for path in &stale {
            assert!(!path.exists(), "{} outlived the resume", path.display());
        }
    }

    /// §6.3 step 3/4: one session move is a sequence of renames. A crash between
    /// the directory rename and the `meta.json` → `metadata` rename leaves a
    /// destination that the resume has to finish, not just accept.
    #[test]
    fn a_half_moved_session_is_repaired_by_the_resume() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let scenario = scenario();
        let legacy_sessions = scenario.fixture.legacy_sessions();
        let v2 = scenario.fixture.v2();
        let (child, root) = (scenario.children[0], scenario.roots[0]);
        let (other_child, other_root) = (scenario.children[2], scenario.roots[1]);

        // Stop the first run right after the roots landed, so a hand-made
        // half-move below is the interleaving a crash really produces.
        set_crash_after(Some("roots"), None);
        let aborted = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd);
        set_crash_after(None, None);
        assert!(aborted.is_err(), "injected crash after roots");

        // Crash between the directory rename and the metadata rename of a child.
        let destination = v2
            .join(root.to_string())
            .join(SUBAGENTS_DIR)
            .join(child.to_string());
        fs::create_dir_all(destination.parent().expect("subagents")).expect("subagents");
        fs::rename(legacy_sessions.join(child.to_string()), &destination).expect("move directory");
        assert!(destination.join(LEGACY_SESSION_META_FILE).is_file());
        assert!(!destination.join(SESSION_META_FILE).is_file());
        // And a root whose destination exists but never got its scaffold.
        fs::remove_dir_all(v2.join(other_root.to_string()).join(ARTIFACTS_DIR))
            .expect("drop the artifact store");
        fs::remove_dir_all(v2.join(other_root.to_string()).join(SUBAGENTS_DIR))
            .expect("drop the subagents store");

        let store = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd)
            .expect("migration repairs the half-finished moves");
        assert!(
            destination.join(SESSION_META_FILE).is_file(),
            "the metadata rename was never completed"
        );
        assert!(
            !destination.join(LEGACY_SESSION_META_FILE).exists(),
            "the legacy cache name survived the move"
        );
        assert!(
            v2.join(other_root.to_string()).join(ARTIFACTS_DIR).is_dir(),
            "the destination that already existed was never reconciled"
        );
        assert!(
            v2.join(other_root.to_string()).join(SUBAGENTS_DIR).is_dir(),
            "the destination that already existed was never reconciled"
        );
        assert_eq!(
            store
                .summary(child)
                .expect("child is reachable")
                .meta
                .session_id,
            child
        );
        assert_eq!(
            store
                .summary(other_child)
                .expect("other child is reachable")
                .meta
                .session_id,
            other_child
        );
        assert_migrated(&scenario, &store);
    }

    /// §6.6 check 4: verification compares against the inventory the plan
    /// recorded, so a blob that a previous invocation moved and that then goes
    /// missing cannot pass a resume by counting this invocation's moves.
    #[test]
    fn resume_fails_loudly_when_a_previously_moved_artifact_is_lost() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let scenario = scenario();
        set_crash_after(Some("artifacts"), None);
        let aborted = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd);
        set_crash_after(None, None);
        assert!(aborted.is_err(), "injected crash after artifacts");

        // A blob already out of the flat store, deleted before the resume: the
        // only evidence it existed is the durable inventory.
        let moved = scenario
            .fixture
            .v2()
            .join(scenario.roots[0].to_string())
            .join(ARTIFACTS_DIR)
            .join(&scenario.only_first);
        assert!(moved.is_file(), "fixture placed the blob");
        fs::remove_file(&moved).expect("lose a placed blob");

        let error = match SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd) {
            Ok(_) => panic!("a lost artifact must not verify"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, SessionError::Migration(message)
                if message.contains("vanished from both")),
            "unexpected refusal: {error}"
        );
        // A lost blob leaves the store resumable, never claimed-complete.
        assert_ne!(
            migration_state(&scenario.fixture.v2()).as_deref(),
            Some(MIGRATION_COMPLETE),
            "a lost artifact must not end in a completed migration"
        );
        // The store is still resumable: put the bytes back and nothing was lost.
        fs::write(&moved, b"referenced by the first tree only").expect("restore blob");
        let store = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd)
            .expect("the migration resumes after the repair");
        assert_migrated(&scenario, &store);
    }

    /// §6.6 check 2, with no exemption for a cache that was already unreadable:
    /// the event log is authoritative, so an unusable cache is rebuilt from it
    /// and validated before the store may be called migrated.
    #[test]
    fn an_unreadable_metadata_cache_is_rebuilt_from_the_event_log() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let scenario = scenario();
        let child = scenario.children[0];
        let directory = scenario.fixture.legacy_sessions().join(child.to_string());
        fs::write(
            directory.join(LEGACY_SESSION_META_FILE),
            b"{\"truncated\":\"by a lost writ",
        )
        .expect("corrupt the cache");

        let store = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd)
            .expect("the log rebuilds the cache");
        // An unreadable origin cannot name a parent, so the session is placed as
        // a root; what matters is that it is not carried along as a hole.
        let placed = scenario.fixture.v2().join(child.to_string());
        assert!(placed.is_dir(), "promoted to a root, as planned");
        let cache = placed.join(SESSION_META_FILE);
        let rebuilt: SessionMeta =
            serde_json::from_str(&fs::read_to_string(&cache).expect("rebuilt cache"))
                .expect("rebuilt cache parses");
        assert_eq!(rebuilt.session_id, child, "cache names its directory");
        assert!(
            !placed.join(LEGACY_SESSION_META_FILE).exists(),
            "the unreadable legacy cache survived"
        );
        assert!(placed.join(EVENTS_FILE).is_file(), "transcript intact");
        assert_eq!(
            store.summary(child).expect("summary").meta.session_id,
            child,
            "the session is reachable after the rebuild"
        );
        // Every other session of the store landed exactly as the round trip does.
        for id in [
            scenario.roots[0],
            scenario.roots[1],
            scenario.children[1],
            scenario.children[2],
        ] {
            assert_eq!(store.summary(id).expect("summary").meta.session_id, id);
        }
        assert!(
            !scenario.fixture.legacy_sessions().exists(),
            "the flat project was retired"
        );
        assert_eq!(
            migration_state(&scenario.fixture.v2()).as_deref(),
            Some(MIGRATION_COMPLETE),
            "a rebuilt cache is a verified cache"
        );
    }

    /// The same session with an unusable log too is damage, not a state to
    /// declare migrated.
    #[test]
    fn a_session_with_no_cache_and_no_usable_log_aborts_the_migration() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let scenario = scenario();
        // A session that references no artifact: damaging it cannot shift any
        // placement decision, so the repair below is a byte-for-byte undo.
        let damaged = {
            let store = SessionStore::open_flat_for_test(
                &scenario.fixture.data_root,
                &scenario.fixture.cwd,
            )
            .expect("legacy store");
            session(&store, SessionOrigin::Root)
        };
        let directory = scenario.fixture.legacy_sessions().join(damaged.to_string());
        let original_cache =
            fs::read(directory.join(LEGACY_SESSION_META_FILE)).expect("fixture cache");
        let original_log = fs::read(directory.join(EVENTS_FILE)).expect("fixture log");
        fs::write(directory.join(LEGACY_SESSION_META_FILE), b"not json at all")
            .expect("corrupt the cache");
        fs::remove_file(directory.join(EVENTS_FILE)).expect("lose the log");

        let error = match SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd) {
            Ok(_) => panic!("a session that cannot be rebuilt must abort the migration"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, SessionError::Migration(message)
                if message.contains("no events.jsonl to rebuild one from")),
            "unexpected refusal: {error}"
        );
        // Not half-migrated-and-served: the v2 marker never claimed completion,
        // and the journal is kept so a corrected resume can finish.
        assert_ne!(
            migration_state(&scenario.fixture.v2()).as_deref(),
            Some(MIGRATION_COMPLETE),
            "an unrebuildable session was declared migrated"
        );
        assert!(
            scenario.fixture.legacy().join(JOURNAL_FILE).is_file(),
            "the resume journal was dropped on a refusal"
        );

        // Repair the damage the refusal reports.
        let placed = scenario.fixture.v2().join(damaged.to_string());
        fs::write(placed.join(SESSION_META_FILE), &original_cache).expect("restore the cache");
        fs::write(placed.join(EVENTS_FILE), &original_log).expect("restore the log");
        let store = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd)
            .expect("the migration resumes after the repair");
        assert_eq!(
            store
                .summary(damaged)
                .expect("repaired session")
                .meta
                .session_id,
            damaged
        );
        assert_migrated(&scenario, &store);
    }

    /// §6.3 step 5: a log that cannot be scanned cannot silently send its blobs
    /// to the shared store while the migration reports success.
    #[test]
    fn an_unscannable_event_log_aborts_the_migration() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let scenario = scenario();
        let root = scenario.roots[0];
        let log = scenario
            .fixture
            .v2()
            .join(root.to_string())
            .join(EVENTS_FILE);

        // Move every session first, then damage one transcript: a scan that
        // cannot run must not be scanned as an empty one.
        set_crash_after(Some("children"), None);
        let aborted = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd);
        set_crash_after(None, None);
        assert!(aborted.is_err(), "injected crash after children");
        let held = log.with_extension("hold");
        fs::rename(&log, &held).expect("hide the log");
        fs::create_dir_all(&log).expect("the log path becomes a directory");

        let error = match SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd) {
            Ok(_) => panic!("an unreadable log must not be scanned as an empty one"),
            Err(error) => error,
        };
        assert!(
            !matches!(&error, SessionError::Migration(message)
                if message.contains("injected crash")),
            "unexpected abort: {error}"
        );
        assert_ne!(
            migration_state(&scenario.fixture.v2()).as_deref(),
            Some(MIGRATION_COMPLETE),
            "a failed scan must never end in a completed migration"
        );
        // Every blob is still findable: nothing was placed on the strength of a
        // scan that reported nothing.
        assert!(
            scenario
                .fixture
                .legacy_artifacts()
                .join(&scenario.shared_between_trees)
                .is_file()
        );

        fs::remove_dir_all(&log).expect("remove the fake log");
        fs::rename(&held, &log).expect("restore the log");
        let store = SessionStore::open(&scenario.fixture.data_root, &scenario.fixture.cwd)
            .expect("migration completes once the log is readable");
        assert_migrated(&scenario, &store);
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
        // §8.2: the delegation registry and the tree approval store are both
        // folded from session logs (`DelegationEventStore::open` over root logs,
        // `rebuild_approvals` over the same snapshots). Those folds are what a
        // migration must leave untouched, so compare their complete input.
        let flat_logs: BTreeMap<SessionId, PathBuf> = summaries
            .keys()
            .map(|id| {
                (
                    *id,
                    fixture
                        .legacy_sessions()
                        .join(id.to_string())
                        .join(EVENTS_FILE),
                )
            })
            .collect();
        let before_fold = fold_inputs(&flat_logs);
        eprintln!(
            "smoke pre-state: {} grant payloads across {} logs",
            before_fold.1.len(),
            before_fold.0.len()
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

        // The flat project keeps a pointer for builds that predate the tree.
        let legacy = fixture.legacy();
        assert!(
            legacy.join(MIGRATED_MARKER_FILE).is_file(),
            "the flat project was never marked migrated"
        );
        assert!(
            fs::read_to_string(legacy.join(TOMBSTONE_FILE))
                .expect("tombstone")
                .contains(&v2.display().to_string()),
            "the tombstone does not name the v2 work dir"
        );

        // The registry and approval folds see exactly what they saw before: same
        // payloads, same order, same restart-stable grants, session by session.
        let mut v2_logs = BTreeMap::new();
        for root in &placed_roots {
            v2_logs.insert(*root, v2.join(root.to_string()).join(EVENTS_FILE));
            let children = v2.join(root.to_string()).join(SUBAGENTS_DIR);
            for child in session_ids_in(&children) {
                v2_logs.insert(child, children.join(child.to_string()).join(EVENTS_FILE));
            }
        }
        assert_eq!(
            fold_inputs(&v2_logs),
            before_fold,
            "a migrated log folds differently"
        );

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

    /// Everything the startup passes fold from, per session: its durable payloads
    /// in log order, plus the store-wide set of restart-stable tree grants the
    /// approval store re-installs (§4.3). Comparing the two before and after a
    /// move is the registry/approval equivalence §8.2 asks for: a migration
    /// renames log files and may not change what they mean.
    fn fold_inputs(
        logs: &BTreeMap<SessionId, PathBuf>,
    ) -> (BTreeMap<SessionId, Vec<String>>, BTreeSet<String>) {
        let mut payloads = BTreeMap::new();
        let mut grants = BTreeSet::new();
        for (id, path) in logs {
            let log = EventLog::open_read_only(path.clone(), *id).expect("log reads");
            let mut ordered = Vec::new();
            for event in log.event_snapshot().iter() {
                ordered.push(serde_json::to_string(&event.payload).expect("payload serializes"));
                if let EventPayload::TreeApprovalGrantCommitted { grant } = &event.payload
                    && crate::session::restart_stable_grant(grant)
                {
                    grants.insert(serde_json::to_string(grant).expect("grant serializes"));
                }
            }
            payloads.insert(*id, ordered);
        }
        (payloads, grants)
    }

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
