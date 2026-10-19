//! The runtime corpora against this coding runtime: the daemon's messages of
//! each recorded conversation (`corpus/runtime/<version>/`) drive it, as
//! recorded and with a member no build knows added to every object of them,
//! and what a newer daemon may send (`corpus/future/runtime.json`) is read
//! as this build reads it. `cookie_agent_protocol::corpus` describes the
//! corpora and their rules.
//!
//! The replay ([`corpus::play`]) sends the daemon's recorded requests and
//! notifications in their recorded order, waiting before each for the
//! runtime's messages that preceded it, and answers the runtime's requests
//! with the daemon's recorded replies. Each daemon request must be answered
//! as it was. The recorded workspace, [`corpus::SCRATCH_PLACEHOLDER`]'s
//! `workspace`, is a fresh directory of the replay's.

use std::time::Duration;

use cookie_agent_protocol::{
    MessageFrame, Transport,
    corpus::{self, CorpusFile, Substitutions},
    in_process_pair,
};
use serde_json::{Value, json};

use super::{ServeOptions, serve};

/// A liveness bound, not a latency assertion.
const WAIT: Duration = Duration::from_secs(20);

/// Members of runtime messages whose contents are data, not types.
const OPAQUE: &[&str] = &[
    "arguments",
    "normalized_arguments",
    "metadata",
    "parameters",
    "value",
    "config",
];

/// A fresh workspace, and the substitution that puts it in place of the
/// recorded one.
fn workspace() -> (tempfile::TempDir, Substitutions) {
    let directory = tempfile::tempdir().expect("scratch directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let mut substitutions = Substitutions::default();
    substitutions.insert(
        format!("{}/workspace", corpus::SCRATCH_PLACEHOLDER),
        workspace.to_string_lossy(),
    );
    (directory, substitutions)
}

/// Plays the daemon's side of `file` to a coding runtime, with `transform`
/// applied to every message the daemon sends; returns the runtime's replies,
/// with the workspace written as recorded.
async fn replay(file: &CorpusFile, transform: impl Fn(Value) -> Value) -> Vec<(String, Value)> {
    let (directory, mut substitutions) = workspace();
    let scratch = directory.path().to_string_lossy().into_owned();
    let (daemon, runtime) = in_process_pair(64);
    let server = tokio::spawn(serve(Box::new(runtime), ServeOptions::default()));
    let played = corpus::play(file, "daemon", daemon, transform, &mut substitutions).await;
    tokio::time::timeout(WAIT, server)
        .await
        .expect("the runtime stops")
        .expect("the runtime task");
    let methods = file.request_methods();
    played
        .replies
        .into_iter()
        .map(|(id, reply)| {
            let method = methods
                .get(&("daemon".to_owned(), id))
                .cloned()
                .unwrap_or_default();
            let result = reply["result"]
                .to_string()
                .replace(&scratch, corpus::SCRATCH_PLACEHOLDER);
            (method, serde_json::from_str(&result).expect("JSON"))
        })
        .collect()
}

/// Each recorded runtime conversation drives this coding runtime: every
/// request the daemon sent is answered as it was, and a member no build
/// knows added at every level of the daemon's messages changes no answer.
#[tokio::test(flavor = "multi_thread")]
async fn recorded_daemon_messages_drive_the_coding_runtime() {
    let methods = corpus::methods("runtime");
    let versions = corpus::frozen_versions("runtime");
    assert!(!versions.is_empty(), "no frozen runtime corpus");
    for version in versions {
        for file in corpus::frozen("runtime", version) {
            for message in file
                .messages
                .iter()
                .filter(|message| message.from == "daemon")
            {
                if let Some(method) = message.method() {
                    let entry = methods.get(method).unwrap_or_else(|| {
                        panic!("{}: `{method}` is no runtime method", file.name)
                    });
                    assert_eq!(entry.direction, "from_daemon", "{}: `{method}`", file.name);
                }
            }
            let answered = replay(&file, |value| value).await;
            assert!(!answered.is_empty(), "{}: nothing was answered", file.name);
            let answered_from_the_future = replay(&file, |mut value| {
                corpus::add_future_members(&mut value, OPAQUE);
                value
            })
            .await;
            assert_eq!(
                answered_from_the_future, answered,
                "{}: unknown members change the runtime's answers",
                file.name
            );
        }
    }
}

/// What a newer daemon may send is read as this build reads it: each
/// request of `corpus/future/runtime.json` is answered as `answer` says
/// (`{"error": code}`, or `{"result": members}` with members the result
/// must hold), and a notification is taken without an answer.
#[tokio::test(flavor = "multi_thread")]
async fn future_daemon_messages_are_read_as_this_runtime_reads_them() {
    let fixture = corpus::future("runtime.json");
    let (_directory, substitutions) = workspace();
    let (mut daemon, runtime) = in_process_pair(64);
    let server = tokio::spawn(serve(Box::new(runtime), ServeOptions::default()));
    for entry in fixture["messages"].as_array().expect("messages") {
        let about = &entry["about"];
        let message = substitutions.apply(&entry["message"]);
        daemon
            .send(MessageFrame::Value(message.clone()))
            .await
            .expect("the runtime's connection");
        let Some(answer) = entry.get("answer") else {
            continue;
        };
        let reply = loop {
            let frame = tokio::time::timeout(WAIT, daemon.recv())
                .await
                .unwrap_or_else(|_| panic!("{about}: no answer"))
                .expect("the runtime's connection")
                .expect("the runtime is connected");
            let received: Value =
                serde_json::from_str(&frame.into_text().expect("text")).expect("JSON");
            match (received.get("id"), received["method"].as_str()) {
                (Some(id), Some("tool/progress")) => daemon
                    .send(MessageFrame::Value(
                        json!({"jsonrpc": "2.0", "id": id, "result": {"error": null}}),
                    ))
                    .await
                    .expect("the runtime's connection"),
                (_, Some(_)) => {}
                (Some(_), None) => break received,
                (None, None) => panic!("{about}: not a JSON-RPC message: {received}"),
            }
        };
        assert_eq!(reply["id"], message["id"], "{about}");
        if let Some(code) = answer.get("error") {
            assert_eq!(&reply["error"]["code"], code, "{about}: {reply}");
        } else {
            assert!(reply.get("result").is_some(), "{about}: {reply}");
            for (member, expected) in answer["result"].as_object().into_iter().flatten() {
                assert_eq!(&reply["result"][member], expected, "{about}: {reply}");
            }
        }
    }
    tokio::time::timeout(WAIT, server)
        .await
        .expect("the runtime stops after runtime/shutdown")
        .expect("the runtime task");
}
