//! Golden session logs (`docs/site/specs/compatibility-v1.md` section 8.8).
//!
//! `crates/engine/tests/fixtures/history/<format>/<scenario>/` holds session
//! trees a scripted run against fake model providers recorded, one directory
//! per root session as the session store lays it out (its artifacts, child
//! sessions, and caches included), and `snapshot.json`: every session's
//! metadata, usage, events as clients receive them (recorded only where that
//! differs from the stored record), and model history, full and assembled,
//! as the recording build read them back.
//!
//! [`golden_history_reads_as_recorded`] opens a copy of every tree read-only
//! (another owner holds its lock) and owned (adopted for writing), and
//! compares what it reads with the snapshot. The snapshot is the older
//! build's view: every member it records must read the same, while members a
//! newer build adds are allowed. The read-only open must leave the copy's
//! bytes as they were.
//!
//! These directories are frozen. No `COOKIE_UPDATE_*` switch applies to
//! them, and nothing regenerates them: a failure means the build no longer
//! reads what an earlier build wrote, and the fix belongs in the code. The
//! recorder, [`record_golden_history`], runs only when asked:
//!
//! ```sh
//! COOKIE_GOLDEN_HISTORY_OUT=/tmp/golden cargo test -p cookie_agent_engine --lib \
//!     record_golden_history -- --ignored
//! ```
//!
//! It writes `<out>/<HISTORY_FORMAT>/` and refuses an `<out>` that exists.
//! A build that bumps the history format records with it once and commits
//! the result as `history/<new format>/` beside the older formats, which stay
//! as they are and are then read through their converters
//! (`crate::events::convert`).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use cookie_agent_protocol::{
    ApprovalRespondParams, ApprovalStatus, ApprovalUserDecision, ClientResponseId, ClientRunId,
    EventOrigin, EventPayload, HISTORY_FORMAT, InputDelivery, Open, RunSelection, RunStartParams,
    SessionId, SessionStatus, SessionSubmitInputParams, UserContent, UserContentPart,
};
use serde_json::{Value, json};

use super::{
    acp::{
        acp_fixture_with, dial_in, final_text, finished, fixture_script, selection, start, turn,
    },
    support::*,
};
use crate::{Engine, EngineHistoryView};

const GOLDEN_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/history");
const OUT_VARIABLE: &str = "COOKIE_GOLDEN_HISTORY_OUT";
const SNAPSHOT_FILE: &str = "snapshot.json";

// --- reading -----------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Opening {
    /// Another owner holds every tree's lock, so the engine only reads.
    ReadOnly,
    /// The engine adopts every tree for writing.
    Owned,
}

/// An engine over a private copy of a scenario's trees.
struct OpenedScenario {
    engine: Engine,
    roots: Vec<SessionId>,
    /// The store's `sessions/` folder.
    sessions: PathBuf,
    _locks: Vec<crate::ownership::SessionOwnership>,
    _directory: PanicResistantTempDir,
}

fn scenario_roots(directory: &Path) -> Vec<SessionId> {
    let mut roots = std::fs::read_dir(directory)
        .expect("scenario directory")
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<SessionId>().ok())
        .collect::<Vec<_>>();
    roots.sort();
    roots
}

async fn open_scenario(scenario: &Path, opening: Opening) -> OpenedScenario {
    let directory = private_tempdir();
    let cwd = directory.path().join("workspace");
    create_private_test_dir(&cwd);
    let data = directory.path().join("data");
    let sessions = data.join(crate::session::SESSIONS_ROOT_DIR);
    let roots = scenario_roots(scenario);
    let mut locks = Vec::new();
    for root in &roots {
        let target = crate::session::root_dir_in(&sessions, *root);
        std::fs::create_dir_all(target.parent().expect("month folder")).expect("month folder");
        copy_test_tree(&scenario.join(root.to_string()), &target);
        if matches!(opening, Opening::ReadOnly) {
            locks.push(crate::ownership::try_acquire(&target).expect("hold the tree's lock"));
        }
    }
    let engine = crate::Engine::open(crate::EngineOptions {
        data_dir: data,
        cwd: cwd.clone(),
        config: reading_configuration(),
        model_manager: reading_model_manager(directory.path()),
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(&cwd),
    })
    .expect("engine over the golden trees");
    if matches!(opening, Opening::Owned) {
        for root in &roots {
            engine.resume(*root).await.expect("adopt a golden tree");
        }
    }
    OpenedScenario {
        engine,
        roots,
        sessions,
        _locks: locks,
        _directory: directory,
    }
}

fn reading_configuration() -> cookie_agent_config::LoadedConfiguration {
    empty_configuration()
}

fn reading_model_manager(directory: &Path) -> Arc<cookie_agent_models::ModelManager> {
    let store = directory.join("provider-store");
    create_private_test_dir(&store);
    Arc::new(
        cookie_agent_models::ModelManager::new(
            BTreeMap::new(),
            empty_catalog(),
            cookie_agent_models::provider_store::ProviderStore::open(store)
                .expect("provider store"),
        )
        .expect("model manager"),
    )
}

/// Every session of the trees rooted at `roots`, as a reader sees it.
async fn snapshot(engine: &Engine, roots: &[SessionId]) -> Value {
    let mut trees = Vec::new();
    for root in roots {
        let tree = engine.tree(*root).expect("session tree");
        let mut sessions = Vec::new();
        let mut pending = vec![&tree];
        while let Some(node) = pending.pop() {
            sessions.push(session_snapshot(engine, node.session.session_id).await);
            pending.extend(node.children.iter().rev());
        }
        trees.push(json!({
            "root": root,
            "tree": tree,
            "sessions": sessions,
        }));
    }
    json!({ "history_format": HISTORY_FORMAT, "trees": trees })
}

async fn session_snapshot(engine: &Engine, id: SessionId) -> Value {
    let meta = engine.get_session(id).expect("session metadata");
    let usage = engine.session_usage(id).expect("session usage");
    // As the daemon sends them to a client of this build's version.
    let audience =
        cookie_agent_protocol::DisplayAudience::new(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION);
    let events = engine
        .inner
        .store
        .log(id)
        .expect("session log")
        .event_snapshot()
        .iter()
        .map(|event| serde_json::to_value(&*audience.stored(event)).expect("event serializes"))
        .collect::<Vec<_>>();
    let mut history = serde_json::Map::new();
    for (name, view) in [
        ("full", EngineHistoryView::Full),
        ("assembled", EngineHistoryView::Assembled),
    ] {
        history.insert(
            name.into(),
            match engine.get_history(id, view).await {
                Ok(turns) => serde_json::to_value(turns).expect("history serializes"),
                Err(error) => json!({ "error": error.to_string() }),
            },
        );
    }
    json!({
        "session_id": id,
        "meta": meta,
        "usage": usage,
        "events": events,
        "history": history,
    })
}

/// Where `actual` differs from `expected`: every member `expected` has must
/// be in `actual` and equal, recursively; `actual` may have more members.
/// Arrays must have the same length.
fn differences(expected: &Value, actual: &Value, path: &str, found: &mut Vec<String>) {
    if found.len() >= 20 {
        return;
    }
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            for (key, value) in expected {
                match actual.get(key) {
                    Some(actual) => differences(value, actual, &format!("{path}.{key}"), found),
                    None => found.push(format!("{path}.{key}: missing")),
                }
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            if expected.len() != actual.len() {
                found.push(format!(
                    "{path}: {} items recorded, {} read",
                    expected.len(),
                    actual.len()
                ));
                return;
            }
            for (index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
                differences(expected, actual, &format!("{path}[{index}]"), found);
            }
        }
        (expected, actual) if expected == actual => {}
        (expected, actual) => {
            let shorten = |value: &Value| {
                let text = value.to_string();
                if text.chars().count() > 200 {
                    format!("{}…", text.chars().take(200).collect::<String>())
                } else {
                    text
                }
            };
            found.push(format!(
                "{path}: recorded {}, read {}",
                shorten(expected),
                shorten(actual)
            ));
        }
    }
}

/// The first history format whose golden snapshots were recorded with the
/// file labels of [`without_file_labels`].
const FILE_LABELS_FORMAT: u32 = 2;

/// Removes from `read`'s history views the file labels a build adds before
/// each user file (`docs/site/specs/machines-on-demand.md` section 6.4):
/// model text the engine injects as it reads, which snapshots recorded
/// before it (those of format 1) cannot hold. Everything else of the history
/// must still read as recorded.
fn without_file_labels(read: &mut Value) {
    for_each_session(read, |session| {
        for view in ["full", "assembled"] {
            let Some(turns) = session["history"][view].as_array_mut() else {
                continue;
            };
            for turn in turns {
                if turn["type"] != "user" {
                    continue;
                }
                if let Some(content) = turn["value"]["content"].as_array_mut() {
                    content.retain(|part| {
                        !(part["type"] == "text"
                            && part["value"]["text"].as_str().is_some_and(|text| {
                                text.starts_with("[file") && text.contains(", artifact ")
                            }))
                    });
                }
            }
        }
    });
}

fn for_each_session(snapshot: &mut Value, mut visit: impl FnMut(&mut Value)) {
    for tree in snapshot["trees"].as_array_mut().expect("trees") {
        for session in tree["sessions"].as_array_mut().expect("sessions") {
            visit(session);
        }
    }
}

/// The records of session `id`'s log in `scenario`, as JSON.
fn log_lines(scenario: &Path, id: &str) -> Vec<Value> {
    fn find(directory: &Path, id: &str) -> Option<PathBuf> {
        for entry in std::fs::read_dir(directory).ok()?.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if entry.file_name() == id {
                return Some(path.join(crate::session::EVENTS_FILE));
            }
            if let Some(found) = find(&path, id) {
                return Some(found);
            }
        }
        None
    }
    let path = find(scenario, id).unwrap_or_else(|| panic!("no log of {id} in the scenario"));
    std::fs::read_to_string(path)
        .expect("golden log")
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// A snapshot records a session's events as clients receive them only where
/// that differs from the stored record (a display the log stores as ranges
/// of the output); every other event is the record itself, as the log holds
/// it. This expands the events to the full list.
fn expand_recorded_events(scenario: &Path, recorded: &mut Value) {
    for_each_session(recorded, |session| {
        let id = session["session_id"]
            .as_str()
            .expect("session ID")
            .to_owned();
        let unlike = session["events"]["unlike_log"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let by_seq = |event: &Value| event["seq"].as_u64();
        let mut events = log_lines(scenario, &id)
            .into_iter()
            .map(|line| {
                unlike
                    .iter()
                    .find(|event| by_seq(event) == by_seq(&line))
                    .cloned()
                    .unwrap_or(line)
            })
            .collect::<Vec<_>>();
        let count = session["events"]["count"].as_u64().unwrap_or_default();
        events.truncate(usize::try_from(count).unwrap_or(usize::MAX));
        session["events"] = Value::Array(events);
    });
}

/// What the converters change in what a snapshot of an older format shows:
/// `session_created` names the format it was converted to. Format 2 moves
/// large values to blobs, which every reader reads back, so nothing else
/// changes.
fn as_converted(recorded: &mut Value) {
    for_each_session(recorded, |session| {
        for event in session["events"].as_array_mut().into_iter().flatten() {
            if event["payload"]["type"] == "session_created" {
                event["payload"]["history_format"] = json!(HISTORY_FORMAT);
            }
        }
    });
}

fn tree_bytes(directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(directory)
            .expect("tree directory")
            .flatten()
        {
            let path = entry.path();
            if crate::ownership::is_owner_lock_path(&path) {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).expect("inside the tree").to_owned(),
                    std::fs::read(&path).expect("tree file"),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(directory, directory, &mut files);
    files
}

fn golden_scenarios() -> Vec<(u32, PathBuf)> {
    let mut scenarios = Vec::new();
    let Ok(formats) = std::fs::read_dir(GOLDEN_ROOT) else {
        return scenarios;
    };
    for format in formats.flatten() {
        let Some(number) = format
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        for scenario in std::fs::read_dir(format.path())
            .expect("format directory")
            .flatten()
        {
            if scenario.path().join(SNAPSHOT_FILE).is_file() {
                scenarios.push((number, scenario.path()));
            }
        }
    }
    scenarios.sort();
    scenarios
}

#[tokio::test]
async fn golden_history_reads_as_recorded() {
    let scenarios = golden_scenarios();
    assert!(
        scenarios
            .iter()
            .any(|(format, _)| *format == HISTORY_FORMAT),
        "no golden trees of history format {HISTORY_FORMAT} in {GOLDEN_ROOT}"
    );
    let mut failures = Vec::new();
    for (format, scenario) in scenarios {
        assert!(
            format <= HISTORY_FORMAT,
            "{} is of a format newer than this build",
            scenario.display()
        );
        let mut recorded: Value = serde_json::from_slice(
            &std::fs::read(scenario.join(SNAPSHOT_FILE)).expect("recorded snapshot"),
        )
        .expect("recorded snapshot JSON");
        expand_recorded_events(&scenario, &mut recorded);
        if format < HISTORY_FORMAT {
            as_converted(&mut recorded);
        }
        let name = scenario.strip_prefix(GOLDEN_ROOT).unwrap_or(&scenario);
        for opening in [Opening::ReadOnly, Opening::Owned] {
            let opened = open_scenario(&scenario, opening).await;
            let mut read = snapshot(&opened.engine, &opened.roots).await;
            if format < FILE_LABELS_FORMAT {
                without_file_labels(&mut read);
            }
            let mut found = Vec::new();
            differences(&recorded["trees"], &read["trees"], "trees", &mut found);
            if matches!(opening, Opening::ReadOnly) {
                for root in &opened.roots {
                    let recorded = tree_bytes(&scenario.join(root.to_string()));
                    if tree_bytes(&crate::session::root_dir_in(&opened.sessions, *root)) != recorded
                    {
                        found.push(format!("the read-only open changed tree {root}"));
                    }
                }
            }
            opened.engine.shutdown().await;
            failures.extend(
                found
                    .into_iter()
                    .map(|found| format!("{} ({opening:?}): {found}", name.display())),
            );
        }
    }
    assert!(
        failures.is_empty(),
        "golden session logs no longer read as recorded. They are frozen: fix the reader, \
         never the logs or their snapshots.\n{}",
        failures.join("\n")
    );
}

// --- the folds ----------------------------------------------------------------

/// The logs of a scenario's trees, as a reader opens them.
fn scenario_logs(scenario: &Path) -> Vec<Arc<crate::events::EventLog>> {
    walkdir(scenario)
        .into_iter()
        .filter(|path| path.file_name().is_some_and(|name| name == "events.jsonl"))
        .map(|path| {
            let directory = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str());
            // An approval reviewer's own log names its session in its first record.
            let session = if directory == Some(crate::session::REVIEWER_DIR) {
                let log = std::fs::read_to_string(&path).expect("a golden log");
                serde_json::from_str::<Value>(log.lines().next().expect("a creation"))
                    .expect("a record")["session_id"]
                    .as_str()
                    .and_then(|id| id.parse::<SessionId>().ok())
                    .expect("the creation names its session")
            } else {
                directory
                    .and_then(|name| name.parse::<SessionId>().ok())
                    .expect("a log's directory names its session")
            };
            crate::events::EventLog::open_read_only(path, session).expect("a golden log")
        })
        .collect()
}

/// At every review of every golden log, the approval reviewer's own log sends
/// what today's render of the whole log gives, with bounds no log reaches
/// and with bounds small enough to start an epoch at nearly every review
/// (`docs/site/specs/forever-sessions.md` section 5.5).
#[test]
fn golden_logs_send_from_the_reviewer_log_what_the_render_gives() {
    use crate::runtime::approval::context::{Bounds, render, tests::log_sends};
    let mut reviews = 0;
    let mut verdicts = 0;
    for (_, scenario) in golden_scenarios() {
        for log in scenario_logs(&scenario) {
            let events = log
                .history()
                .expect("a pass")
                .collect::<Result<Vec<_>, _>>()
                .expect("a golden log");
            verdicts += log_sends::verdicts(&events).len();
            for event in &events {
                let EventPayload::ApprovalRequested { request } = &event.payload else {
                    continue;
                };
                reviews += 1;
                for bounds in [
                    Bounds::new(None, None, "Evaluate approval requests."),
                    Bounds {
                        window: 1_500,
                        system_tokens: 20,
                    },
                ] {
                    assert_eq!(
                        log_sends::sent(&events, None, request.approval_id(), bounds),
                        render(&events, None, request.approval_id(), bounds),
                        "{}: review {}",
                        log.path().display(),
                        event.seq
                    );
                }
            }
        }
    }
    assert!(reviews > 0, "no golden log asks for a review");
    assert!(verdicts > 0, "no golden log records a verdict");
}

/// The approval reviewer's own logs the golden `reviewer` scenario recorded
/// hold, at every turn, what today's render of the session's whole log gives
/// for that review with the same bounds: the turns since the epoch's first as
/// its earlier turns, and the turn as its request. That holds for a root, for
/// the fork, whose log was copied from its source's latest epoch, after a
/// revert that cut the root's, and for a delegated child, whose opening its
/// root's log gave once (`docs/site/specs/forever-sessions.md` section 5.5).
#[test]
fn the_golden_reviewer_logs_hold_what_the_render_gives() {
    use crate::runtime::approval::context::{Bounds, Inherited, RootOpening, render};
    let scenario = Path::new(GOLDEN_ROOT)
        .join(HISTORY_FORMAT.to_string())
        .join("reviewer");
    let logs = scenario_logs(&scenario);
    let read = |log: &crate::events::EventLog| {
        log.history()
            .expect("a pass")
            .collect::<Result<Vec<_>, _>>()
            .expect("a golden log")
    };
    let mut turns_checked = 0;
    let mut epochs = 0;
    for reviewer in logs
        .iter()
        .filter(|log| log.path().parent().and_then(Path::file_name) == Some("reviewer".as_ref()))
    {
        let session_dir = reviewer.path().parent().and_then(Path::parent).unwrap();
        let session = logs
            .iter()
            .find(|log| log.path() == session_dir.join("events.jsonl"))
            .expect("the reviewed session's log");
        let events = read(session);
        // The reviewer's bounds, from the approval agent the runs froze.
        let approval = events
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::RunStarted {
                    internal_agents, ..
                } => internal_agents
                    .iter()
                    .find(|agent| agent.agent.as_str() == "approval")
                    .cloned(),
                _ => None,
            })
            .expect("the approval agent");
        let system = cookie_agent_config::render_internal_prompt(
            &approval.composed_prompt,
            cookie_agent_config::InternalPromptValues {
                default_prompt: "",
                focus: None,
            },
        );
        let bounds = Bounds::new(Some(7_000), Some(1_024), &system);
        // A delegated child's opening, from its root's log up to its creation.
        let inherited = (session_dir.parent().and_then(Path::file_name)
            == Some(crate::session::SUBAGENTS_DIR.as_ref()))
        .then(|| {
            let root_dir = session_dir.parent().and_then(Path::parent).unwrap();
            let root = logs
                .iter()
                .find(|log| log.path() == root_dir.join("events.jsonl"))
                .expect("the root's log");
            let mut opening = RootOpening::new(session.creation().expect("a creation").timestamp);
            for event in read(root) {
                opening.observe(event);
            }
            let (entries, agents_md) = opening.finish();
            Inherited { entries, agents_md }
        });
        let mut thread = Vec::new();
        for event in read(reviewer) {
            let EventPayload::ReviewerTurnRecorded { turn } = &event.payload else {
                continue;
            };
            if turn.epoch.is_some() {
                epochs += 1;
                thread.clear();
            }
            let rendered = render(&events, inherited.as_ref(), turn.approval_id, bounds)
                .expect("the review's request");
            assert_eq!(
                rendered.reviews,
                thread,
                "{}: the turns before {}",
                reviewer.path().display(),
                turn.cut_seq
            );
            assert_eq!(
                rendered.request,
                turn.request,
                "{}: the turn at {}",
                reviewer.path().display(),
                turn.cut_seq
            );
            thread.push((turn.request.clone(), turn.verdict.clone()));
            turns_checked += 1;
        }
    }
    assert!(turns_checked >= 10, "{turns_checked} turns");
    assert!(epochs >= 3, "{epochs} epochs");
}

/// Every fold of `EventLog::state` reads what the scan it replaced read, at
/// every event of every golden log
/// (`docs/site/specs/forever-sessions.md` section 7).
#[test]
fn golden_logs_fold_as_their_scans_read_them() {
    let mut logs = 0;
    for (_, scenario) in golden_scenarios() {
        for log in scenario_logs(&scenario) {
            logs += 1;
            let found =
                crate::events::state::reference::differences_at_every_event(&log.event_snapshot());
            assert!(
                found.is_empty(),
                "{}: folds differ from their scans (fold, first record): {found:?}",
                log.path().display()
            );
        }
    }
    assert!(logs > 0, "no golden logs in {GOLDEN_ROOT}");
}

/// The records of a golden log appended as the store appends them, its window
/// moved by each turn an ACP agent committed and each checkpoint a native model
/// did: at each, the window answers as a reopening of the file does, and an owned
/// reopening holds the same window. A native log with no checkpoint has none
/// (`docs/site/specs/forever-sessions.md` sections 5 and 7).
///
/// One test per scenario, so they run side by side.
fn golden_log_appended_again_answers_as_reopened(scenario: &str) -> (usize, usize) {
    let mut windows = 0;
    let mut native_windows = 0;
    let scenarios = Path::new(GOLDEN_ROOT).join(HISTORY_FORMAT.to_string());
    {
        for golden in scenario_logs(&scenarios.join(scenario)) {
            let records = golden.event_snapshot();
            let directory = private_tempdir();
            let tree = directory.path().join(records[0].session_id.to_string());
            create_private_test_dir(&tree);
            let log = crate::events::EventLog::create(
                tree.join("events.jsonl"),
                records[0].session_id,
                records[0].origin.clone().expect("an origin"),
                records[0].payload.clone(),
            )
            .expect("the log");
            let mut checkpointed = false;
            for record in &records[1..] {
                // A run's records rely on no record a fork left out.
                let Ok(appended) = log.append(
                    record.run_id,
                    record.origin.clone().expect("an origin"),
                    record.payload.clone(),
                ) else {
                    break;
                };
                let context = format!("{scenario} at {}", appended.seq);
                match &record.payload {
                    EventPayload::ModelTurnCommitted { resolved_model, .. }
                        if resolved_model.provider_id.as_str()
                            == cookie_agent_protocol::ACP_MODEL_PROVIDER =>
                    {
                        windows += 1;
                        assert_eq!(log.window_for_test(), Some(appended.seq), "{context}");
                    }
                    EventPayload::ModelTurnCommitted { .. } => {
                        assert_eq!(log.window_for_test().is_some(), checkpointed, "{context}");
                    }
                    EventPayload::ContextCheckpointCommitted { .. } => {
                        checkpointed = true;
                        native_windows += 1;
                        assert!(log.window_for_test().is_some(), "{context}");
                    }
                    _ => continue,
                }
                super::acp::window::assert_answers_as_reopened(&log, &context);
                let copy = private_tempdir();
                let copied = copy.path().join(records[0].session_id.to_string());
                create_private_test_dir(&copied);
                std::fs::copy(log.path(), copied.join("events.jsonl")).expect("a copy");
                // With the blobs its large values moved to.
                let blobs = crate::events::blobs::tree_blob_dir(log.path());
                if blobs.is_dir() {
                    let target = crate::events::blobs::tree_blob_dir(&copied.join("events.jsonl"));
                    std::fs::create_dir_all(&target).expect("a blob directory");
                    for blob in std::fs::read_dir(&blobs).expect("blobs").flatten() {
                        std::fs::copy(blob.path(), target.join(blob.file_name())).expect("a blob");
                    }
                }
                let owned = crate::events::EventLog::open(
                    copied.join("events.jsonl"),
                    records[0].session_id,
                )
                .expect("reopens owned");
                crate::session::projection(Arc::clone(&owned)).expect("folds");
                assert_eq!(owned.window_for_test(), log.window_for_test(), "{context}");
                let seqs = |log: &crate::events::EventLog| {
                    log.recent()
                        .iter()
                        .map(|event| event.seq)
                        .collect::<Vec<_>>()
                };
                assert_eq!(seqs(&owned), seqs(&log), "{context}: what it holds");
                super::acp::window::assert_answers_as_reopened(&owned, &context);
            }
        }
    }
    (windows, native_windows)
}

#[test]
fn a_golden_acp_log_appended_again_answers_as_reopened_at_each_committed_turn() {
    let (windows, _) = golden_log_appended_again_answers_as_reopened("acp");
    assert!(windows >= 5, "{windows} committed turns of an ACP agent");
}

#[test]
fn a_golden_coding_log_appended_again_answers_as_reopened_at_each_turn_and_checkpoint() {
    let (_, checkpoints) = golden_log_appended_again_answers_as_reopened("coding");
    assert_eq!(checkpoints, 1);
}

#[test]
fn a_golden_native_compaction_log_appended_again_answers_as_reopened() {
    for scenario in ["native-compaction", "native-compaction-fallback"] {
        let (_, checkpoints) = golden_log_appended_again_answers_as_reopened(scenario);
        assert_eq!(checkpoints, 1, "{scenario}");
    }
}

#[test]
fn a_golden_replay_and_revert_log_appended_again_answers_as_reopened() {
    for scenario in ["replay", "revert-fork"] {
        golden_log_appended_again_answers_as_reopened(scenario);
    }
}

#[test]
fn a_golden_delegation_log_appended_again_answers_as_reopened() {
    golden_log_appended_again_answers_as_reopened("delegation");
}

/// A window, a page, a count, and an event read as a pass over a golden log are
/// what the whole list gives, at each run of each log: an open, an older page,
/// and `event.read` no longer take the whole list
/// (`docs/site/specs/forever-sessions.md` section 3.3).
#[test]
fn golden_logs_read_as_a_pass_give_what_the_whole_list_gives() {
    use cookie_agent_protocol::{EventsView, display_view};
    for (_, scenario) in golden_scenarios() {
        for log in scenario_logs(&scenario) {
            let events = log.event_snapshot();
            let path = log.path().display();
            for view in [EventsView::Display, EventsView::Full] {
                for budget in [2 * 1024, 64 * 1024] {
                    let mut window = display_view::TailWindowReader::new(view, budget);
                    let mut page = display_view::OlderPageReader::new(view, budget);
                    for (index, event) in log.history().unwrap().enumerate() {
                        let event = event.unwrap();
                        window.observe(Arc::clone(&event));
                        page.observe(Arc::clone(&event));
                        if !matches!(event.payload, EventPayload::RunFinished { .. }) {
                            continue;
                        }
                        let through = &events[..=index];
                        assert_eq!(
                            window.clone().window(),
                            display_view::tail_window(through, view, budget),
                            "{path}: the window through {}",
                            event.seq
                        );
                        assert_eq!(
                            page.clone().finish(),
                            display_view::older_page(through, event.seq + 1, view, budget),
                            "{path}: the page before {}",
                            event.seq + 1
                        );
                        assert_eq!(
                            page.header_at_end(),
                            display_view::state_events(through),
                            "{path}: the header of what follows {}",
                            event.seq
                        );
                    }
                }
            }
            let mut count = display_view::DisplayCount::default();
            for event in log.history().unwrap() {
                count.observe(&event.unwrap());
            }
            assert_eq!(
                count.shown(),
                display_view::display_events(&events, false).0.len() as u64,
                "{path}: the count"
            );
            assert_eq!(
                log.record_count(),
                events.len() as u64,
                "{path}: the records"
            );
            for event in events.iter() {
                let read = log
                    .history_range(event.seq..=event.seq)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap();
                assert!(read == *event, "{path}: event {}", event.seq);
            }
        }
    }
}

/// What a request assembles from a golden log's model context is what it
/// assembles from the log through the same record, at every turn boundary,
/// on each model its runs bound and on another; and a log that let go of the
/// rest each time would hold the same (`docs/site/specs/forever-sessions.md`
/// section 5.3). These logs' checkpoints were written before `context_fresh`,
/// so they pin as they did.
#[tokio::test]
async fn golden_logs_assemble_from_their_model_context_as_from_the_whole_log() {
    let mut boundaries = 0;
    for (_, scenario) in golden_scenarios() {
        let opened = open_scenario(&scenario, Opening::ReadOnly).await;
        for log in scenario_logs(&scenario) {
            let events = log.event_snapshot();
            let mut state = crate::events::state::LogState::default();
            let mut held = Vec::<Arc<cookie_agent_protocol::StoredEvent>>::new();
            let mut bindings = vec![crate::test_support::model_binding()];
            for (index, event) in events.iter().enumerate() {
                state.observe(event);
                if let EventPayload::RunStarted { .. } = &event.payload
                    && let Some(run) = event.run_id
                    && let Ok(policy) = opened.engine.historical_title_policy(&log, run)
                {
                    bindings.extend(policy.selected_suffix.first().cloned());
                }
                let boundary = index + 1 == events.len()
                    || matches!(
                        event.payload,
                        EventPayload::ModelTurnCommitted { .. }
                            | EventPayload::ContextCheckpointCommitted { .. }
                            | EventPayload::RunFinished { .. }
                    );
                if !boundary {
                    continue;
                }
                boundaries += 1;
                let through = &events[..=index];
                let selected = crate::events::model_context::model_context(through, &state)
                    .unwrap_or_else(|| through.to_vec());
                let tip = held.last().map_or(0, |event| event.seq);
                held.extend(through.iter().filter(|event| event.seq > tip).cloned());
                held = crate::events::model_context::model_context(&held, &state).unwrap_or(held);
                assert!(
                    held.iter()
                        .map(|event| event.seq)
                        .eq(selected.iter().map(|event| event.seq)),
                    "{}: what a native window would hold through {}",
                    log.path().display(),
                    event.seq
                );
                for binding in &bindings {
                    crate::model_history::assert_assembles_as_whole(
                        &selected,
                        through,
                        &opened.engine.inner.artifacts,
                        binding,
                        "System prompt.",
                    );
                }
            }
        }
        opened.engine.shutdown().await;
    }
    assert!(boundaries > 0);
}

/// The ACP scenario holds the records every fold reads, so a fold that went
/// wrong has something to differ on.
#[test]
fn the_golden_acp_scenario_reaches_the_folds() {
    let scenario = Path::new(GOLDEN_ROOT)
        .join(HISTORY_FORMAT.to_string())
        .join("acp");
    let logs = scenario_logs(&scenario);
    let mut seen = std::collections::BTreeSet::new();
    let mut agents = std::collections::BTreeSet::new();
    let mut reasons = Vec::new();
    let mut starts = Vec::new();
    let mut in_system_prompt = Vec::new();
    let mut owners = std::collections::HashSet::new();
    let mut update_parts = std::collections::BTreeSet::new();
    let mut decided = false;
    for log in &logs {
        for event in log.event_snapshot().iter() {
            seen.insert(cookie_agent_protocol::OpenTagged::tag(&event.payload).to_owned());
            match &event.payload {
                EventPayload::AcpSessionStarted { record } => {
                    agents.insert(record.agent.to_string());
                    starts.push(record.how);
                }
                EventPayload::AcpSessionUpdated { update } => {
                    if let Some(marker) = &update.detached {
                        reasons.push(marker.reason);
                    }
                    in_system_prompt.extend(update.prompt_context_system_prompt);
                    for (name, present) in [
                        ("options", update.options.is_some()),
                        ("commands", update.commands.is_some()),
                        ("starting_options", update.starting_options.is_some()),
                        ("context", update.context.is_some()),
                        ("prompt_context", update.prompt_context.is_some()),
                        ("fork_point", update.fork_point.is_some()),
                    ] {
                        if present {
                            update_parts.insert(name);
                        }
                    }
                }
                EventPayload::ProducerMessageAccepted { producer_owner, .. } => {
                    owners.insert(std::mem::discriminant(producer_owner));
                }
                EventPayload::ApprovalStep {
                    step: Open::Known(cookie_agent_protocol::ApprovalStep::UserDecision { .. }),
                    ..
                } => decided = true,
                _ => {}
            }
        }
    }
    for tag in [
        "acp_session_started",
        "acp_session_updated",
        "user_input_admitted",
        "user_input_submitted",
        "user_input_applied",
        "user_input_recalled",
        "user_input_queued",
        "producer_message_accepted",
        "producer_message_admitted",
        "producer_messages_claimed",
        "approval_requested",
        "approval_finalized",
        "tool_call_started",
        "tool_call_terminated",
        "tool_call_linked",
        "runtime_attached",
        "runtime_detached",
        "skill_loaded",
        "skill_invocation_noted",
        "session_permission_overlay_set",
        "session_title_committed",
        "memory_loaded",
        "model_attempt_started",
        "model_turn_committed",
        "run_started",
        "run_finished",
    ] {
        assert!(
            seen.contains(tag),
            "the scenario records no {tag}: {seen:?}"
        );
    }
    assert!(decided, "a user's decision");
    assert!(
        agents.contains("fixture") && agents.contains("second"),
        "{agents:?}"
    );
    use cookie_agent_protocol::{AcpDetachReason, AcpSessionStart};
    for reason in [AcpDetachReason::Forked, AcpDetachReason::Reverted] {
        assert!(reasons.contains(&reason), "{reasons:?}");
    }
    for how in [AcpSessionStart::New, AcpSessionStart::Forked] {
        assert!(starts.contains(&how), "{starts:?}");
    }
    assert!(in_system_prompt.contains(&true) && in_system_prompt.contains(&false));
    assert!(owners.len() >= 2, "producer owners: {owners:?}");
    assert_eq!(update_parts.len(), 6, "{update_parts:?}");
}

// --- recording ---------------------------------------------------------------

/// What the scripted model answers at one step of a turn.
#[derive(Clone, Debug)]
pub(super) enum Reply {
    Text(String),
    Tools(Vec<(&'static str, Value)>),
    /// Answers after a pause, so the recorder can act while the request is
    /// in flight.
    Slow(u64, Box<Reply>),
}

pub(super) fn text(text: &str) -> Reply {
    Reply::Text(text.into())
}

fn tool(name: &'static str, arguments: Value) -> Reply {
    Reply::Tools(vec![(name, arguments)])
}

/// A step that calls one tool.
pub(super) fn tool_reply(name: &'static str, arguments: Value) -> Reply {
    tool(name, arguments)
}

pub(super) type Rules = BTreeMap<&'static str, Vec<Reply>>;

/// The scenario's root, for `$child:<n>` in a reply's arguments: the handle
/// of the root's `n`th child. `$page` is the URL of the page the fixture
/// serves.
type Children = Arc<OnceLock<(Engine, SessionId)>>;

fn resolve_placeholders(value: &mut Value, children: &Children, page: &str) {
    match value {
        Value::String(text) => {
            if text == "$page" {
                *text = page.to_owned();
            } else if let Some(index) = text
                .strip_prefix("$child:")
                .and_then(|index| index.parse::<usize>().ok())
            {
                let (engine, root) = children.get().expect("the scenario's root");
                let mut sessions = engine
                    .tree(*root)
                    .expect("the root's tree")
                    .children
                    .into_iter()
                    .map(|child| child.session.session_id)
                    .collect::<Vec<_>>();
                sessions.sort();
                let child = sessions[index];
                *text = engine
                    .get_session(child)
                    .expect("child metadata")
                    .short_id
                    .unwrap_or_else(|| child.to_string());
            } else if let Some(rest) = text.strip_prefix("$text:") {
                *text = rest.to_owned();
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| resolve_placeholders(item, children, page)),
        Value::Object(members) => members
            .values_mut()
            .for_each(|member| resolve_placeholders(member, children, page)),
        _ => {}
    }
}

/// A turn's script is named by `#k:<name>` in the user message that starts
/// it (a run's input, a steer, a delegated prompt, a message); its step is
/// the number of assistant messages since then. Requests without tools are
/// the summarizer's.
///
/// A review answers with the verdict a `#v:<decision>` in the call under
/// review names; a call that names none gets the summarizer's text, which
/// fails the review, as in the scenarios recorded before the reviewer kept
/// its own log.
fn reply_to(rules: &Rules, request: &Value) -> Reply {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let message_text = |message: &Value| match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    if let Some(last) = messages.last().map(message_text)
        && let Some(block) = last.rfind("<approval_request>").map(|at| &last[at..])
        && let Some(at) = block.find("#v:")
    {
        let decision = block[at + 3..]
            .split(|character: char| !character.is_ascii_alphabetic())
            .next()
            .unwrap_or_default();
        return text(&format!(
            "{{\"decision\":\"{decision}\",\"reason\":\"The call asks for {decision}.\"}}"
        ));
    }
    if request
        .get("tools")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return text("Summary: the conversation explored, edited, and fetched files.");
    }
    for (index, message) in messages.iter().enumerate().rev() {
        if message["role"] != "user" {
            continue;
        }
        let content = message_text(message);
        let Some(start) = content.find("#k:") else {
            continue;
        };
        let name = content[start + 3..]
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '-'))
            .next()
            .unwrap_or_default();
        let step = messages[index..]
            .iter()
            .filter(|message| message["role"] == "assistant")
            .count();
        if let Some(reply) = rules.get(name).and_then(|steps| steps.get(step)) {
            return reply.clone();
        }
        return text("Done.");
    }
    text("Done.")
}

fn reply_body(reply: &Reply, call_prefix: &str) -> String {
    match reply {
        Reply::Text(text) => scripted_text_body(text),
        Reply::Tools(calls) => {
            let calls = calls
                .iter()
                .enumerate()
                .map(|(index, (name, arguments))| {
                    (format!("{call_prefix}-{index}"), *name, arguments.clone())
                })
                .collect::<Vec<_>>();
            scripted_tool_batch_body(
                &calls
                    .iter()
                    .map(|(id, name, arguments)| (id.as_str(), *name, arguments.clone()))
                    .collect::<Vec<_>>(),
            )
        }
        Reply::Slow(_, reply) => reply_body(reply, call_prefix),
    }
}

/// A fake OpenAI-compatible model answering every request from `rules`, and
/// serving `GET /page.md` for `webfetch`.
pub(super) async fn script_server(rules: Rules) -> String {
    script_server_with(rules, Arc::default()).await
}

async fn script_server_with(rules: Rules, children: Children) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("script listener");
    let address = listener.local_addr().expect("script address");
    let rules = Arc::new(rules);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let page = Arc::new(format!("http://{address}/page.md"));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let rules = Arc::clone(&rules);
            let calls = Arc::clone(&calls);
            let children = Arc::clone(&children);
            let page = Arc::clone(&page);
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt as _;
                let request = read_scripted_http_request(&mut socket).await;
                if request.starts_with(b"GET ") {
                    let body = "# Release notes\n\n- The protocols froze at 1.0.0.\n- Session logs stay readable.\n";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/markdown\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    return;
                }
                let start = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map_or(request.len(), |end| end + 4);
                let body = serde_json::from_slice(&request[start..]).unwrap_or(Value::Null);
                let mut reply = reply_to(&rules, &body);
                if let Reply::Slow(millis, inner) = reply {
                    tokio::time::sleep(Duration::from_millis(millis)).await;
                    reply = *inner;
                }
                if let Reply::Tools(calls) = &mut reply {
                    for (_, arguments) in calls {
                        resolve_placeholders(arguments, &children, &page);
                    }
                }
                let prefix = format!(
                    "call_{}",
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                );
                write_scripted_sse(&mut socket, &reply_body(&reply, &prefix)).await;
            });
        }
    });
    format!("http://{address}/v1")
}

pub(super) fn origin() -> EventOrigin {
    EventOrigin::new("client:golden").unwrap()
}

pub(super) fn user_text(text: &str) -> UserContent {
    text.into()
}

pub(super) async fn run(
    engine: &Engine,
    session: SessionId,
    selection: &RunSelection,
    key: &str,
    content: UserContent,
) -> cookie_agent_protocol::RunId {
    engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new(key).unwrap(),
                selection: selection.clone(),
                content,
            },
            origin(),
        )
        .await
        .expect("golden run")
        .run_id
}

/// Waits until nothing in `root`'s tree runs or waits, for a full second.
pub(super) async fn quiesce(engine: &Engine, root: SessionId) {
    fn running(tree: &cookie_agent_protocol::SessionTree) -> bool {
        tree.session.status == SessionStatus::Running || tree.children.iter().any(running)
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut quiet_since = None;
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "golden tree {root} did not settle"
        );
        let busy = running(&engine.tree(root).expect("golden tree"))
            || !engine
                .list_approvals(root, Some(ApprovalStatus::Escalated))
                .unwrap()
                .approvals
                .is_empty();
        if busy {
            quiet_since = None;
        } else if quiet_since
            .get_or_insert_with(tokio::time::Instant::now)
            .elapsed()
            >= Duration::from_secs(1)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Answers the escalated approvals of `session` as they come, in order.
async fn answer_approvals(engine: &Engine, session: SessionId, decisions: &[ApprovalUserDecision]) {
    for (index, decision) in decisions.iter().enumerate() {
        let approval = wait_for_escalated_approval(engine, session).await;
        let request_revision = serde_json::to_value(&approval.request)
            .expect("approval request JSON")["revision"]
            .as_u64()
            .expect("approval request revision");
        engine
            .approval_respond(
                ApprovalRespondParams {
                    session_id: approval.session_id,
                    approval_id: approval.request.approval_id(),
                    request_revision,
                    operation_fingerprint: approval.request.operation_fingerprint().clone(),
                    client_response_id: ClientResponseId::new(format!("golden-answer-{index}"))
                        .unwrap(),
                    decision: *decision,
                    feedback: None,

                    agent_option: None,
                },
                origin(),
            )
            .await
            .expect("answer a golden approval");
    }
}

/// The trees of the session store `sessions`, copied to `scenario` with their
/// snapshot.
async fn keep_scenario(sessions: &Path, scenario: &Path) {
    std::fs::create_dir_all(scenario).expect("scenario directory");
    let months = std::fs::read_dir(sessions)
        .expect("session store")
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(crate::session::Month::parse)
                .is_some()
        })
        .map(|entry| entry.path());
    for month in months {
        for root in scenario_roots(&month) {
            copy_test_tree(
                &month.join(root.to_string()),
                &scenario.join(root.to_string()),
            );
        }
    }
    let opened = open_scenario(scenario, Opening::ReadOnly).await;
    let mut read = snapshot(&opened.engine, &opened.roots).await;
    opened.engine.shutdown().await;
    for_each_session(&mut read, |session| {
        let id = session["session_id"]
            .as_str()
            .expect("session ID")
            .to_owned();
        let lines = log_lines(scenario, &id);
        let events = session["events"].as_array().expect("events");
        let unlike = events
            .iter()
            .filter(|event| !lines.contains(event))
            .cloned()
            .collect::<Vec<_>>();
        session["events"] = json!({ "count": events.len(), "unlike_log": unlike });
    });
    let mut written = String::new();
    write_snapshot(&read, 0, &mut written);
    written.push('\n');
    std::fs::write(scenario.join(SNAPSHOT_FILE), written).expect("write the snapshot");
}

/// `value` as JSON, indented down to the members of a session and compact
/// below, so each event and history turn is one line.
fn write_snapshot(value: &Value, depth: usize, out: &mut String) {
    const INDENTED_DEPTH: usize = 6;
    let indent = |depth: usize| "  ".repeat(depth);
    match value {
        Value::Object(members) if depth < INDENTED_DEPTH && !members.is_empty() => {
            out.push_str("{\n");
            for (index, (key, member)) in members.iter().enumerate() {
                out.push_str(&indent(depth + 1));
                out.push_str(&Value::from(key.as_str()).to_string());
                out.push_str(": ");
                write_snapshot(member, depth + 1, out);
                out.push_str(if index + 1 < members.len() {
                    ",\n"
                } else {
                    "\n"
                });
            }
            out.push_str(&indent(depth));
            out.push('}');
        }
        Value::Array(items) if depth < INDENTED_DEPTH && !items.is_empty() => {
            out.push_str("[\n");
            for (index, item) in items.iter().enumerate() {
                out.push_str(&indent(depth + 1));
                write_snapshot(item, depth + 1, out);
                out.push_str(if index + 1 < items.len() { ",\n" } else { "\n" });
            }
            out.push_str(&indent(depth));
            out.push(']');
        }
        value => out.push_str(&value.to_string()),
    }
}

fn sessions_of(fixture_root: &Path) -> PathBuf {
    fixture_root
        .join("data")
        .join(crate::session::SESSIONS_ROOT_DIR)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "records the golden session logs; run explicitly with COOKIE_GOLDEN_HISTORY_OUT"]
async fn record_golden_history() {
    let out = PathBuf::from(
        std::env::var_os(OUT_VARIABLE)
            .unwrap_or_else(|| panic!("set {OUT_VARIABLE} to a directory that does not exist")),
    );
    assert!(
        !out.exists(),
        "{} exists; the recorder never writes over golden logs",
        out.display()
    );
    let out = out.join(HISTORY_FORMAT.to_string());
    std::fs::create_dir_all(&out).expect("output directory");
    // While a recorder is being written, `COOKIE_GOLDEN_HISTORY_ONLY` names
    // the one scenario to record.
    let only = std::env::var("COOKIE_GOLDEN_HISTORY_ONLY").ok();
    let wanted = |name: &str| only.as_deref().is_none_or(|only| only == name);
    if wanted("coding") {
        record_coding(&out.join("coding")).await;
    }
    if wanted("delegation") {
        record_delegation(&out.join("delegation")).await;
    }
    if wanted("replay") {
        record_replay(&out.join("replay")).await;
    }
    if wanted("revert-fork") {
        record_revert_and_fork(&out.join("revert-fork")).await;
    }
    for (name, fail_native) in [
        ("native-compaction", false),
        ("native-compaction-fallback", true),
    ] {
        if wanted(name) {
            record_native_compaction(&out.join(name), fail_native).await;
        }
    }
    if wanted("acp") {
        record_acp(&out.join("acp")).await;
    }
    if wanted("reviewer") {
        record_reviewer(&out.join("reviewer")).await;
    }
    eprintln!("recorded the golden session logs in {}", out.display());
}

/// Text, reasoning, and native replay of signed Anthropic thinking.
async fn record_replay(scenario: &Path) {
    let (endpoint, _captured) = anthropic_replay_server(vec![
        AnthropicReplayResponse::Thinking(Some("golden-signature")),
        AnthropicReplayResponse::Thinking(Some("golden-signature-2")),
    ])
    .await;
    let primary = "---\ndescription: Replay\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nThink, then answer.\n";
    let (fixture, selection) = custom_fixture_with_capabilities(
        &endpoint,
        primary,
        None,
        None,
        false,
        None,
        None,
        4_096,
        None,
        "anthropic-compatible",
        Some(ANTHROPIC_REPLAY_CAPABILITIES),
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    run_replay_test_turn(&fixture, session, &selection, "think-first").await;
    run_replay_test_turn(&fixture, session, &selection, "think-again").await;
    quiesce(&fixture.engine, session).await;
    fixture.engine.shutdown().await;
    keep_scenario(&sessions_of(fixture._directory.path()), scenario).await;
}

/// A revert to the end of the first run, and a fork of it.
async fn record_revert_and_fork(scenario: &Path) {
    let rules = Rules::from([
        ("one", vec![text("The first answer.")]),
        (
            "two",
            vec![
                tool("bash", json!({"command": "echo second"})),
                text("The second answer."),
            ],
        ),
        ("three", vec![text("The answer after the revert.")]),
        ("forked", vec![text("The answer in the fork.")]),
    ]);
    let endpoint = script_server(rules).await;
    let (fixture, selection) = coding_fixture(&endpoint, CODING_AGENT, None).await;
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    run(
        engine,
        session,
        &selection,
        "one",
        user_text("#k:one Answer once."),
    )
    .await;
    quiesce(engine, session).await;
    let first_end = engine
        .inner
        .store
        .log(session)
        .unwrap()
        .last_event()
        .unwrap()
        .seq;
    run(
        engine,
        session,
        &selection,
        "two",
        user_text("#k:two Run a command."),
    )
    .await;
    quiesce(engine, session).await;
    let fork = engine
        .fork_session(session, first_end, origin(), None)
        .await
        .expect("fork the first run")
        .session_id;
    run(
        engine,
        fork,
        &selection,
        "forked",
        user_text("#k:forked Go on in the fork."),
    )
    .await;
    quiesce(engine, fork).await;
    engine
        .revert_session(session, first_end, origin(), None)
        .await
        .expect("revert the second run");
    run(
        engine,
        session,
        &selection,
        "three",
        user_text("#k:three Answer again."),
    )
    .await;
    quiesce(engine, session).await;
    engine.shutdown().await;
    keep_scenario(&sessions_of(fixture.root.path()), scenario).await;
}

const REVIEWED_AGENT: &str = "---\ndescription: Golden reviewed agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n  bash: ask\n  write: ask\n  delegate: allow\n---\nWork in the repository.\n";

const REVIEWED_WORKER: &str = "---\ndescription: Golden reviewed worker\nmode: subagent\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  bash: ask\n---\nDo what the parent asks.\n";

/// The coding model with a window small enough that the reviewer's
/// conversation starts over within the scenario.
fn reviewer_capabilities() -> String {
    CODING_CAPABILITIES.replace("context_tokens = 200000", "context_tokens = 7000")
}

fn reviewer_rules() -> Rules {
    let bash = |command: String| tool("bash", json!({ "command": command }));
    let mut grow = (0..9)
        .map(|index| {
            bash(format!(
                "echo step {index} #v:allow {}",
                "padding ".repeat(48)
            ))
        })
        .collect::<Vec<_>>();
    grow.push(text("Every step ran."));
    Rules::from([
        (
            "start",
            vec![
                tool(
                    "write",
                    json!({"path": "notes.md", "content": "# Notes #v:allow\n\nThe build passes.\n"}),
                ),
                bash("echo build #v:allow".into()),
                bash("echo push #v:ask".into()),
                bash("rm -rf target #v:deny".into()),
                text("The notes are written; the clean was refused."),
            ],
        ),
        ("grow", grow),
        (
            "delegate",
            vec![
                tool(
                    "delegate_subagent",
                    json!({"agent_type": "worker", "description": "Check the child", "prompt": "#k:child Run the child's check."}),
                ),
                text("The child checked."),
            ],
        ),
        (
            "child",
            vec![
                bash("echo child check #v:allow".into()),
                text("The child's check passed."),
            ],
        ),
        (
            "forked",
            vec![
                bash("echo in the fork #v:allow".into()),
                text("The fork ran its step."),
            ],
        ),
        (
            "again",
            vec![
                bash("echo after the revert #v:allow".into()),
                text("Ran after the revert."),
            ],
        ),
    ])
}

/// The approval reviewer with its own log (`docs/site/specs/forever-sessions.md`
/// section 5.5): verdicts of each kind, a user's answer to an escalation,
/// enough reviews for the conversation to start over, a fork after them that
/// copies the reviewer's log and reviews on in the fork, a revert before them
/// that cuts it, and a delegated child's reviews.
async fn record_reviewer(scenario: &Path) {
    let endpoint = script_server(reviewer_rules()).await;
    let (fixture, selection) = coding_fixture_with(
        &endpoint,
        REVIEWED_AGENT,
        None,
        &reviewer_capabilities(),
        Some(REVIEWED_WORKER),
    )
    .await;
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    run(
        engine,
        session,
        &selection,
        "start",
        user_text("#k:start Write the notes, check the build, and push."),
    )
    .await;
    answer_approvals(engine, session, &[ApprovalUserDecision::ApproveOnce]).await;
    quiesce(engine, session).await;
    let first_end = log_tip(engine, session);
    run(
        engine,
        session,
        &selection,
        "grow",
        user_text("#k:grow Run every step."),
    )
    .await;
    quiesce(engine, session).await;
    let grown = log_tip(engine, session);
    let fork = engine
        .fork_session(session, grown, origin(), None)
        .await
        .expect("fork after the reviews that started over")
        .session_id;
    run(
        engine,
        fork,
        &selection,
        "forked",
        user_text("#k:forked Go on in the fork."),
    )
    .await;
    quiesce(engine, fork).await;
    engine
        .revert_session(session, first_end, origin(), None)
        .await
        .expect("revert to the first run's end");
    run(
        engine,
        session,
        &selection,
        "again",
        user_text("#k:again Run one more step."),
    )
    .await;
    quiesce(engine, session).await;
    run(
        engine,
        session,
        &selection,
        "delegate",
        user_text("#k:delegate Have a worker check it."),
    )
    .await;
    quiesce(engine, session).await;
    engine.shutdown().await;
    keep_scenario(&sessions_of(fixture.root.path()), scenario).await;
}

/// Native compaction, and its fallback to a summary when it fails.
async fn record_native_compaction(scenario: &Path, fail_native: bool) {
    let (endpoint, _captured) = native_compaction_server(fail_native).await;
    let (fixture, selection) = managed_openai_compaction_fixture(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    run(
        &fixture.engine,
        session,
        &selection,
        "compact-me",
        user_text("compact this context"),
    )
    .await;
    quiesce(&fixture.engine, session).await;
    assert!(
        fixture
            .engine
            .compact_session(session, Some("keep the decisions"), origin())
            .await
            .expect("compaction")
    );
    quiesce(&fixture.engine, session).await;
    fixture.engine.shutdown().await;
    keep_scenario(&sessions_of(fixture._directory.path()), scenario).await;
}

// --- the ACP scenario --------------------------------------------------------

/// The scripted agent `fixture` takes a system prompt; `second`, on the same
/// script, gets its context ahead of its first prompt and declines steers.
const ACP_DOCUMENT: &str = "---\ndescription: Golden ACP agent\nmode: all\nacp: {}\npermissions:\n  bash: allow\n  switch_mode: ask\n  delegate: { fixture: allow }\n---\nReview what you are given.\n";
const ACP_ENV: &[(&str, &str)] = &[
    ("ACP_FIXTURE_FORK", "1"),
    ("ACP_FIXTURE_DELETE", "1"),
    ("ACP_FIXTURE_SYSTEM_PROMPT", "1"),
];

fn second_agent() -> cookie_agent_protocol::AgentId {
    cookie_agent_protocol::AgentId::new("second").unwrap()
}

fn second_selection() -> RunSelection {
    RunSelection {
        agent: second_agent(),
        model: cookie_agent_protocol::acp_model_selection(&second_agent()),
        preset: None,
        acp_runtime: None,
    }
}

/// Waits until the log of `session` records `found`.
async fn logged(engine: &Engine, session: SessionId, found: impl Fn(&EventPayload) -> bool) {
    with_watchdog("a record of the golden ACP scenario", async {
        while !engine
            .inner
            .store
            .log(session)
            .expect("session log")
            .event_snapshot()
            .iter()
            .any(|event| found(&event.payload))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

/// The latest record of `session`'s log.
fn log_tip(engine: &Engine, session: SessionId) -> u64 {
    engine
        .inner
        .store
        .log(session)
        .expect("session log")
        .last_event()
        .expect("a log's creation")
        .seq
}

/// Every fold's records in one tree (`docs/site/specs/forever-sessions.md`
/// section 7): several ACP agents and their sessions, a detach by a fork and
/// by a revert, options, a compaction, machines, queued, steered, and recalled
/// input, producer messages, wakes, approvals, skills, delegations, and
/// cancelled runs.
async fn record_acp(scenario: &Path) {
    let second = format!(
        "[runtimes.local.acp.agents.second]\ncommand = [{}, {}]\nenv = {{ ACP_FIXTURE_FORK = \"1\", ACP_FIXTURE_DELETE = \"1\", ACP_FIXTURE_DECLINE_STEERS = \"1\" }}\ndescription = \"Second fixture agent\"\n",
        serde_json::to_string(python_command()).unwrap(),
        serde_json::to_string(&*fixture_script().to_string_lossy()).unwrap(),
    );
    let acp = acp_fixture_with(
        ACP_ENV,
        &[("fixture.md", ACP_DOCUMENT), ("second.md", ACP_DOCUMENT)],
        &second,
    )
    .await;
    let engine = acp.engine();
    let project = acp.workspace().join("project");
    let skills = project.join(".cookie-agent/skills/golden");
    std::fs::create_dir_all(&skills).expect("skill directory");
    write_private_test_file(
        &skills.join("SKILL.md"),
        "---\nname: golden\ndescription: The golden skill.\n---\nDo the golden thing.\n",
    );
    let (stop_labpc, labpc) = dial_in(&acp, "labpc", "fixture").await;
    let elsewhere = private_tempdir();

    // A session of `fixture` with starting options, then its first turns.
    let session = engine
        .create_session_with_agent_options(
            selection(),
            None,
            Vec::new(),
            crate::runtime::attachments::SessionRuntimes {
                runtimes: Some(vec![crate::SessionRuntimeRequest {
                    runtime_id: "local".to_owned(),
                    capability: Some("coding".to_owned()),
                    workspace: Some(project),
                }]),
                global: true,
                cwd: None,
            },
            None,
            vec![cookie_agent_protocol::AcpOptionChoice {
                option_id: "reasoning_effort".to_owned(),
                value: "high".to_owned(),
            }],
            None,
        )
        .await
        .expect("session created")
        .session_id;
    turn(engine, session, "one", "say one").await;
    logged(engine, session, |payload| {
        matches!(payload, EventPayload::AcpSessionUpdated { update } if update.fork_point.is_some())
    })
    .await;
    engine
        .set_session_agent_option(cookie_agent_protocol::SessionAgentOptionSetParams {
            session_id: session,
            option_id: "mode".to_owned(),
            value: "plan".to_owned(),
        })
        .await
        .expect("option set");
    turn(engine, session, "work", "work").await;
    let first_end = log_tip(engine, session);
    turn(engine, session, "title", "title Golden title").await;

    // An approval the user answers, and a permission overlay.
    let run_id = start(engine, session, "ask", "permission switch_mode plan").await;
    let approval = wait_for_escalated_approval(engine, session).await;
    approve_once(engine, &approval, "golden-approve").await;
    finished(engine, session, run_id).await;
    wait_for_session_not_running(engine, session).await;
    engine
        .append(
            session,
            None,
            origin(),
            EventPayload::SessionPermissionOverlaySet {
                overlay: cookie_agent_protocol::SessionPermissionOverlay { rules: Vec::new() },
            },
        )
        .await
        .expect("a permission overlay");

    // A machine that comes, and goes, between runs.
    let attachment = engine
        .attach_runtime(
            session,
            "labpc",
            None,
            Some(elsewhere.path().to_owned()),
            origin(),
        )
        .await
        .expect("labpc attaches");
    turn(engine, session, "with-labpc", "say with labpc").await;
    engine
        .detach_runtime(session, attachment.name, origin())
        .await
        .expect("labpc detaches");
    turn(engine, session, "without-labpc", "say without labpc").await;

    // A skill loaded twice, and the agent's compaction.
    let skill = "mcp call skill {\"name\":\"golden\"}";
    for key in ["skill", "skill-again"] {
        let answer = final_text(&turn(engine, session, key, skill).await);
        assert!(answer.ends_with("error: False"), "{answer}");
    }
    assert!(
        engine
            .compact_session_result(session, Some("the review"), origin())
            .await
            .expect("forwarded")
            .compacted
    );
    wait_for_session_not_running(engine, session).await;

    // Input steered into a running turn, and input queued behind it.
    let run_id = start(engine, session, "steer", "steer-wait 10").await;
    logged(engine, session, |payload| {
        matches!(payload, EventPayload::ModelAttemptStarted { .. })
    })
    .await;
    for (key, text, delivery) in [
        ("steered", "the steer", InputDelivery::Steer),
        ("queued", "say queued", InputDelivery::Queue),
    ] {
        engine
            .submit_input(
                SessionSubmitInputParams {
                    session_id: session,
                    content: text.into(),
                    delivery,
                    selection: None,
                    client_run_id: ClientRunId::new(key).unwrap(),
                    reset_fallback: false,
                },
                origin(),
            )
            .await
            .expect("input submitted");
    }
    finished(engine, session, run_id).await;
    quiesce(engine, session).await;

    // The agent's own wake, and a producer's message.
    turn(engine, session, "wake", "wake-later 0.3 the build passed").await;
    logged(engine, session, |payload| {
        matches!(
            payload,
            EventPayload::ProducerMessageAccepted {
                producer_owner: cookie_agent_protocol::ProducerOwner::AcpAgent { .. },
                ..
            }
        )
    })
    .await;
    quiesce(engine, session).await;
    let authority = super::producers::producer_authority();
    let cookie_agent_protocol::ProducerOwner::Delegation {
        invocation_id: producer_invocation,
    } = authority.owner
    else {
        panic!("the test producer is a delegation's");
    };
    let producer = engine
        .register_producer(session, authority.clone())
        .await
        .expect("a producer");
    engine
        .send_producer_message(
            session,
            authority.clone(),
            producer,
            cookie_agent_protocol::ProducerDeliveryMode::Steer,
            super::producers::producer_key("golden-message"),
            cookie_agent_protocol::SafeDisplayText::new("Golden producer").unwrap(),
            "say from a producer".into(),
        )
        .await
        .expect("a producer message");
    quiesce(engine, session).await;
    engine
        .unregister_producer(session, authority, producer)
        .await
        .expect("the producer goes");

    // A delegation in the background, whose completion comes back.
    let call = format!(
        "mcp call delegate_subagent {}",
        json!({
            "description": "Golden child",
            "prompt": "say from the child",
            "agent_type": "fixture",
            "background": true,
        })
    );
    let answer = final_text(&turn(engine, session, "delegate", &call).await);
    assert!(answer.ends_with("error: False"), "{answer}");
    logged(engine, session, |payload| {
        matches!(
            payload,
            EventPayload::ProducerMessageAccepted {
                producer_owner: cookie_agent_protocol::ProducerOwner::Delegation { invocation_id },
                ..
            } if *invocation_id != producer_invocation
        )
    })
    .await;
    quiesce(engine, session).await;

    // A second agent in the same session, a steer it declines and the user
    // takes back, a cancel, and queued input the cancel voids.
    run(
        engine,
        session,
        &second_selection(),
        "second-one",
        "say from second".into(),
    )
    .await;
    quiesce(engine, session).await;
    logged(engine, session, |payload| {
        matches!(payload, EventPayload::AcpSessionUpdated { update } if update.fork_point.is_some())
    })
    .await;
    let run_id = run(
        engine,
        session,
        &second_selection(),
        "second-wait",
        "decline-wait".into(),
    )
    .await;
    logged(engine, session, |payload| {
        matches!(payload, EventPayload::ModelAttemptStarted { resolved_model, .. }
            if resolved_model.selection == second_selection().model)
    })
    .await;
    let steered = engine
        .steer(run_id, "say later".into(), origin())
        .await
        .expect("a steer")
        .user_input_seq
        .expect("admitted");
    with_watchdog("the agent declined", async {
        while engine.input_is_steering(session, steered) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    engine
        .recall_input(session, steered, origin())
        .await
        .expect("taken back");
    let queued = engine
        .submit_input(
            SessionSubmitInputParams {
                session_id: session,
                content: "say never".into(),
                delivery: InputDelivery::Queue,
                selection: None,
                client_run_id: ClientRunId::new("voided").unwrap(),
                reset_fallback: false,
            },
            origin(),
        )
        .await;
    queued.expect("input queued");
    engine.cancel_run(run_id, origin()).await.expect("cancel");
    finished(engine, session, run_id).await;
    quiesce(engine, session).await;

    // Back to `fixture`, then a fork of the session's early turns, which
    // branches `fixture`'s session off where the cut leaves it.
    turn(engine, session, "back", "recall").await;
    let before_revert = log_tip(engine, session);
    let fork = engine
        .fork_session(session, first_end, origin(), None)
        .await
        .expect("forked")
        .session_id;
    turn(engine, fork, "fork-recall", "recall").await;
    quiesce(engine, fork).await;
    run(
        engine,
        fork,
        &second_selection(),
        "fork-second",
        "say from second in the fork".into(),
    )
    .await;
    quiesce(engine, fork).await;

    // A revert, whose next turns branch both agents' sessions.
    turn(engine, session, "more", "say more").await;
    run(
        engine,
        session,
        &second_selection(),
        "second-more",
        "say more of second".into(),
    )
    .await;
    quiesce(engine, session).await;
    engine
        .revert_session(session, before_revert, origin(), None)
        .await
        .expect("reverted");
    turn(engine, session, "after-revert", "recall").await;
    run(
        engine,
        session,
        &second_selection(),
        "second-after-revert",
        "recall".into(),
    )
    .await;
    quiesce(engine, session).await;

    stop_labpc.cancel();
    let _ = labpc.await;
    engine.shutdown().await;
    keep_scenario(&sessions_of(acp.fixture._directory.path()), scenario).await;
}

// --- the coding fixture ------------------------------------------------------

const CODING_CAPABILITIES: &str = "input = [\"text\", \"image\", \"video\"]\noutput = [\"text\"]\ncontext_tokens = 200000\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = true\nnative_replay = \"unsupported\"\nmedia = { image = { mime_types = [\"image/png\"], max_bytes = 4194304, max_count = 4 }, video = { mime_types = [\"video/mp4\"], max_bytes = 4194304, max_count = 1 } }";

pub(super) const CODING_AGENT: &str = "---\ndescription: Golden coding agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n  bash: allow\n  write: ask\n  webfetch: ask\n  delegate: allow\n  message: allow\n---\nWork in the repository.\n";

/// An engine whose local runtime is the coding runtime over the runtime
/// protocol, working in the fixture's directory, with `plugin` configured.
pub(super) struct CodingFixture {
    pub(super) engine: Engine,
    pub(super) root: PanicResistantTempDir,
}

pub(super) async fn coding_fixture(
    endpoint: &str,
    primary: &str,
    plugin: Option<(&str, cookie_agent_config::PluginConfig)>,
) -> (CodingFixture, RunSelection) {
    coding_fixture_with(endpoint, primary, plugin, CODING_CAPABILITIES, None).await
}

/// [`coding_fixture`] with the model's `capabilities` and a `worker` agent.
async fn coding_fixture_with(
    endpoint: &str,
    primary: &str,
    plugin: Option<(&str, cookie_agent_config::PluginConfig)>,
    capabilities: &str,
    worker: Option<&str>,
) -> (CodingFixture, RunSelection) {
    let (fixture, selection) = custom_fixture_with_capabilities(
        endpoint,
        primary,
        None,
        None,
        false,
        None,
        None,
        200_000,
        worker,
        "openai-compatible",
        Some(capabilities),
    );
    fixture.engine.shutdown().await;
    let Fixture {
        _directory: root,
        mut config,
        manager,
        ..
    } = fixture;
    if let Some((name, plugin)) = plugin {
        config.plugins = [(name.to_owned(), plugin)].into_iter().collect();
    }
    // A summary keeps a short recent tail, so a manual compaction of a short
    // conversation still has something to summarize.
    config.runtime.context_compaction.keep_recent_tokens = 300;
    let directory = root.path();
    write_workspace(directory);
    let engine = crate::Engine::open_async(crate::EngineOptions {
        data_dir: directory.join("data"),
        cwd: directory.to_owned(),
        config,
        model_manager: manager,
        runtimes: cookie_agent_runtime_api::RuntimeOptions {
            local: cookie_agent_runtime_api::LocalRuntime::Protocol(
                cookie_agent_coding_runtime::server::in_process_connector(),
            ),
            ..cookie_agent_runtime_api::RuntimeOptions::new(directory)
        },
    })
    .await
    .expect("engine with the coding runtime");
    // Engine unit tests register the base layer themselves.
    engine.register_base_layer().expect("the base layer");
    engine.inner.plugins.await_eager_ready().await;
    (CodingFixture { engine, root }, selection)
}

const LIBRARY: &str = "//! A small library the golden runs read and edit.\n\n/// Adds two numbers.\npub fn add(left: u64, right: u64) -> u64 {\n    left + right\n}\n\n/// Subtracts the second number from the first.\npub fn subtract(left: u64, right: u64) -> u64 {\n    left - right\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn adds() {\n        assert_eq!(add(2, 2), 4);\n    }\n}\n";

fn write_workspace(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), LIBRARY).unwrap();
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(root.join("assets/pixel.png"), super::media::png(2, 2, 16)).unwrap();
    // Long enough that reading it gives an output history format 2 stores
    // in a blob.
    let changelog = (1..=400)
        .map(|release| format!("- 0.{release}.0: steadier subtraction in area {release}\n"))
        .collect::<String>();
    std::fs::write(root.join("CHANGELOG.md"), changelog).unwrap();
    let skill = root.join(".agents/skills/release-notes");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: release-notes\ndescription: Write release notes\n---\nCollect the changes since the last release and list them by area.\n",
    )
    .unwrap();
}

/// A minimal MP4: an `ftyp` box and a short `mdat` box.
fn mp4() -> Vec<u8> {
    let mut bytes = 24_u32.to_be_bytes().to_vec();
    bytes.extend_from_slice(b"ftypisom");
    bytes.extend_from_slice(&0x200_u32.to_be_bytes());
    bytes.extend_from_slice(b"isommp41");
    let data = [7_u8; 24];
    bytes.extend_from_slice(&(8 + data.len() as u32).to_be_bytes());
    bytes.extend_from_slice(b"mdat");
    bytes.extend_from_slice(&data);
    bytes
}

async fn upload(
    engine: &Engine,
    session: SessionId,
    bytes: &[u8],
    filename: &str,
) -> cookie_agent_protocol::ArtifactRef {
    use sha2::Digest as _;
    let id = engine
        .begin_upload(1, session, bytes.len() as u64, Some(filename.into()))
        .await
        .expect("begin upload");
    engine
        .upload_chunk(1, &id, 0, bytes.to_vec())
        .await
        .expect("upload chunk");
    engine
        .finish_upload(1, &id, &format!("{:x}", sha2::Sha256::digest(bytes)))
        .await
        .expect("finish upload")
        .artifact
}

/// The Python reference plugin (`examples/plugins/python`).
fn reference_plugin() -> cookie_agent_config::PluginConfig {
    cookie_agent_config::PluginConfig {
        command: vec![
            python_command().into(),
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../examples/plugins/python/reference_plugin.py"
            )
            .into(),
        ],
        env: BTreeMap::new(),
        cwd: None,
        enabled: true,
        producer_messaging: true,
        interception_timeout_ms: 10_000,
        startup_timeout_ms: 10_000,
        shutdown_grace_ms: 3_000,
        always_load: false,
    }
}

fn golden_plugin() -> cookie_agent_config::PluginConfig {
    let mut plugin = interception_plugin(
        "golden",
        &[
            (
                "FIXTURE_TOOLS",
                json!([{
                    "name": "golden_report",
                    "description": "Report on the repository.",
                    "parameters": {"type": "object", "properties": {"text": {"type": "string"}}},
                }])
                .to_string(),
            ),
            (
                "FIXTURE_CAPABILITIES",
                json!({
                    "producer_messaging": false,
                    "subscribe_events": [],
                    "subscribe_bus": false,
                    "publish_bus": false,
                    "intercept": [],
                })
                .to_string(),
            ),
            (
                "FIXTURE_TOOL_DISPLAY_BLOCKS",
                json!([
                    {"type": "json", "value": {"files": 1, "functions": ["add", "subtract"], "clean": true}},
                    {"type": "link", "url": "https://example.com/report", "label": "Full report"},
                    {"type": "code", "text": "pub fn add(left: u64, right: u64) -> u64", "language": "rust", "start_line": 4},
                    {"type": "key_value", "entries": [{"key": "files", "value": "1"}, {"key": "functions", "value": "2"}]},
                ])
                .to_string(),
            ),
        ],
    );
    plugin.interception_timeout_ms = 10_000;
    plugin
}

/// Tools of every kind against the coding runtime, a plugin's tool, user
/// media, approvals, background work, and a summary compaction.
async fn record_coding(scenario: &Path) {
    let endpoint = script_server(coding_rules()).await;
    let (fixture, selection) =
        coding_fixture(&endpoint, CODING_AGENT, Some(("golden", golden_plugin()))).await;
    let engine = &fixture.engine;
    let workspace = fixture.root.path();
    let session = engine.create_session(selection.clone()).unwrap().session_id;

    let image = upload(
        engine,
        session,
        &super::media::png(2, 2, 16),
        "screenshot.png",
    )
    .await;
    let video = upload(engine, session, &mp4(), "clip.mp4").await;
    let content = UserContent::new(vec![
        Open::Known(UserContentPart::Text {
            text: "#k:explore Look around the repository; here is what I see.".into(),
        }),
        Open::Known(UserContentPart::File { artifact: image }),
        Open::Known(UserContentPart::File { artifact: video }),
    ])
    .unwrap();
    run(engine, session, &selection, "explore", content).await;
    quiesce(engine, session).await;

    run(
        engine,
        session,
        &selection,
        "change",
        user_text("#k:change Fix the subtraction, note it, and fetch the release notes."),
    )
    .await;
    answer_approvals(
        engine,
        session,
        &[
            ApprovalUserDecision::ApproveOnce,
            ApprovalUserDecision::Reject,
            ApprovalUserDecision::ApproveTree,
        ],
    )
    .await;
    quiesce(engine, session).await;

    run(
        engine,
        session,
        &selection,
        "later",
        user_text("#k:later Start the slow check in the background."),
    )
    .await;
    quiesce(engine, session).await;

    assert!(
        engine
            .compact_session(session, Some("keep the edits"), origin())
            .await
            .expect("summary compaction")
    );
    quiesce(engine, session).await;
    run(
        engine,
        session,
        &selection,
        "after",
        user_text("#k:after Carry on from the summary."),
    )
    .await;
    quiesce(engine, session).await;
    engine.shutdown().await;
    keep_scenario(&sessions_of(workspace), scenario).await;
}

fn coding_rules() -> Rules {
    Rules::from([
        (
            "explore",
            vec![
                Reply::Tools(vec![
                    ("read", json!({"path": "src/lib.rs"})),
                    ("read", json!({"path": "src"})),
                    ("read", json!({"path": "assets/pixel.png"})),
                    ("read", json!({"path": "missing.txt"})),
                    ("read", json!({"path": "CHANGELOG.md"})),
                ]),
                Reply::Tools(vec![
                    (
                        "bash",
                        json!({"command": "printf '\\033[32mok\\033[0m 2 tests passed\\n'; printf '\\033[33mwarning\\033[0m: unused import\\n' >&2"}),
                    ),
                    ("bash", json!({"command": "echo checking; exit 3"})),
                ]),
                tool("bash", json!({"command": "seq 1 14000"})),
                tool(
                    "golden_report",
                    json!({"text": "Summarize the repository."}),
                ),
                text("I read the library, its directory, and the picture; the tests pass."),
            ],
        ),
        (
            "change",
            vec![
                tool(
                    "edit",
                    json!({
                        "path": "src/lib.rs",
                        "old_string": "    left - right\n",
                        "new_string": "    left.saturating_sub(right)\n",
                    }),
                ),
                tool(
                    "write",
                    json!({"path": "notes.md", "content": "# Notes\n\nSubtraction saturates now.\n"}),
                ),
                tool("webfetch", json!({"url": "$page"})),
                tool("webfetch", json!({"url": "$page"})),
                tool("skill", json!({"name": "release-notes"})),
                text(
                    "Subtraction saturates; the notes were declined; the release notes are fetched.",
                ),
            ],
        ),
        (
            "later",
            vec![
                tool(
                    "bash",
                    json!({"command": "sleep 0.3; echo the slow check passed", "background": true}),
                ),
                text("The check runs in the background; I will report when it ends."),
                text("The slow check passed."),
            ],
        ),
        ("after", vec![text("Carrying on from the summary.")]),
    ])
}

const DELEGATING_AGENT: &str = "---\ndescription: Golden delegating agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n  text_read:\n    \"*.txt\": allow\n  delegate: allow\n  message: allow\n---\nDelegate, then report.\n";

fn delegation_rules() -> Rules {
    let delegate = |description: &str, prompt: &str| json!({"agent_type": "worker", "description": description, "prompt": prompt});
    Rules::from([
        (
            "delegate",
            vec![
                tool(
                    "delegate_subagent",
                    delegate("Survey the module", "#k:child-a Survey the module."),
                ),
                tool("delegate_subagent", {
                    let mut background =
                        delegate("Write the report", "#k:child-b Write the report.");
                    background["background"] = json!(true);
                    background
                }),
                text("The survey is in; the report is being written."),
                text("The report arrived."),
            ],
        ),
        (
            "child-a",
            vec![text("Child A: the module has two functions.")],
        ),
        (
            "child-b",
            vec![Reply::Slow(
                300,
                Box::new(text("Child B: the report is written.")),
            )],
        ),
        (
            "resume",
            vec![
                tool("delegate_subagent", {
                    let mut resumed =
                        delegate("Survey again", "#k:child-a2 Look at the tests too.");
                    resumed["resume_session_id"] = json!("$child:0");
                    resumed
                }),
                tool("get_subagent_result", json!({"session_id": "$child:1"})),
                tool(
                    "send_message",
                    json!({"to": "$child:0", "body": "#k:child-a3 One more thing: check the docs.", "mode": "queue"}),
                ),
                tool(
                    "send_message",
                    json!({"to": "$child:1", "body": "#k:child-b2 Add a summary line.", "mode": "steer"}),
                ),
                text("Asked both subagents one more thing."),
                text("A subagent answered."),
                text("Both subagents answered."),
            ],
        ),
        ("child-a2", vec![text("Child A: the tests cover addition.")]),
        ("child-a3", vec![text("Child A: the docs are current.")]),
        ("child-b2", vec![text("Child B: added a summary line.")]),
        (
            "count",
            vec![
                tool(
                    "word_count",
                    json!({"text": "one two three\nfour", "label": "notes.txt"}),
                ),
                tool(
                    "word_count",
                    json!({"text": "forbidden words", "label": "secret.txt"}),
                ),
                text("Counted the notes; the plugin refused the secret."),
                text("The plugin's count arrived."),
            ],
        ),
        ("goal", vec![text("Working through the survey checklist.")]),
        (
            "hold",
            vec![
                Reply::Slow(1_500, Box::new(tool("read", json!({"path": "src/lib.rs"})))),
                text("Read the library."),
            ],
        ),
        ("steered", vec![text("Checked the tests as well.")]),
        ("queued", vec![text("Summarized the session.")]),
    ])
}

/// Foreground, background, and resumed delegation, messages between
/// sessions (steered and queued), a goal whose pending reminder is
/// discarded when it completes, a plugin's tool, hook, and queued producer
/// message, and user input steered into a run and queued behind it.
async fn record_delegation(scenario: &Path) {
    let children = Children::default();
    let endpoint = script_server_with(delegation_rules(), Arc::clone(&children)).await;
    let (fixture, selection) = coding_fixture(
        &endpoint,
        DELEGATING_AGENT,
        Some(("reference", reference_plugin())),
    )
    .await;
    let engine = &fixture.engine;
    let root = engine.create_session(selection.clone()).unwrap().session_id;
    let _ = children.set((engine.clone(), root));

    run(
        engine,
        root,
        &selection,
        "delegate",
        user_text("#k:delegate Survey and report."),
    )
    .await;
    quiesce(engine, root).await;
    run(
        engine,
        root,
        &selection,
        "resume",
        user_text("#k:resume Go back to the survey."),
    )
    .await;
    quiesce(engine, root).await;

    let goal_set = engine
        .inner
        .store
        .log(root)
        .unwrap()
        .last_event()
        .unwrap()
        .seq;
    engine
        .set_session_goal(
            cookie_agent_protocol::SessionGoalSetParams {
                session_id: root,
                objective: "#k:goal Finish the survey checklist".into(),
                selection: None,
            },
            origin(),
        )
        .await
        .expect("set the goal");
    // The goal's reminder starts a run; the checklist is then finished
    // through the API, which completes the goal.
    await_event(engine, root, "the goal's first run", |event| {
        matches!(event.payload, EventPayload::RunFinished { .. }) && event.seq > goal_set
    })
    .await;
    engine
        .goal_update(
            root,
            cookie_agent_protocol::GoalUpdateParams {
                items: vec![cookie_agent_protocol::GoalItem {
                    description: "Survey the module".into(),
                    finished: true,
                }],
            },
        )
        .await
        .expect("finish the goal's checklist");
    quiesce(engine, root).await;

    run(
        engine,
        root,
        &selection,
        "count",
        user_text("#k:count Count the words of the notes."),
    )
    .await;
    quiesce(engine, root).await;

    let held = run(
        engine,
        root,
        &selection,
        "hold",
        user_text("#k:hold Read the library slowly."),
    )
    .await;
    engine
        .steer(
            held,
            user_text("#k:steered Check the tests as well."),
            origin(),
        )
        .await
        .expect("steer the held run");
    engine
        .submit_input(
            SessionSubmitInputParams {
                session_id: root,
                content: user_text("#k:queued Then summarize the session."),
                delivery: InputDelivery::Queue,
                selection: None,
                client_run_id: ClientRunId::new("queued").unwrap(),
                reset_fallback: false,
            },
            origin(),
        )
        .await
        .expect("queue input behind the run");
    quiesce(engine, root).await;
    engine.shutdown().await;
    keep_scenario(&sessions_of(fixture.root.path()), scenario).await;
}

// --- what a newer build adds ------------------------------------------------

/// Rewrites every log of a copy of `scenario` as a newer build of the same
/// history format could have written it: unknown members in envelopes,
/// payloads, and tool results, a display block of an unknown kind, and a
/// final record of an unknown type (`skippable` as given).
fn add_future_data(copy: &Path, skippable: bool) {
    for entry in walkdir(copy) {
        if entry
            .file_name()
            .is_none_or(|name| name != crate::session::EVENTS_FILE)
        {
            continue;
        }
        let mut lines = std::fs::read_to_string(&entry)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        for line in &mut lines {
            line["future_envelope"] = json!({"written_by": "a newer build"});
            line["payload"]["future_member"] = json!([1, "two"]);
            if let Some(result) = line
                .pointer_mut("/payload/result")
                .and_then(Value::as_object_mut)
                && let Some(Value::Array(display)) = result.get_mut("display")
            {
                display.push(json!({"type": "hologram", "frames": 3}));
                result.insert("future_result_member".into(), json!(true));
            }
        }
        let last = lines.last().unwrap().clone();
        let mut future = json!({
            "engine_version": "9.0.0",
            "origin": "engine:future",
            "session_id": last["session_id"],
            "run_id": null,
            "seq": last["seq"].as_u64().unwrap() + 1,
            "timestamp": last["timestamp"],
            "payload": {"type": "future_record", "detail": {"anything": ["at", "all"]}},
        });
        if skippable {
            future["skippable"] = json!(true);
        }
        lines.push(future);
        let text = lines
            .iter()
            .map(|line| line.to_string() + "\n")
            .collect::<String>();
        std::fs::write(&entry, text).unwrap();
    }
}

fn clear_skipped(tree: &mut Value) {
    if let Some(skipped) = tree["session"]["skipped_events"].as_array_mut() {
        skipped.clear();
    }
    for child in tree["children"].as_array_mut().into_iter().flatten() {
        clear_skipped(child);
    }
}

fn walkdir(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walkdir(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Golden trees as a newer build of their format could have written them
/// read as the recorded ones: unknown members are ignored, a display block of
/// an unknown kind is not sent to clients, and an unknown record that says it
/// is skippable is skipped. A fork copies every record as it was read,
/// unknown members included. An unknown record that is not skippable opens
/// the session render-only.
#[tokio::test]
async fn golden_history_tolerates_what_a_newer_build_adds() {
    for name in ["coding", "delegation"] {
        tolerates_future_data(name).await;
    }
}

async fn tolerates_future_data(name: &str) {
    let scenario = PathBuf::from(GOLDEN_ROOT)
        .join(HISTORY_FORMAT.to_string())
        .join(name);
    let mut recorded: Value = serde_json::from_slice(
        &std::fs::read(scenario.join(SNAPSHOT_FILE)).expect("recorded snapshot"),
    )
    .unwrap();
    expand_recorded_events(&scenario, &mut recorded);
    // The newer record is the logs' last.
    fn advance_tips(value: &mut Value) {
        match value {
            Value::Object(members) => {
                for (key, member) in members.iter_mut() {
                    match (key.as_str(), member.as_u64()) {
                        ("last_event_seq", Some(seq)) => *member = json!(seq + 1),
                        ("events", _) => {}
                        _ => advance_tips(member),
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(advance_tips),
            _ => {}
        }
    }
    advance_tips(&mut recorded);
    for skippable in [true, false] {
        let directory = private_tempdir();
        let copy = directory.path().join(name);
        copy_test_tree(&scenario, &copy);
        add_future_data(&copy, skippable);
        for opening in [Opening::ReadOnly, Opening::Owned] {
            let opened = open_scenario(&copy, opening).await;
            let mut read = snapshot(&opened.engine, &opened.roots).await;
            if !skippable {
                // The unknown record each log ends with is reported as
                // skipped, which opens the log render-only.
                for_each_session(&mut read, |session| {
                    let tip = session["meta"]["last_event_seq"].clone();
                    let skipped = session["meta"]["skipped_events"]
                        .as_array_mut()
                        .expect("skipped events");
                    assert_eq!(skipped.len(), 1, "{skipped:?}");
                    assert_eq!(skipped[0]["seq"], tip, "{skipped:?}");
                    skipped.clear();
                });
                for tree in read["trees"].as_array_mut().unwrap() {
                    clear_skipped(&mut tree["tree"]);
                }
            }
            let mut found = Vec::new();
            differences(&recorded["trees"], &read["trees"], "trees", &mut found);
            assert!(
                found.is_empty(),
                "{name} ({opening:?}), skippable {skippable}: {found:#?}"
            );
            let root = opened.roots[0];
            let render_only = opened
                .engine
                .inner
                .store
                .log(root)
                .unwrap()
                .is_render_only();
            assert_eq!(render_only, !skippable, "{opening:?}");
            if matches!(opening, Opening::Owned) && skippable {
                let through = opened
                    .engine
                    .inner
                    .store
                    .log(root)
                    .unwrap()
                    .last_event()
                    .unwrap()
                    .seq
                    + 1;
                let fork = opened
                    .engine
                    .fork_session(root, through, origin(), None)
                    .await
                    .expect("fork a tree with future data")
                    .session_id;
                let forked = std::fs::read_to_string(
                    crate::session::root_dir_in(&opened.sessions, fork)
                        .join(crate::session::EVENTS_FILE),
                )
                .unwrap();
                for kept in [
                    "future_envelope",
                    "future_member",
                    "hologram",
                    "future_record",
                ] {
                    assert!(forked.contains(kept), "the fork dropped {kept}");
                }
                // A record the fork leaves behind goes whole, unknown members
                // and all: its replacement is written as this build writes it.
                for line in forked.lines() {
                    let line: Value = serde_json::from_str(line).unwrap();
                    if line["payload"]["type"] == "record_omitted" {
                        assert!(line.get("future_envelope").is_none(), "{line}");
                        assert!(line["payload"].get("future_member").is_none(), "{line}");
                    }
                }
            }
            if !skippable {
                let selection = opened.engine.session_model_selection(root).unwrap();
                let refused = opened
                    .engine
                    .start_run(
                        RunStartParams {
                            reset_fallback: false,
                            session_id: root,
                            client_run_id: ClientRunId::new("after-the-future").unwrap(),
                            selection,
                            content: user_text("Go on."),
                        },
                        origin(),
                    )
                    .await
                    .expect_err("a render-only session runs nothing");
                assert!(
                    refused.to_string().contains("newer"),
                    "{opening:?}: {refused}"
                );
            }
            opened.engine.shutdown().await;
        }
    }
}

// --- forks of a session that delegated -------------------------------------

/// The engine of `opened` shut down, and another opened over the same
/// directory: holding the lock of every tree in `roots` for a read-only open,
/// adopting each for an owned one.
async fn reopen_scenario(
    opened: OpenedScenario,
    roots: Vec<SessionId>,
    opening: Opening,
) -> OpenedScenario {
    let OpenedScenario {
        engine,
        sessions,
        _locks,
        _directory: directory,
        ..
    } = opened;
    engine.shutdown().await;
    drop(engine);
    drop(_locks);
    let mut locks = Vec::new();
    if matches!(opening, Opening::ReadOnly) {
        for root in &roots {
            locks.push(
                crate::ownership::try_acquire(&crate::session::root_dir_in(&sessions, *root))
                    .expect("hold the tree's lock"),
            );
        }
    }
    let cwd = directory.path().join("workspace");
    let models = directory.path().join(format!("reopened-{opening:?}"));
    create_private_test_dir(&models);
    let engine = crate::Engine::open(crate::EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: cwd.clone(),
        config: reading_configuration(),
        model_manager: reading_model_manager(&models),
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(&cwd),
    })
    .expect("engine over the forked trees");
    if matches!(opening, Opening::Owned) {
        for root in &roots {
            engine.resume(*root).await.expect("adopt a tree");
        }
    }
    OpenedScenario {
        engine,
        roots,
        sessions,
        _locks: locks,
        _directory: directory,
    }
}

/// A session's events as clients receive them, and its model history.
async fn fork_view(engine: &Engine, id: SessionId) -> (Vec<Value>, Value) {
    let snapshot = session_snapshot(engine, id).await;
    (
        snapshot["events"].as_array().expect("events").clone(),
        snapshot["history"].clone(),
    )
}

/// A fork of the golden `delegation` root, which delegated in the
/// foreground and in the background, resumed a child, and received its
/// children's reports and messages as producer messages, leaves the
/// subagents behind: each of the source's delegation records becomes a
/// `record_omitted` at its sequence, everything else is copied as it was,
/// the model history is the source's, and the fork has no children. The
/// source's tree is untouched, and the fork reads the same after a restart,
/// read-only and owned.
#[tokio::test]
async fn a_fork_of_a_session_that_delegated_leaves_its_subagents_behind() {
    let scenario = PathBuf::from(GOLDEN_ROOT)
        .join(HISTORY_FORMAT.to_string())
        .join("delegation");
    for opening in [Opening::ReadOnly, Opening::Owned] {
        let opened = open_scenario(&scenario, opening).await;
        let root = opened.roots[0];
        let engine = &opened.engine;
        let source_dir = crate::session::root_dir_in(&opened.sessions, root);
        let source_bytes = tree_bytes(&source_dir);
        let source_children = engine.children(root).expect("source children");
        assert_eq!(source_children.len(), 2);
        let source_events = engine.inner.store.log(root).unwrap().event_snapshot();
        let tip = source_events.last().unwrap().seq;
        let delegations = engine.inner.delegation_events.entries().len();
        assert_eq!(delegations, 3);

        let fork = engine
            .fork_session(root, tip, origin(), None)
            .await
            .unwrap_or_else(|error| panic!("{opening:?}: fork a session that delegated: {error}"))
            .session_id;

        assert_eq!(tree_bytes(&source_dir), source_bytes, "{opening:?}");
        let fork_events = engine.inner.store.log(fork).unwrap().event_snapshot();
        let mut omitted = Vec::new();
        for (source, forked) in source_events.iter().zip(fork_events.iter()) {
            assert_eq!(forked.session_id, fork);
            assert_eq!(
                (forked.seq, forked.run_id, forked.timestamp, &forked.origin),
                (source.seq, source.run_id, source.timestamp, &source.origin)
            );
            let tag = cookie_agent_protocol::OpenTagged::tag(&source.payload);
            if tag.starts_with("delegation_") {
                assert_eq!(
                    forked.payload,
                    EventPayload::RecordOmitted {
                        omitted_type: cookie_agent_protocol::EventType::new(tag.clone()).unwrap()
                    }
                );
                omitted.push(tag);
            } else {
                assert_eq!(forked.payload, source.payload, "seq {}", source.seq);
            }
        }
        omitted.sort();
        omitted.dedup_by(|a, b| a == b);
        assert_eq!(
            omitted,
            [
                "delegation_finished",
                "delegation_progressed",
                "delegation_reserved"
            ]
        );
        assert_eq!(
            fork_events
                .iter()
                .filter(|event| matches!(event.payload, EventPayload::RecordOmitted { .. }))
                .count(),
            12
        );
        // The links to the children, and the producer messages that brought
        // their reports, are conversation and stay.
        for kept in [
            |payload: &EventPayload| matches!(payload, EventPayload::ToolCallLinked { .. }),
            |payload: &EventPayload| {
                matches!(
                    payload,
                    EventPayload::ProducerMessageAccepted {
                        producer_owner: cookie_agent_protocol::ProducerOwner::Delegation { .. },
                        ..
                    }
                )
            },
            |payload: &EventPayload| {
                matches!(
                    payload,
                    EventPayload::ProducerMessageAccepted {
                        producer_owner: cookie_agent_protocol::ProducerOwner::Subagent { .. },
                        ..
                    }
                )
            },
        ] {
            assert!(fork_events.iter().any(|event| kept(&event.payload)));
        }
        // The prefix ends between runs: the fork adds only its title.
        assert_eq!(fork_events.len(), source_events.len() + 1);
        assert!(matches!(
            fork_events.last().unwrap().payload,
            EventPayload::SessionTitleCommitted { .. }
        ));

        let (_, source_history) = fork_view(engine, root).await;
        let (_, fork_history) = fork_view(engine, fork).await;
        assert_eq!(fork_history, source_history, "{opening:?}");
        assert!(engine.children(fork).expect("fork children").is_empty());
        assert_eq!(engine.inner.delegation_events.entries().len(), delegations);
        assert!(
            engine
                .inner
                .delegation_events
                .entries()
                .iter()
                .all(|entry| entry.reservation.parent_session_id == root)
        );
        for child in &source_children {
            assert!(
                engine
                    .get_subagent_result(
                        fork,
                        child.session_id,
                        false,
                        page_window(1, 1),
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                    .is_err(),
                "the source's child is not the fork's"
            );
        }
        // On disk, each replacement is an audit record.
        let written = std::fs::read_to_string(
            crate::session::root_dir_in(&opened.sessions, fork).join(crate::session::EVENTS_FILE),
        )
        .unwrap();
        let mut replaced = 0;
        for line in written.lines() {
            let line: Value = serde_json::from_str(line).unwrap();
            if line["payload"]["type"] == "record_omitted" {
                assert_eq!(line["skippable"], true, "{line}");
                replaced += 1;
            }
            assert!(
                !line["payload"]["type"]
                    .as_str()
                    .unwrap()
                    .starts_with("delegation_"),
                "{line}"
            );
        }
        assert_eq!(replaced, 12);

        for reopening in [Opening::ReadOnly, Opening::Owned] {
            let opened = open_scenario(&scenario, opening).await;
            let fork = opened
                .engine
                .fork_session(root, tip, origin(), None)
                .await
                .expect("fork again")
                .session_id;
            let (forked_events, _) = fork_view(&opened.engine, fork).await;
            let reopened = reopen_scenario(opened, vec![root, fork], reopening).await;
            let (events, history) = fork_view(&reopened.engine, fork).await;
            assert_eq!(events, forked_events, "{opening:?} then {reopening:?}");
            assert_eq!(history, source_history, "{opening:?} then {reopening:?}");
            let meta = reopened.engine.get_session(fork).expect("reopened fork");
            assert!(meta.skipped_events.is_empty(), "{:?}", meta.skipped_events);
            assert!(
                reopened
                    .engine
                    .children(fork)
                    .expect("reopened fork children")
                    .is_empty()
            );
            assert_eq!(
                reopened
                    .engine
                    .children(root)
                    .expect("reopened source children")
                    .len(),
                2
            );
            assert!(
                reopened
                    .engine
                    .inner
                    .delegation_events
                    .entries()
                    .iter()
                    .all(|entry| entry.reservation.parent_session_id == root)
            );
            reopened.engine.shutdown().await;
        }
        opened.engine.shutdown().await;
    }
}
