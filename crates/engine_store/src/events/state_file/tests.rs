//! `state.json` against a parse of the whole log
//! (`docs/site/specs/forever-sessions.md` section 7). Every open in here that
//! does not read its log whole also parses it and asserts the two are equal
//! (`reference.rs`), so what most of these tests assert themselves is only which
//! way the log opened.

use std::path::{Path, PathBuf};

use sha2::Digest as _;
use tempfile::tempdir;

use super::*;
use crate::session::projection;

/// The digest of what every fold gives at every turn boundary of the golden logs,
/// and the [`STATE_VERSION`] it was recorded under. Recorded again, under the same
/// version, when the golden `reviewer` scenario was added: what the folds give on
/// the earlier logs did not change, and no earlier build wrote a reviewer's log.
/// Version 10: validation keeps each run's replay base.
const RECORDED_DIGEST: (u32, &str) = (
    10,
    "61b151ef6047535b97246073f78511c512925eaa9f7259c3c8ba5e1a35d00cbf",
);

fn golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engine/tests/fixtures/history")
        .join(cookie_agent_protocol::HISTORY_FORMAT.to_string())
}

fn logs_under(directory: &Path, logs: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(directory)
        .expect("a golden directory")
        .map(|entry| entry.expect("a directory entry").path())
        .collect::<Vec<_>>();
    entries.sort();
    for entry in entries {
        if entry.is_dir() {
            logs_under(&entry, logs);
        } else if entry.file_name().is_some_and(|name| name == "events.jsonl") {
            logs.push(entry);
        }
    }
}

/// Every log of the golden trees of this history format, in a stable order.
fn golden_logs() -> Vec<PathBuf> {
    let mut logs = Vec::new();
    logs_under(&golden_root(), &mut logs);
    assert!(!logs.is_empty(), "no golden logs");
    logs
}

/// The session of a golden log: the one its directory is named for, or, for an
/// approval reviewer's own log, the one its first record names.
fn session_of(path: &Path) -> SessionId {
    let directory = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    if directory == Some(crate::session::REVIEWER_DIR) {
        let first = fs::read_to_string(path).expect("a golden log");
        let first = first.lines().next().expect("a creation");
        return serde_json::from_str::<serde_json::Value>(first).expect("a record")["session_id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .expect("the creation names its session");
    }
    directory
        .and_then(|name| name.parse().ok())
        .expect("a log's directory is named for its session")
}

fn copy_directory(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("a directory");
    for entry in fs::read_dir(from).expect("a directory") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("a copy");
        }
    }
}

/// A copy of the tree `log`, a golden log, belongs to, and where its log is.
fn copy_of(log: &Path) -> (tempfile::TempDir, PathBuf) {
    let relative = log.strip_prefix(golden_root()).expect("a golden log");
    let mut parts = relative.components();
    let scenario = parts.next().expect("a scenario").as_os_str().to_owned();
    let root = parts.next().expect("a root").as_os_str().to_owned();
    let directory = tempdir().expect("a temporary directory");
    copy_directory(
        &golden_root().join(&scenario).join(&root),
        &directory.path().join(&root),
    );
    let copied = directory.path().join(&root).join(parts.as_path());
    (directory, copied)
}

/// Calls `visit` with a state file per committed turn of the log at `path`, as
/// the log's own folds give it there, and where the turn's line ends.
fn for_each_turn_boundary(path: &Path, mut visit: impl FnMut(StateFile, u64)) {
    let session_id = session_of(path);
    let blob_dir = blobs::tree_blob_dir(path);
    let mut fetch = |digest: &str| blobs::read(&blob_dir, digest);
    let mut cursor = LineCursor::open(path, 0).expect("a log");
    let mut storage = EventStorage::new(Vec::new());
    storage.lines = Some(LineIndex::default());
    let mut folding = Folding {
        path,
        session_id,
        storage,
        validation: ValidationState::default(),
        projection: None,
        fresh: true,
        reader: LineReader::new(path),
    };
    let mut index = 0;
    cookie_agent_protocol::read_stored_history(|| {
        loop {
            let start = cursor.position;
            if cursor.next().expect("a read") != Next::Line {
                break;
            }
            let length = cursor.line.len() as u64;
            let line = &cursor.line[..cursor.line.len() - 1];
            folding
                .read(index, start, line, length, &mut fetch)
                .expect("a golden log folds a record at a time");
            index += 1;
            let tip = folding.storage.all.last().expect("a record").clone();
            let boundary = matches!(
                tip.payload,
                EventPayload::ModelTurnCommitted { .. }
                    | EventPayload::ContextCheckpointCommitted { .. }
            );
            if tip.seq != folding.reader.last_observed_seq || !boundary {
                continue;
            }
            let mut folds = folding.projection.clone().expect("a golden log applies");
            folds.finish_read(tip.seq, &folding.reader.diagnostics);
            visit(
                StateFile {
                    version: STATE_VERSION,
                    history_format: cookie_agent_protocol::HISTORY_FORMAT,
                    session_id,
                    at: StateAt {
                        seq: tip.seq,
                        timestamp: tip.timestamp,
                        offset: start,
                    },
                    folds: LogFolds::of(
                        &folding.storage,
                        folding.validation.clone(),
                        ValidationTaint::default(),
                        folding.reader.diagnostics.clone(),
                        None,
                        tip.seq + 1,
                        folds,
                    )
                    .expect("a golden log's layout is known"),
                },
                cursor.position,
            );
        }
    });
}

fn states_at_turn_boundaries(path: &Path) -> Vec<(StateFile, u64)> {
    let mut states = Vec::new();
    for_each_turn_boundary(path, |state, end| states.push((state, end)));
    states
}

fn write_state(path: &Path, state: &StateFile) {
    fs::write(
        path.with_file_name(STATE_FILE),
        state.to_bytes().expect("state serializes"),
    )
    .expect("the state file");
}

/// Opens the log both ways, each comparing itself with a parse, and says whether
/// it went through the state file. An owned open cuts a torn final record, so a
/// read-only open goes first.
fn opened_from_state(path: &Path) -> bool {
    let session = session_of(path);
    let read_only = EventLog::open_read_only(path.to_owned(), session).expect("a read-only open");
    let from_state = read_only.opened_from_state_for_test();
    projection(Arc::clone(&read_only)).expect("a projection");
    let owned = EventLog::open(path.to_owned(), session).expect("an owned open");
    assert_eq!(owned.opened_from_state_for_test(), from_state, "both opens");
    projection(owned).expect("a projection");
    from_state
}

fn acp_log() -> PathBuf {
    golden_logs()
        .into_iter()
        .filter(|log| log.starts_with(golden_root().join("acp")))
        .max_by_key(|log| fs::metadata(log).expect("a log").len())
        .expect("the golden ACP scenario")
}

/// A snapshot and tail against a parse at every turn boundary of every golden log,
/// with the tail as far as the log goes, and with no tail at all.
#[test]
fn a_state_file_and_the_tail_open_as_a_parse_at_every_turn_boundary_of_the_golden_logs() {
    let mut boundaries = 0;
    for log in golden_logs() {
        let (_directory, path) = copy_of(&log);
        let whole = fs::read(&path).expect("a log");
        let states = states_at_turn_boundaries(&path);
        for (state, end) in &states {
            boundaries += 1;
            write_state(&path, state);
            assert!(
                opened_from_state(&path),
                "{}: the state file of record {}",
                log.display(),
                state.at.seq
            );
            fs::write(&path, &whole[..*end as usize]).expect("a prefix of the log");
            assert!(
                EventLog::open_read_only(path.clone(), session_of(&path))
                    .expect("a read-only open")
                    .opened_from_state_for_test(),
                "{}: the state file of record {}, its last",
                log.display(),
                state.at.seq
            );
            fs::write(&path, &whole).expect("the whole log");
        }
    }
    assert!(boundaries > 50, "only {boundaries} turn boundaries");
}

/// What the folds give that would change a state file's meaning without changing
/// its shape: see section 4.4.
fn digest_of_the_golden_logs() -> String {
    let mut digest = sha2::Sha256::new();
    for log in golden_logs() {
        let relative = log.strip_prefix(golden_root()).expect("a golden log");
        for part in relative.components() {
            digest.update(part.as_os_str().to_string_lossy().as_bytes());
            digest.update(b"/");
        }
        let (_directory, path) = copy_of(&log);
        for_each_turn_boundary(&path, |state, _| {
            digest.update(state.at.seq.to_string().as_bytes());
            digest.update(serde_json::to_vec(&state.folds).expect("folds serialize"));
        });
    }
    format!("{:x}", digest.finalize())
}

/// The alarm of section 4.4: a fold whose logic changes leaves the file's shape
/// alone and its contents wrong, on every session of every store, so this fails
/// until `STATE_VERSION` rises.
#[test]
fn the_folds_of_the_golden_logs_match_their_recorded_digest() {
    let digest = digest_of_the_golden_logs();
    assert_eq!(digest, digest_of_the_golden_logs(), "the digest is stable");
    let (version, recorded) = RECORDED_DIGEST;
    assert!(
        digest == recorded && version == STATE_VERSION,
        "What the folds give on the golden logs changed, or STATE_VERSION did: this \
         digest is {digest} under STATE_VERSION {STATE_VERSION}, and the recorded one is \
         {recorded} under {version}. A state.json an earlier build wrote would be read as \
         what this build's folds give, and be wrong on every later turn. Raise STATE_VERSION \
         in crates/engine_store/src/events/state_file.rs, then record the new digest with \
         that version in RECORDED_DIGEST in \
         crates/engine_store/src/events/state_file/tests.rs."
    );
}

/// `edit` applied to the JSON of the log's state file at its latest turn.
fn edited(path: &Path, state: &StateFile, edit: impl FnOnce(&mut Value)) {
    let mut value = serde_json::to_value(state).expect("state serializes");
    edit(&mut value);
    fs::write(
        path.with_file_name(STATE_FILE),
        serde_json::to_vec(&value).expect("state serializes"),
    )
    .expect("the state file");
}

/// A file that does not match the log folds the log from its start.
#[test]
fn a_state_file_that_does_not_match_its_log_folds_the_log_from_its_start() {
    let (_directory, path) = copy_of(&acp_log());
    let states = states_at_turn_boundaries(&path);
    let (latest, _) = states.last().expect("a turn");
    let state_path = path.with_file_name(STATE_FILE);

    assert!(!opened_from_state(&path), "a log with none");

    let bytes = latest.to_bytes().expect("state serializes");
    fs::write(&state_path, &bytes[..bytes.len() / 2]).expect("a truncated file");
    assert!(!opened_from_state(&path), "a truncated file");
    fs::write(&state_path, b"not json").expect("a damaged file");
    assert!(!opened_from_state(&path), "a damaged file");
    fs::write(&state_path, b"").expect("an empty file");
    assert!(!opened_from_state(&path), "an empty file");

    edited(&path, latest, |value| {
        value["version"] = (STATE_VERSION + 1).into();
    });
    assert!(!opened_from_state(&path), "of another STATE_VERSION");
    // An earlier build's, whose folds kept what this one's let go of.
    edited(&path, latest, |value| {
        value["version"] = (STATE_VERSION - 1).into();
    });
    assert!(!opened_from_state(&path), "of an earlier STATE_VERSION");
    edited(&path, latest, |value| {
        value["history_format"] = (cookie_agent_protocol::HISTORY_FORMAT + 1).into();
    });
    assert!(!opened_from_state(&path), "of another history format");
    edited(&path, latest, |value| {
        value["session_id"] = serde_json::to_value(SessionId::new_v7()).unwrap();
    });
    assert!(!opened_from_state(&path), "of another session");
    edited(&path, latest, |value| {
        value["at"]["seq"] = (latest.at.seq - 1).into();
    });
    assert!(!opened_from_state(&path), "naming another record");
    edited(&path, latest, |value| {
        value["at"]["offset"] = (latest.at.offset + 1).into();
    });
    assert!(!opened_from_state(&path), "naming another offset");
    edited(&path, latest, |value| {
        value["at"]["timestamp"] = serde_json::to_value(Timestamp::now()).unwrap();
    });
    assert!(!opened_from_state(&path), "the same offset, another record");
    edited(&path, latest, |value| {
        value["folds"]["window"] = (latest.at.seq + 1).into();
    });
    assert!(!opened_from_state(&path), "a window past the record");

    // That file is still of this log.
    write_state(&path, latest);
    assert!(opened_from_state(&path), "the file as it was written");
}

/// An older file that still matches its log is the file of a long run: what came
/// after it is the tail, whatever its length.
#[test]
fn a_state_file_behind_its_log_opens_with_a_longer_tail() {
    let (_directory, path) = copy_of(&acp_log());
    let states = states_at_turn_boundaries(&path);
    write_state(&path, &states[0].0);
    assert!(opened_from_state(&path));
}

/// A revert removes the file, and one a write in flight puts back, or one from
/// before it, no longer matches the log, whose records past the cut are others.
#[test]
fn a_state_file_from_before_a_revert_folds_the_log_from_its_start() {
    let (_directory, path) = copy_of(&acp_log());
    let session = session_of(&path);
    let states = states_at_turn_boundaries(&path);
    let (latest, _) = states.last().expect("a turn");
    let (earlier, _) = &states[states.len() / 2];
    write_state(&path, latest);
    let log = EventLog::open(path.clone(), session).expect("an owned open");
    assert!(log.opened_from_state_for_test());
    log.truncate_after_for_test(earlier.at.seq)
        .expect("a revert");
    assert!(
        !path.with_file_name(STATE_FILE).exists(),
        "a revert removes the file"
    );
    drop(log);
    assert!(!opened_from_state(&path), "after a revert, no file");

    write_state(&path, latest);
    assert!(!opened_from_state(&path), "the file from before it, cut");

    // The same sequences again, as the log goes on from the cut.
    let log = EventLog::open(path.clone(), session).expect("an owned open");
    let origin = log
        .creation()
        .and_then(|creation| creation.origin.clone())
        .expect("an origin");
    for index in 0..(latest.at.seq - earlier.at.seq + 2) {
        log.append(
            None,
            origin.clone(),
            EventPayload::Diagnostic {
                source: cookie_agent_protocol::DiagnosticSource::engine("test").expect("source"),
                kind: cookie_agent_protocol::DiagnosticKind::new("filler").expect("kind"),
                message: format!("{index} {}", "x".repeat(2_000)),
                count: None,
                detail: None,
            },
        )
        .expect("an append");
    }
    drop(log);
    assert!(
        !opened_from_state(&path),
        "the file from before it, regrown"
    );

    // A file of a record the revert kept is as good as ever.
    write_state(&path, earlier);
    assert!(opened_from_state(&path), "the file of what the revert kept");
}

/// A crash at each step of writing the file and of what it depends on in the log.
#[test]
fn a_crash_at_each_step_of_writing_folds_or_opens_as_a_parse() {
    let (_directory, path) = copy_of(&acp_log());
    let states = states_at_turn_boundaries(&path);
    let (latest, end) = states.last().expect("a turn");
    let (earlier, _) = &states[0];
    let whole = fs::read(&path).expect("a log");
    let temporary = path.with_file_name(format!(".{STATE_FILE}.crashed.tmp"));

    // Before the temporary is whole, and before it is renamed.
    let bytes = latest.to_bytes().expect("state serializes");
    fs::write(&temporary, &bytes[..bytes.len() / 3]).expect("a temporary");
    assert!(!opened_from_state(&path), "a partial temporary and no file");
    fs::write(&temporary, &bytes).expect("a temporary");
    assert!(!opened_from_state(&path), "a whole temporary and no file");
    write_state(&path, earlier);
    assert!(opened_from_state(&path), "a temporary over the file before");
    fs::remove_file(&temporary).expect("the temporary");

    // The rename survived, but the log's last records, not yet synced, did not.
    write_state(&path, latest);
    let line_start = latest.at.offset as usize;
    fs::write(&path, &whole[..line_start]).expect("a log without the record");
    assert!(!opened_from_state(&path), "the log ends before the record");
    fs::write(&path, &whole).expect("the whole log");

    // Part of the record the file names reached the disk.
    let torn = line_start + (*end as usize - line_start) / 2;
    fs::write(&path, &whole[..torn]).expect("a torn record");
    assert!(!opened_from_state(&path), "the record is torn");
    fs::write(&path, &whole).expect("the whole log");

    // Part of a later record did: step 2 reads the tail as the loader does.
    write_state(&path, earlier);
    let session = session_of(&path);
    let torn_tail = whole.len() - 3;
    fs::write(&path, &whole[..torn_tail]).expect("a torn final record");
    let read_only = EventLog::open_read_only(path.clone(), session).expect("a read-only open");
    assert!(
        read_only.opened_from_state_for_test(),
        "a torn final record"
    );
    assert_eq!(
        fs::metadata(&path).expect("a log").len(),
        torn_tail as u64,
        "a read-only open leaves it in the file"
    );
    let owned = EventLog::open(path.clone(), session).expect("an owned open");
    assert!(owned.opened_from_state_for_test(), "a torn final record");
    assert!(
        fs::read(&path).expect("a log").ends_with(b"\n"),
        "an owned open cuts it"
    );
    assert_eq!(
        owned.physical_tip_seq(),
        read_only.physical_tip_seq(),
        "where each goes on"
    );
}

/// A tail with a line the loader would repair or skip, or with damage, is the
/// loader's: the file is not used, whatever it says.
#[test]
fn a_tail_the_loader_would_repair_is_left_to_the_loader() {
    let (_directory, path) = copy_of(&acp_log());
    let states = states_at_turn_boundaries(&path);
    let (earlier, end) = &states[0];
    write_state(&path, earlier);
    let whole = fs::read(&path).expect("a log");

    let mut damaged = whole.clone();
    let at = whole[*end as usize..]
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("a line after the record")
        + *end as usize;
    // The first tail line stops being JSON, its length kept.
    damaged[at - 1] = b'#';
    fs::write(&path, &damaged).expect("a damaged tail");
    let session = session_of(&path);
    assert!(
        !EventLog::open_read_only(path.clone(), session)
            .expect("the loader reads past it")
            .opened_from_state_for_test()
    );

    let mut gap = whole.clone();
    gap.drain(*end as usize..=at);
    fs::write(&path, &gap).expect("a tail with a gap");
    assert!(
        !EventLog::open_read_only(path.clone(), session)
            .expect("the loader reads past it")
            .opened_from_state_for_test()
    );
}

/// An open folds no more than it has to: a read-only open of an ACP agent's log
/// holds its window, and writes no file.
#[test]
fn a_read_only_open_holds_the_window_and_writes_nothing() {
    let (_directory, path) = copy_of(&acp_log());
    let log = EventLog::open_read_only(path.clone(), session_of(&path)).expect("an open");
    assert!(log.window_for_test().is_some());
    assert!(
        (log.recent().len() as u64) < log.record_count() / 4,
        "held {} of {}",
        log.recent().len(),
        log.record_count()
    );
    assert_eq!(log.state_behind(), None);
    projection(log).expect("a projection");
    assert!(!path.with_file_name(STATE_FILE).exists());
}

/// A native log holds its model's context once a checkpoint commits, and its file
/// holds what that pins before the window; one with no checkpoint holds every
/// record and has no file, whatever its size (section 5.6).
#[test]
fn a_native_log_holds_its_context_once_a_checkpoint_commits_and_its_file_the_pins() {
    let golden = |scenario: &str| {
        golden_logs()
            .into_iter()
            .find(|log| log.starts_with(golden_root().join(scenario)))
            .expect("a golden scenario")
    };
    let (_directory, path) = copy_of(&golden("replay"));
    let session = session_of(&path);
    let owned = EventLog::open(path.clone(), session).expect("an open");
    assert_eq!(owned.window_for_test(), None);
    assert_eq!(owned.recent().len() as u64, owned.record_count());
    assert_eq!(owned.state_behind(), None);
    let folds = projection(Arc::clone(&owned)).expect("a projection");
    assert!(owned.state_file(&folds).is_none());

    let (_directory, path) = copy_of(&golden("coding"));
    let session = session_of(&path);
    let owned = EventLog::open(path.clone(), session).expect("an open");
    let window = owned.window_for_test().expect("a native window");
    let held = owned.storage().snapshot();
    let pinned = held.iter().filter(|event| event.seq < window).count();
    assert!(pinned > 0, "the checkpoint pins what precedes its context");
    assert!((held.len() as u64) < owned.record_count());
    assert_eq!(owned.recent().len(), held.len() - pinned);
    let folds = projection(Arc::clone(&owned)).expect("a projection");
    let state = owned
        .state_file(&folds)
        .expect("a native window has a file");
    assert_eq!(state.folds.pinned.len(), pinned);
    assert!(state.folds.context);
    write_state(&path, &state);
    drop(owned);
    assert!(opened_from_state(&path));
    let reopened = EventLog::open_read_only(path.clone(), session).expect("an open");
    assert!(
        reopened
            .storage()
            .snapshot()
            .iter()
            .map(|event| event.seq)
            .eq(held.iter().map(|event| event.seq)),
        "it opens holding what it held"
    );
}

/// The first golden log of `scenario`.
fn golden(scenario: &str) -> PathBuf {
    golden_logs()
        .into_iter()
        .find(|log| log.starts_with(golden_root().join(scenario)))
        .expect("a golden scenario")
}

/// A pass that ends before what a native window holds, its pinned records
/// included, gives nothing of what it holds, and a page of it is the page of
/// what precedes `before`: an `events.page` with a small `before` read
/// `events[pinned..0]` (review, 2026-11-08). A range that ends before it starts
/// gives nothing either, whatever the log holds.
#[test]
fn a_pass_ending_before_what_a_native_window_holds_gives_only_what_precedes_it() {
    use std::ops::Bound;

    let view = cookie_agent_protocol::EventsView::Display;
    let budget = 64 * 1024;
    let (_directory, path) = copy_of(&golden("coding"));
    let session = session_of(&path);
    let log = EventLog::open(path.clone(), session).expect("an open");
    let window = log.window_for_test().expect("a native window");
    let held = log.storage().snapshot();
    let pinned = held.iter().filter(|event| event.seq < window).count();
    assert!(pinned > 0, "the checkpoint pins what precedes its context");
    let whole = log
        .history()
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .expect("a pass");
    for before in 0..=held[0].seq + 1 {
        let expected = whole
            .iter()
            .filter(|event| event.seq < before)
            .cloned()
            .collect::<Vec<_>>();
        let read = log
            .history_range(..before)
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .expect("a pass");
        assert_eq!(read, expected, "the records before {before}");
        let page = log
            .history_range(..before)
            .and_then(|pass| pass.older_page(view, budget, true))
            .expect("a page");
        // A page ends after the last record it was given.
        let end = expected.last().map_or(0, |event| event.seq + 1);
        assert_eq!(
            page.window,
            cookie_agent_protocol::display_view::older_page(&expected, end, view, budget),
            "the page before {before}"
        );
    }

    let (_directory, path) = copy_of(&golden("replay"));
    let whole = EventLog::open(path.clone(), session_of(&path)).expect("an open");
    assert_eq!(whole.window_for_test(), None, "a log holding its records");
    for log in [&log, &whole] {
        let page = log
            .history_range((Bound::Included(5), Bound::Included(2)))
            .and_then(|pass| pass.older_page(view, budget, true))
            .expect("a page");
        assert!(page.window.events.is_empty(), "a reversed range");
    }
}

/// Damage before the offset is not read as the log opens: it shows when a pass
/// reads it (section 11, decision 1).
#[test]
fn damage_before_the_offset_shows_when_a_pass_reads_it() {
    // The file no longer parses as the state says, which is the point.
    crate::session::skip_reference_fold();
    let (_directory, path) = copy_of(&acp_log());
    let states = states_at_turn_boundaries(&path);
    let (latest, _) = states.last().expect("a turn");
    write_state(&path, latest);
    let mut bytes = fs::read(&path).expect("a log");
    let second_line = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("a line")
        + 1;
    let third_line = bytes[second_line..]
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("a line")
        + second_line;
    bytes[third_line - 1] = b'#';
    fs::write(&path, &bytes).expect("a damaged log");
    let session = session_of(&path);
    let log = EventLog::open_read_only(path.clone(), session).expect("it opens");
    assert!(log.opened_from_state_for_test());
    let tip = fs::read(&path)
        .expect("a log")
        .iter()
        .filter(|byte| **byte == b'\n')
        .count() as u64;
    let folds = projection(Arc::clone(&log)).expect("what it folded");
    assert_eq!(folds.meta.last_event_seq, tip);
    let read = log
        .history()
        .expect("a pass")
        .collect::<Result<Vec<_>, _>>();
    assert!(
        matches!(read, Err(EventLogError::Corrupt { .. })),
        "a pass finds the damage"
    );
}

/// A read-only log's owner can cut the file with no word to this process: a pass
/// reading past what changed ends as a pass an owned revert cut does.
#[test]
fn a_pass_of_a_read_only_log_finds_its_owner_cut_the_file() {
    crate::session::skip_reference_fold();
    let (_directory, path) = copy_of(&acp_log());
    let session = session_of(&path);
    let log = EventLog::open_read_only(path.clone(), session).expect("an open");
    let window = log.window_for_test().expect("a window");
    let whole = fs::read(&path).expect("a log");
    let offset = log.line_start(window).expect("the window's line") as usize;

    // The window's line is gone, and another took its place.
    let mut other = whole[..offset].to_vec();
    other.extend_from_slice(&whole[offset + 1..]);
    fs::write(&path, &other).expect("a file its owner cut");
    let read = log
        .history()
        .expect("a pass")
        .collect::<Result<Vec<_>, _>>();
    assert!(matches!(read, Err(EventLogError::HistoryCut { .. })));

    fs::write(&path, &whole).expect("the file as it was");
    assert!(
        log.history()
            .expect("a pass")
            .collect::<Result<Vec<_>, _>>()
            .is_ok()
    );
}

/// A fork of a log that opened from its file copies what a fork of a parse does:
/// its lines left in the file, values in blobs and all.
#[test]
fn a_fork_of_a_log_opened_from_its_state_copies_what_a_parse_does() {
    let mut named_blobs = false;
    for log in golden_logs() {
        let (_directory, path) = copy_of(&log);
        let session = session_of(&path);
        named_blobs |= blobs::has_references(&fs::read(&path).expect("a log"));
        let states = states_at_turn_boundaries(&path);
        let Some((latest, _)) = states.last() else {
            continue;
        };
        let (earlier, _) = &states[states.len() / 2];
        write_state(&path, latest);
        let opened = EventLog::open_read_only(path.clone(), session).expect("an open");
        assert!(opened.opened_from_state_for_test());
        let parsed = EventLog::parsed_read_only(path.clone(), session).expect("a parse");
        let target = tempdir().expect("a temporary directory");
        let fork_of = |log: &EventLog, name: &str| {
            let authority = WriteAuthority::new();
            let prefix = log
                .read_fork_prefix(session, earlier.at.seq)
                .expect("a prefix");
            let file = target.path().join(name);
            EventLog::fork_buffered(file.clone(), prefix, authority.capability())
                .write_whole(&file)
                .expect("a copy");
            fs::read(file).expect("a copy")
        };
        assert!(
            fork_of(&opened, "opened") == fork_of(&parsed, "parsed"),
            "{}",
            log.display()
        );
    }
    assert!(named_blobs, "no golden log names a blob");
}

/// A line naming blobs stays in the file, and what a fork copies of it is the line,
/// with what this build does not know.
#[test]
fn a_line_naming_blobs_is_read_from_the_file_with_what_this_build_does_not_know() {
    let log = golden_logs()
        .into_iter()
        .find(|log| blobs::has_references(&fs::read(log).expect("a log")))
        .expect("a golden log naming blobs");
    let (_directory, path) = copy_of(&log);
    let session = session_of(&path);
    let lines = fs::read(&path).expect("a log");
    let mut changed = Vec::new();
    let mut named = None;
    for (index, line) in lines.split_inclusive(|byte| *byte == b'\n').enumerate() {
        if named.is_none() && blobs::has_references(line) {
            let mut value = serde_json::from_slice::<Value>(line).expect("a line");
            value["from_a_newer_build"] = Value::Bool(true);
            serde_json::to_writer(&mut changed, &value).expect("a line");
            changed.push(b'\n');
            named = Some((index, value["seq"].as_u64().expect("a sequence")));
        } else {
            changed.extend_from_slice(line);
        }
    }
    fs::write(&path, &changed).expect("a log");
    let (_, seq) = named.expect("a line naming blobs");
    let opened = EventLog::open(path.clone(), session).expect("an open");
    assert!(opened.kept_raw_seqs_for_test().is_empty(), "held in memory");
    let mut found = false;
    opened
        .visit_json(seq..=seq, |value| {
            found = value.get("from_a_newer_build").is_some();
        })
        .expect("the line");
    assert!(found, "what this build did not read survives");
}
