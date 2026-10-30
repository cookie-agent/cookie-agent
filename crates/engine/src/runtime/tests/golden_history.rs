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

use super::support::*;
use crate::{Engine, EngineHistoryView, session::SessionStore};

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
    workdir: PathBuf,
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
    let workdir = data
        .join(crate::session::SESSIONS_ROOT_DIR)
        .join(SessionStore::workdir_key(&cwd));
    std::fs::create_dir_all(&workdir).expect("work dir");
    let roots = scenario_roots(scenario);
    let mut locks = Vec::new();
    for root in &roots {
        let target = workdir.join(root.to_string());
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
        workdir,
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

/// Removes from `read`'s history views the file labels a build adds before
/// each user file (`docs/site/specs/machines-on-demand.md` section 6.4):
/// model text the engine injects as it reads, which logs recorded before it
/// cannot hold. Everything else of the history must still read as recorded.
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
        let name = scenario.strip_prefix(GOLDEN_ROOT).unwrap_or(&scenario);
        for opening in [Opening::ReadOnly, Opening::Owned] {
            let opened = open_scenario(&scenario, opening).await;
            let mut read = snapshot(&opened.engine, &opened.roots).await;
            without_file_labels(&mut read);
            let mut found = Vec::new();
            differences(&recorded["trees"], &read["trees"], "trees", &mut found);
            if matches!(opening, Opening::ReadOnly) {
                for root in &opened.roots {
                    let recorded = tree_bytes(&scenario.join(root.to_string()));
                    if tree_bytes(&opened.workdir.join(root.to_string())) != recorded {
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

// --- recording ---------------------------------------------------------------

/// What the scripted model answers at one step of a turn.
#[derive(Clone, Debug)]
enum Reply {
    Text(String),
    Tools(Vec<(&'static str, Value)>),
    /// Answers after a pause, so the recorder can act while the request is
    /// in flight.
    Slow(u64, Box<Reply>),
}

fn text(text: &str) -> Reply {
    Reply::Text(text.into())
}

fn tool(name: &'static str, arguments: Value) -> Reply {
    Reply::Tools(vec![(name, arguments)])
}

type Rules = BTreeMap<&'static str, Vec<Reply>>;

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
fn reply_to(rules: &Rules, request: &Value) -> Reply {
    if request
        .get("tools")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return text("Summary: the conversation explored, edited, and fetched files.");
    }
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
async fn script_server(rules: Rules) -> String {
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

fn origin() -> EventOrigin {
    EventOrigin::new("client:golden").unwrap()
}

fn user_text(text: &str) -> UserContent {
    text.into()
}

async fn run(
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
async fn quiesce(engine: &Engine, root: SessionId) {
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
                },
                origin(),
            )
            .await
            .expect("answer a golden approval");
    }
}

/// The trees of the work dir `workdir`, copied to `scenario` with their
/// snapshot.
async fn keep_scenario(workdir: &Path, scenario: &Path) {
    std::fs::create_dir_all(scenario).expect("scenario directory");
    for root in scenario_roots(workdir) {
        copy_test_tree(
            &workdir.join(root.to_string()),
            &scenario.join(root.to_string()),
        );
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

fn workdir_of(fixture_root: &Path) -> PathBuf {
    fixture_root
        .join("data")
        .join(crate::session::SESSIONS_ROOT_DIR)
        .join(SessionStore::workdir_key(fixture_root))
}

#[tokio::test]
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
    keep_scenario(&workdir_of(fixture._directory.path()), scenario).await;
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
    keep_scenario(&workdir_of(fixture.root.path()), scenario).await;
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
    keep_scenario(&workdir_of(fixture._directory.path()), scenario).await;
}

// --- the coding fixture ------------------------------------------------------

const CODING_CAPABILITIES: &str = "input = [\"text\", \"image\", \"video\"]\noutput = [\"text\"]\ncontext_tokens = 200000\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = true\nnative_replay = \"unsupported\"\nmedia = { image = { mime_types = [\"image/png\"], max_bytes = 4194304, max_count = 4 }, video = { mime_types = [\"video/mp4\"], max_bytes = 4194304, max_count = 1 } }";

const CODING_AGENT: &str = "---\ndescription: Golden coding agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n  bash: allow\n  write: ask\n  webfetch: ask\n  skill: allow\n  delegate: allow\n  message: allow\n---\nWork in the repository.\n";

/// An engine whose local runtime is the coding runtime over the runtime
/// protocol, working in the fixture's directory, with `plugin` configured.
struct CodingFixture {
    engine: Engine,
    root: PanicResistantTempDir,
}

async fn coding_fixture(
    endpoint: &str,
    primary: &str,
    plugin: Option<(&str, cookie_agent_config::PluginConfig)>,
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
        None,
        "openai-compatible",
        Some(CODING_CAPABILITIES),
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
    keep_scenario(&workdir_of(workspace), scenario).await;
}

fn coding_rules() -> Rules {
    Rules::from([
        (
            "explore",
            vec![
                Reply::Tools(vec![
                    ("read", json!({"filePath": "src/lib.rs"})),
                    ("read", json!({"filePath": "src"})),
                    ("read", json!({"filePath": "assets/pixel.png"})),
                    ("read", json!({"filePath": "missing.txt"})),
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
                        "filePath": "src/lib.rs",
                        "oldString": "    left - right\n",
                        "newString": "    left.saturating_sub(right)\n",
                    }),
                ),
                tool(
                    "write",
                    json!({"filePath": "notes.md", "content": "# Notes\n\nSubtraction saturates now.\n"}),
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
                Reply::Slow(
                    1_500,
                    Box::new(tool("read", json!({"filePath": "src/lib.rs"}))),
                ),
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
    keep_scenario(&workdir_of(fixture.root.path()), scenario).await;
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
            without_file_labels(&mut read);
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
                    opened
                        .workdir
                        .join(fork.to_string())
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
        workdir,
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
                crate::ownership::try_acquire(&workdir.join(root.to_string()))
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
        workdir,
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
        let source_dir = opened.workdir.join(root.to_string());
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
            opened
                .workdir
                .join(fork.to_string())
                .join(crate::session::EVENTS_FILE),
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
