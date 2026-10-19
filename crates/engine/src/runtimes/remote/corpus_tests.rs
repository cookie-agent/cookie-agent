//! The plugin and runtime corpora against the daemon's end of those
//! protocols: what a plugin or runtime sent in each recorded conversation
//! (`corpus/plugin/<version>/`, `corpus/runtime/<version>/`) is read as the
//! daemon reads it, as recorded and with a member no build knows added to
//! every object of its information; and what a newer peer may send
//! (`corpus/future/daemon.json`) is read as this build reads it.
//! `cookie_agent_protocol::corpus` describes the corpora and their rules.
//!
//! The handshakes go through [`RuntimeClient::initialize_plugin`], and
//! [`RuntimeClient::initialize`] and [`RuntimeClient::open`], against a peer
//! that answers with the recorded replies. Every other message is read the
//! way [`RuntimeClient`] reads it: a command strictly
//! ([`cookie_agent_protocol::strict::decode_command`], after
//! `tool/progress` loses the display blocks of kinds the daemon does not
//! know), and information and replies tolerantly.

use std::{path::Path, sync::Arc, time::Duration};

use cookie_agent_protocol::{
    MessageFrame, Transport,
    corpus::{self, CorpusFile, MethodEntry},
    in_process_pair,
};
use cookie_agent_runtime_api::{
    RuntimeConnection,
    protocol::{self, RuntimeSettings},
};
use serde_json::Value;

use super::{ClientExtension, PluginHandshake, RuntimeClient};

/// Members of peer messages whose contents are data, not types.
const OPAQUE: &[&str] = &[
    "arguments",
    "normalized_arguments",
    "metadata",
    "parameters",
    "value",
    "config",
    "payload",
];

/// A message as the daemon read it: its JSON as this build writes it back.
type Read = Result<Value, String>;

fn information<T: serde::de::DeserializeOwned + serde::Serialize>(value: Value) -> Read {
    let decoded = serde_json::from_value::<T>(value).map_err(|error| error.to_string())?;
    serde_json::to_value(&decoded).map_err(|error| error.to_string())
}

fn command<T>(value: Value) -> Read
where
    T: serde::de::DeserializeOwned + serde::Serialize + schemars::JsonSchema + 'static,
{
    let decoded = cookie_agent_protocol::strict::decode_command::<T>(value)
        .map_err(|error| error.to_string())?;
    serde_json::to_value(&decoded).map_err(|error| error.to_string())
}

macro_rules! readers {
    (information: $($info:literal => $info_type:ty,)* commands: $($command:literal => $command_type:ty,)*) => {
        /// `value` read as the type `root` names, as the daemon reads it.
        fn read(root: &str, value: Value) -> Read {
            match root {
                $($info => information::<$info_type>(value),)*
                $($command => command::<$command_type>(value),)*
                other => Err(format!("the daemon reads no `{other}`")),
            }
        }
    };
}

readers! {
    information:
    "ExtensionInitializeResult" => cookie_agent_protocol::ExtensionInitializeResult,
    "InitializeResult" => protocol::InitializeResult,
    "CapabilityOpenResult" => protocol::CapabilityOpenResult,
    "ContextResult" => protocol::ContextResult,
    "Empty" => protocol::Empty,
    "Outcome" => protocol::Outcome,
    "PrepareResult" => protocol::PrepareResult,
    "ExecuteResult" => protocol::ExecuteResult,
    "RuntimeTools" => protocol::RuntimeTools,
    "CapabilityToolsChangedParams" => protocol::CapabilityToolsChangedParams,
    "McpChangedParams" => protocol::McpChangedParams,
    "ExtensionToolBeforeCallResult" => cookie_agent_protocol::ExtensionToolBeforeCallResult,
    "ExtensionToolAfterResultResult" => cookie_agent_protocol::ExtensionToolAfterResultResult,
    "ExtensionAgentBeforeStartResult" => cookie_agent_protocol::ExtensionAgentBeforeStartResult,
    "ExtensionSessionBeforeCompactResult" => cookie_agent_protocol::ExtensionSessionBeforeCompactResult,
    "ExtensionUserBeforeInputResult" => cookie_agent_protocol::ExtensionUserBeforeInputResult,
    "ExtensionModelBeforeRequestResult" => cookie_agent_protocol::ExtensionModelBeforeRequestResult,
    "ExtensionMessageEndResult" => cookie_agent_protocol::ExtensionMessageEndResult,
    "ExtensionAllowBlockResult" => cookie_agent_protocol::ExtensionAllowBlockResult,
    "ExtensionSessionBeforeRevertResult" => cookie_agent_protocol::ExtensionSessionBeforeRevertResult,
    commands:
    "ToolProgressParams" => protocol::ToolProgressParams,
    "DetachableParams" => protocol::DetachableParams,
    "ProducerRegisterParams" => cookie_agent_protocol::ProducerRegisterParams,
    "ProducerSendParams" => cookie_agent_protocol::ProducerSendParams,
    "ProducerUnregisterParams" => cookie_agent_protocol::ProducerUnregisterParams,
    "ProducerDiscardParams" => cookie_agent_protocol::ProducerDiscardParams,
    "RecoveryCompleteParams" => cookie_agent_protocol::RecoveryCompleteParams,
    "ExtensionEmitParams" => cookie_agent_protocol::ExtensionEmitParams,
}

/// The first place where `read` and `expected` differ.
fn difference(read: &Value, expected: &Value, path: &str) -> Option<String> {
    match (read, expected) {
        (Value::Object(left), Value::Object(right)) => {
            left.keys()
                .chain(right.keys())
                .find_map(|key| match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => difference(left, right, &format!("{path}.{key}")),
                    (left, right) => Some(format!("{path}.{key}: {left:?} instead of {right:?}")),
                })
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => left
            .iter()
            .zip(right)
            .enumerate()
            .find_map(|(index, (left, right))| {
                difference(left, right, &format!("{path}[{index}]"))
            }),
        (left, right) if left != right => Some(format!("{path}: {left} instead of {right}")),
        _ => None,
    }
}

/// Reads `value` as the daemon reads `root`. A command's params are read
/// as sent; information, when `add_members`, is also read with a member no
/// build knows added at every level, which must read to the same value.
/// Returns the display block kinds a `tool/progress` replacement held that
/// the daemon drops.
fn assert_read(
    root: &str,
    role: &str,
    mut value: Value,
    what: &str,
    add_members: bool,
) -> Vec<String> {
    let unknown = if root == "ToolProgressParams" {
        protocol::take_unknown_progress_blocks(&mut value)
    } else {
        Vec::new()
    };
    let read_as_sent = read(root, value.clone()).unwrap_or_else(|error| panic!("{what}: {error}"));
    if add_members && role != "command" {
        let mut from_the_future = value;
        corpus::add_future_members(&mut from_the_future, OPAQUE);
        let read_from_the_future = read(root, from_the_future)
            .unwrap_or_else(|error| panic!("{what} with future members: {error}"));
        if let Some(difference) = difference(&read_from_the_future, &read_as_sent, "$") {
            panic!("{what}: unknown members change what the daemon reads: {difference}");
        }
    }
    unknown
}

/// Reads every message the peer sent in `file` but its handshake replies.
fn assert_peer_messages_read(
    file: &CorpusFile,
    peer: &str,
    methods: &std::collections::BTreeMap<String, MethodEntry>,
) {
    let requests = file.request_methods();
    for message in file.messages.iter().filter(|message| message.from == peer) {
        let what = format!("{}: {}", file.name, message.message);
        if message.is_reply() {
            let method = CorpusFile::replied_method(&requests, "daemon", message)
                .unwrap_or_else(|| panic!("{what}: a reply to no request"));
            let Some(result) = message.message.get("result") else {
                continue;
            };
            let root = methods[method].result.as_deref().expect("a result");
            assert_read(root, "information", result.clone(), &what, true);
        } else {
            let method = message.method().unwrap_or_default();
            let entry = methods
                .get(method)
                .unwrap_or_else(|| panic!("{what}: `{method}` is no method of this surface"));
            assert_eq!(entry.direction, "to_daemon", "{what}");
            let root = entry.params.as_deref().expect("params");
            let unknown = assert_read(
                root,
                entry.params_role.as_deref().unwrap_or("information"),
                message.message["params"].clone(),
                &what,
                true,
            );
            assert!(unknown.is_empty(), "{what}: blocks of kinds {unknown:?}");
        }
    }
}

/// The recorded reply `from` sent to the request of `method`.
fn recorded_result(file: &CorpusFile, from: &str, method: &str) -> Value {
    let requests = file.request_methods();
    file.messages
        .iter()
        .filter(|message| message.from == from && message.is_reply())
        .find(|message| CorpusFile::replied_method(&requests, "daemon", message) == Some(method))
        .and_then(|message| message.message.get("result").cloned())
        .unwrap_or_else(|| panic!("{}: no `{method}` result", file.name))
}

/// The params of the daemon's recorded request of `method`.
fn recorded_params(file: &CorpusFile, method: &str) -> Value {
    file.messages
        .iter()
        .find(|message| message.from == "daemon" && message.method() == Some(method))
        .map(|message| message.message["params"].clone())
        .unwrap_or_else(|| panic!("{}: no `{method}` request", file.name))
}

/// A peer that answers the requests it receives with `results`, in order.
async fn answering(results: Vec<Value>) -> (RuntimeConnection, tokio::task::JoinHandle<()>) {
    let (daemon, mut peer) = in_process_pair(8);
    let task = tokio::spawn(async move {
        let mut results = results.into_iter();
        while let Ok(Some(frame)) = peer.recv().await {
            let request: Value =
                serde_json::from_str(&frame.into_text().expect("text")).expect("JSON");
            if request.get("id").is_none() {
                continue;
            }
            let Some(result) = results.next() else {
                return;
            };
            let reply =
                serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
            if peer.send(MessageFrame::Value(reply)).await.is_err() {
                return;
            }
        }
    });
    (RuntimeConnection::new(Box::new(daemon)), task)
}

/// Records what the daemon reports about a plugin.
#[derive(Default)]
struct Reported(std::sync::Mutex<Vec<String>>);

impl ClientExtension for Reported {
    fn notification(
        &self,
        _client: RuntimeClient,
        notification: cookie_agent_runtime_api::rpc::IncomingNotification,
    ) {
        self.0
            .lock()
            .expect("reported")
            .push(format!("notification {}", notification.method));
    }

    fn diagnostic(&self, message: String) {
        self.0.lock().expect("reported").push(message);
    }
}

/// The plugin handshake against a plugin that answers `plugin/initialize`
/// with `result`.
async fn plugin_handshake(result: Value) -> PluginHandshake {
    let name = result["name"].as_str().expect("a name").to_owned();
    let (connection, peer) = answering(vec![result]).await;
    let handshake = RuntimeClient::initialize_plugin(
        connection,
        &name,
        true,
        Duration::from_secs(2),
        Duration::from_secs(1),
        Arc::new(Reported::default()),
    )
    .await
    .unwrap_or_else(|error| panic!("the plugin handshake: {error}"));
    peer.abort();
    handshake
}

/// The runtime handshake and the opening of `capability` against a runtime
/// that answers them with `initialized` and `opened`.
async fn runtime_handshake(
    settings: &RuntimeSettings,
    initialized: Value,
    capability: &str,
    workspace: Option<&Path>,
    opened: Value,
) -> crate::StartedRuntime {
    let (connection, peer) = answering(vec![initialized, opened]).await;
    let (client, _) =
        RuntimeClient::initialize(connection, "local", settings, Duration::from_secs(1), None)
            .await
            .unwrap_or_else(|error| panic!("the runtime handshake: {error}"));
    let started = client
        .open(
            &cookie_agent_protocol::CapabilityName::new(capability).expect("capability"),
            workspace,
        )
        .await
        .unwrap_or_else(|error| panic!("capability/open: {error}"));
    peer.abort();
    started
}

/// Each recorded plugin conversation reads as the daemon reads it: its
/// handshake connects with nothing noted, and every other message it sent
/// reads, with or without members no build knows.
#[tokio::test]
async fn recorded_plugin_messages_read_as_the_daemon_reads_them() {
    let methods = corpus::methods("plugin");
    let versions = corpus::frozen_versions("plugin");
    assert!(!versions.is_empty(), "no frozen plugin corpus");
    for version in versions {
        for file in corpus::frozen("plugin", version) {
            let result = recorded_result(
                &file,
                "plugin",
                cookie_agent_protocol::PLUGIN_INITIALIZE_METHOD,
            );
            let handshake = plugin_handshake(result.clone()).await;
            assert!(
                handshake.notes.is_empty(),
                "{}: {:?}",
                file.name,
                handshake.notes
            );
            let mut from_the_future = result;
            corpus::add_future_members(&mut from_the_future, OPAQUE);
            let handshake = plugin_handshake(from_the_future).await;
            assert!(
                handshake.notes.is_empty(),
                "{}: {:?}",
                file.name,
                handshake.notes
            );
            assert_peer_messages_read(&file, "plugin", &methods);
        }
    }
}

/// Each recorded runtime conversation reads as the daemon reads it: its
/// handshake and the opening of its capability connect, and every other
/// message it sent reads, with or without members no build knows.
#[tokio::test]
async fn recorded_runtime_messages_read_as_the_daemon_reads_them() {
    let methods = corpus::methods("runtime");
    let versions = corpus::frozen_versions("runtime");
    assert!(!versions.is_empty(), "no frozen runtime corpus");
    for version in versions {
        for file in corpus::frozen("runtime", version) {
            let settings: RuntimeSettings = serde_json::from_value(
                recorded_params(&file, protocol::method::INITIALIZE)["settings"].clone(),
            )
            .expect("settings");
            let open = recorded_params(&file, protocol::method::CAPABILITY_OPEN);
            let capability = open["capability"].as_str().expect("capability");
            let workspace = open["workspace"].as_str().map(Path::new);
            let initialized = recorded_result(&file, "runtime", protocol::method::INITIALIZE);
            let opened = recorded_result(&file, "runtime", protocol::method::CAPABILITY_OPEN);
            for (initialized, opened) in [
                (initialized.clone(), opened.clone()),
                (
                    {
                        let mut value = initialized;
                        corpus::add_future_members(&mut value, OPAQUE);
                        value
                    },
                    {
                        let mut value = opened;
                        corpus::add_future_members(&mut value, OPAQUE);
                        value
                    },
                ),
            ] {
                let started =
                    runtime_handshake(&settings, initialized, capability, workspace, opened).await;
                assert!(!started.providers.is_empty(), "{}: no tools", file.name);
            }
            assert_peer_messages_read(&file, "runtime", &methods);
        }
    }
}

/// What a newer plugin or runtime may send is read as this build reads it
/// (`corpus/future/daemon.json`): its handshake connects, noting only what
/// `notes` lists, and every other message reads, a `tool/progress`
/// replacement losing exactly the blocks `drops` lists.
#[tokio::test]
async fn future_peer_messages_read_as_this_daemon_reads_them() {
    let fixture = corpus::future("daemon.json");

    let plugin = &fixture["plugin"];
    let handshake = plugin_handshake(plugin["initialize"].clone()).await;
    let notes = plugin["notes"].as_array().expect("notes");
    assert_eq!(handshake.notes.len(), notes.len(), "{:?}", handshake.notes);
    for (note, expected) in handshake.notes.iter().zip(notes) {
        assert!(
            note.contains(expected.as_str().expect("a note")),
            "{note} does not name {expected}"
        );
    }
    let tools = handshake.result.tools.tools;
    assert!(
        tools
            .iter()
            .all(|tool| tool.collision == protocol::ToolCollision::Prefix),
        "an unknown collision mode behaves like `prefix`"
    );

    let runtime = &fixture["runtime"];
    let settings: RuntimeSettings =
        serde_json::from_value(runtime["settings"].clone()).expect("settings");
    let started = runtime_handshake(
        &settings,
        runtime["initialize"].clone(),
        runtime["capability"].as_str().expect("capability"),
        runtime["workspace"].as_str().map(Path::new),
        runtime["open"].clone(),
    )
    .await;
    assert!(!started.providers.is_empty(), "the runtime's tools");

    for surface in ["plugin", "runtime"] {
        let methods = corpus::methods(surface);
        for entry in fixture[surface]["messages"].as_array().expect("messages") {
            let method = entry["method"].as_str().expect("method");
            let method_entry = &methods[method];
            let what = format!("{surface}: {}", entry["about"]);
            let (root, role, value) = match entry.get("result") {
                Some(result) => (
                    method_entry.result.as_deref().expect("a result"),
                    "information",
                    result.clone(),
                ),
                None => (
                    method_entry.params.as_deref().expect("params"),
                    method_entry.params_role.as_deref().unwrap_or("information"),
                    entry["params"].clone(),
                ),
            };
            // Blocks of unknown kinds keep what they hold, so members
            // added to them would read back: these are read as they are.
            let dropped = assert_read(root, role, value, &what, false);
            let expected = entry
                .get("drops")
                .and_then(Value::as_array)
                .map(|kinds| {
                    kinds
                        .iter()
                        .map(|kind| kind.as_str().expect("a kind").to_owned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assert_eq!(dropped, expected, "{what}");
        }
    }
}
