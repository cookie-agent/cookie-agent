//! The client corpora against this build's client: the replies a daemon
//! sent decode as their methods' results, and its notifications reach a
//! [`Client`] without a diagnostic, as recorded (`corpus/client/<version>/`),
//! with a member no build knows added to every object of them, and as a
//! newer daemon may send them (`corpus/future/client.json`). The module
//! [`crate::corpus`] describes the corpora and their rules.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    Client, ClientDelivery, ClientEvent, MessageFrame, SessionId, Transport, TransportError,
    corpus::{self, CorpusFile},
};

/// Members of client messages whose contents are data, not types: tool
/// arguments, inputs, and metadata, frozen model bindings, JSON values,
/// schemas, and maps with typed keys.
const OPAQUE: &[&str] = &[
    "arguments",
    "normalized_arguments",
    "metadata",
    "parameters",
    "value",
    "by_model",
    "setup_values",
    "provider_options",
    "provider_metadata",
    "response_metadata",
    "raw_input",
    "frozen",
    "input",
];

/// A message as this client read it: its `Debug` form, and its JSON as
/// this build writes it back.
struct Read {
    debug: String,
    json: Value,
}

fn read<T: serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug>(
    value: Value,
) -> Result<Read, String> {
    let decoded = serde_json::from_value::<T>(value).map_err(|error| error.to_string())?;
    Ok(Read {
        debug: format!("{decoded:?}"),
        json: serde_json::to_value(&decoded).map_err(|error| error.to_string())?,
    })
}

macro_rules! decoders {
    ($($root:ident),* $(,)?) => {
        /// `value` read as the type `root` names, as this client reads it.
        fn decode(root: &str, value: Value) -> Result<Read, String> {
            match root {
                // A client reads a page's events one by one, open.
                "EventsSubscribeResult" => {
                    let mut page = read::<Vec<ClientEvent>>(value["events"].clone())?;
                    page.json = json!({"events": page.json, "has_more": value["has_more"],
                        "stale_cursor": value["stale_cursor"]});
                    Ok(page)
                }
                "EventSubscriptionMessage" => read::<crate::Open<crate::ClientSubscriptionMessage>>(value),
                "SessionsChangedNotification" => read::<crate::Open<crate::SessionsChangedNotification>>(value),
                $(stringify!($root) => read::<crate::$root>(value),)*
                other => Err(format!("no decoder for `{other}`")),
            }
        }
    };
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

decoders!(
    ApprovalListResult,
    ApprovalRespondResult,
    ArtifactReadResult,
    ArtifactUploadBeginResult,
    ArtifactUploadCancelResult,
    ArtifactUploadChunkResult,
    ArtifactUploadFinishResult,
    ConfigApplyResult,
    ConfigGetResult,
    ConfigReloadResult,
    EventsUnsubscribeResult,
    McpAuthBeginResult,
    McpAuthCancelResult,
    McpAuthCompleteResult,
    McpServerListResult,
    McpServerMutationResult,
    MachinesListResult,
    MemoryDeleteResult,
    MemoryListResult,
    MemoryReadResult,
    MemoryWriteResult,
    PluginsListResult,
    ProjectDeleteResult,
    ProjectGetResult,
    ProjectResult,
    ProjectsListResult,
    PluginsRestartResult,
    ProviderAuthBeginResult,
    ProviderAuthCancelResult,
    ProviderAuthCompleteResult,
    ProviderAuthCompletedNotification,
    ProviderConnectResult,
    ProviderDisconnectResult,
    RunCancelResult,
    RunStartResult,
    RunSteerResult,
    RunToolBackgroundResult,
    RuntimeSnapshotResult,
    RuntimesListResult,
    RuntimesReconnectResult,
    ServerHello,
    SessionBackgroundListResult,
    SessionBackgroundStopResult,
    SessionChildrenResult,
    SessionCompactCancelResult,
    SessionCompactResult,
    SessionCreateResult,
    SessionForkResult,
    SessionGetResult,
    SessionGoalGetResult,
    SessionGoalLifecycleResult,
    SessionGoalSetResult,
    SessionListResult,
    SessionPermissionGetResult,
    SessionPermissionMutationResult,
    SessionPermissionPreviewResult,
    SessionProducersResult,
    SessionProjectSetResult,
    SessionRecallInputResult,
    SessionRenameResult,
    SessionResumeResult,
    SessionRevertResult,
    SessionRuntimeAttachResult,
    SessionRuntimeDetachResult,
    SessionSelectModelResult,
    SessionSetPermissionModeResult,
    SessionSubmitInputResult,
    SessionTreeResult,
    SessionTreeUsageResult,
    SessionUsageResult,
    SkillsGetResult,
    SkillsListResult,
    BackgroundChangedNotification,
    ExtensionBusEventParams,
    MachinesChangedNotification,
    McpChangedNotification,
    MemoryChangedNotification,
    PluginsChangedNotification,
    ProjectsChangedNotification,
    RuntimeChangedNotification,
    RuntimesChangedNotification,
);

/// Every recorded client corpus.
fn client_corpora() -> Vec<CorpusFile> {
    let versions = corpus::frozen_versions("client");
    assert!(!versions.is_empty(), "no frozen client corpus");
    versions
        .into_iter()
        .flat_map(|version| corpus::frozen("client", version))
        .collect()
}

/// The result of each reply of `file`, with the method of its request.
fn replies(file: &CorpusFile) -> Vec<(String, Value)> {
    let methods = file.request_methods();
    file.messages
        .iter()
        .filter(|message| message.from == "daemon" && message.is_reply())
        .filter_map(|reply| {
            let method = CorpusFile::replied_method(&methods, "client", reply)
                .unwrap_or_else(|| panic!("{}: a reply to no request: {reply:?}", file.name));
            reply
                .message
                .get("result")
                .map(|result| (method.to_owned(), result.clone()))
        })
        .collect()
}

fn future(mut value: Value) -> Value {
    corpus::add_future_members(&mut value, OPAQUE);
    value
}

/// Every corpus directory is pinned, and every pinned one is unchanged.
#[test]
fn every_recorded_corpus_is_frozen() {
    let root = std::path::Path::new(corpus::CORPUS_DIRECTORY);
    let mut found = Vec::new();
    for surface in ["client", "plugin", "runtime"] {
        for entry in std::fs::read_dir(root.join(surface)).expect("surface directory") {
            let version = entry
                .expect("version directory")
                .file_name()
                .to_string_lossy()
                .into_owned();
            found.push((surface.to_owned(), version));
        }
    }
    found.sort();
    let mut pinned = corpus::FROZEN
        .iter()
        .map(|(surface, version, _)| ((*surface).to_owned(), (*version).to_owned()))
        .collect::<Vec<_>>();
    pinned.sort();
    assert_eq!(
        found, pinned,
        "every recorded corpus directory is pinned in corpus::FROZEN"
    );
    for (surface, version) in &found {
        let files = corpus::frozen(surface, version);
        assert!(
            files.iter().all(|file| !file.messages.is_empty()),
            "corpus/{surface}/{version} holds a conversation"
        );
    }
}

/// Reads `value` as `root`, as recorded and with a member no build knows
/// added at every level: both must read, the recorded one with nothing this
/// client does not know, and to the same value.
fn assert_reads(root: &str, value: Value, what: &str) {
    let recorded = decode(root, value.clone()).unwrap_or_else(|error| panic!("{what}: {error}"));
    assert!(
        !recorded.debug.contains("Unknown"),
        "{what} holds something this client does not know: {}",
        recorded.debug
    );
    let from_the_future = decode(root, future(value))
        .unwrap_or_else(|error| panic!("{what} with future members: {error}"));
    if let Some(difference) = difference(&from_the_future.json, &recorded.json, "$") {
        panic!("{what}: unknown members change what the client reads: {difference}");
    }
}

/// Each recorded reply reads as its method's result, and each recorded
/// notification as its params, with or without members no build knows.
#[test]
fn recorded_client_replies_and_notifications_read_as_their_types() {
    let methods = corpus::methods("client");
    for file in client_corpora() {
        let replies = replies(&file);
        assert!(!replies.is_empty(), "{}: no replies", file.name);
        for (method, result) in replies {
            let root = methods[&method]
                .result
                .as_deref()
                .unwrap_or_else(|| panic!("`{method}` has no result"));
            assert_reads(
                root,
                result,
                &format!("{}: the `{method}` result", file.name),
            );
        }
        for notification in file
            .messages
            .iter()
            .filter(|message| message.from == "daemon" && message.is_notification())
        {
            let method = notification.method().unwrap_or_default();
            let root = methods
                .get(method)
                .and_then(|entry| entry.params.as_deref())
                .unwrap_or_else(|| panic!("{}: `{method}` is no client notification", file.name));
            assert_reads(
                root,
                notification.message["params"].clone(),
                &format!("{}: a `{method}` notification", file.name),
            );
        }
    }
}

/// The daemon's end of a scripted connection.
struct Scripted {
    incoming: mpsc::UnboundedReceiver<MessageFrame>,
    sent: mpsc::UnboundedSender<Value>,
}

#[async_trait]
impl Transport for Scripted {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        let value = match frame {
            MessageFrame::Value(value) => value,
            other => serde_json::from_str(&other.into_text()?)?,
        };
        self.sent.send(value).map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        Ok(self.incoming.recv().await)
    }
}

/// What a client delivers when a daemon answers its handshake and its
/// subscription to `session` with `answers` (results by method), then sends
/// `notifications`.
async fn deliver(
    session: SessionId,
    answers: BTreeMap<String, Value>,
    notifications: Vec<Value>,
) -> Vec<ClientDelivery> {
    let (daemon, incoming) = mpsc::unbounded_channel();
    let (sent, mut requests) = mpsc::unbounded_channel::<Value>();
    let client = Client::connect_stream(Scripted { incoming, sent });
    let mut deliveries = client.subscribe_deliveries().expect("deliveries");
    let answering = daemon.clone();
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let (Some(id), Some(method)) = (request.get("id"), request["method"].as_str()) else {
                continue;
            };
            let reply = match answers.get(method) {
                Some(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                None if method == "corpus.flush" => {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                }
                None => json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "not in the corpus"}}),
            };
            if answering.send(MessageFrame::Value(reply)).is_err() {
                return;
            }
        }
    });
    client.handshake().await.expect("handshake");
    client
        .subscribe_events(session, None)
        .await
        .expect("events.subscribe");
    for notification in notifications {
        daemon
            .send(MessageFrame::Value(notification))
            .expect("the connection");
    }
    // The connection reads in order: once this is answered, every
    // notification before it was routed.
    let _: Value = client
        .call("corpus.flush", &json!({}))
        .await
        .expect("flush");
    let mut delivered = Vec::new();
    while let Ok(delivery) = deliveries.try_recv() {
        delivered.push(delivery);
    }
    client.shutdown();
    delivered
}

/// A delivery that says the client could not read something.
fn unreadable(delivery: &ClientDelivery) -> bool {
    matches!(
        delivery,
        ClientDelivery::Diagnostic { .. }
            | ClientDelivery::UnknownEvent { .. }
            | ClientDelivery::RecoveryFailed { .. }
            | ClientDelivery::Disconnected { .. }
    )
}

/// The session `file` subscribed to, and the answers and notifications to
/// replay it to a client.
fn subscription(file: &CorpusFile) -> (SessionId, BTreeMap<String, Value>, Vec<Value>) {
    let subscribe = file
        .messages
        .iter()
        .find(|message| message.from == "client" && message.method() == Some("events.subscribe"))
        .unwrap_or_else(|| panic!("{}: no subscription", file.name));
    let session = serde_json::from_value(subscribe.message["params"]["session_id"].clone())
        .expect("session ID");
    let answers = replies(file)
        .into_iter()
        .filter(|(method, _)| method == "handshake" || method == "events.subscribe")
        .collect();
    let notifications = file
        .messages
        .iter()
        .filter(|message| message.from == "daemon" && message.is_notification())
        .map(|message| message.message.clone())
        .collect();
    (session, answers, notifications)
}

/// Each recorded notification reaches a client without a diagnostic or an
/// unknown event, and with a member no build knows added at every level it
/// reaches it as the same delivery.
#[tokio::test]
async fn recorded_client_notifications_reach_the_client_as_they_were_sent() {
    for file in client_corpora() {
        let (session, answers, notifications) = subscription(&file);
        let live = notifications
            .iter()
            .filter(|notification| notification["method"] == "events.subscription")
            .count();
        let delivered = deliver(session, answers.clone(), notifications.clone()).await;
        let unread = delivered
            .iter()
            .filter(|d| unreadable(d))
            .collect::<Vec<_>>();
        assert!(unread.is_empty(), "{}: {unread:?}", file.name);
        let delivered_live = delivered
            .iter()
            .filter(|delivery| matches!(delivery, ClientDelivery::Live { .. }))
            .count();
        assert_eq!(
            delivered_live, live,
            "{}: every subscription message is delivered",
            file.name
        );

        let delivered_from_the_future = deliver(
            session,
            answers
                .into_iter()
                .map(|(method, result)| (method, future(result)))
                .collect(),
            notifications
                .into_iter()
                .map(|mut notification| {
                    notification["params"] = future(notification["params"].take());
                    notification
                })
                .collect(),
        )
        .await;
        assert_eq!(
            format!("{delivered_from_the_future:?}"),
            format!("{delivered:?}"),
            "{}: unknown members change what the client delivers",
            file.name
        );
    }
}

/// What a delivery is, in `corpus/future/client.json`'s words; `None` for
/// the subscription's own replay.
fn delivery_kind(delivery: &ClientDelivery) -> Option<&'static str> {
    Some(match delivery {
        ClientDelivery::ReplayStart { .. }
        | ClientDelivery::ReplayEvent { .. }
        | ClientDelivery::ReplayEnd { .. } => return None,
        ClientDelivery::Live { .. } => "live",
        ClientDelivery::UnknownEvent { .. } => "unknown_event",
        ClientDelivery::SessionsChanged(changed) => match changed.as_ref() {
            crate::SessionsChangedNotification::Resync => "sessions_resync",
            _ => "sessions_changed",
        },
        ClientDelivery::RuntimeChanged(_) => "runtime_changed",
        ClientDelivery::PluginEvent(_) => "plugin_event",
        ClientDelivery::McpChanged(_) => "mcp_changed",
        ClientDelivery::RuntimesChanged(_) => "runtimes_changed",
        ClientDelivery::BackgroundChanged(_) => "background_changed",
        ClientDelivery::PluginsChanged(_) => "plugins_changed",
        ClientDelivery::MachinesChanged(_) => "machines_changed",
        ClientDelivery::MemoryChanged(_) => "memory_changed",
        ClientDelivery::ProjectsChanged(_) => "projects_changed",
        ClientDelivery::CronsChanged(_) => "crons_changed",
        ClientDelivery::NotificationsChanged(_) => "notifications_changed",
        ClientDelivery::DaemonRestarting(_) => "daemon_restarting",
        ClientDelivery::OnboardingChanged(_) => "onboarding_changed",
        ClientDelivery::Diagnostic { .. } => "diagnostic",
        ClientDelivery::RecoveryFailed { .. } => "recovery_failed",
        ClientDelivery::Disconnected { .. } => "disconnected",
    })
}

/// The frozen corpus file `surface/version/file` names.
fn corpus_file(name: &str) -> CorpusFile {
    let [surface, version, file] = name.split('/').collect::<Vec<_>>()[..] else {
        panic!("`{name}` is not <surface>/<version>/<file>");
    };
    corpus::frozen(surface, version)
        .into_iter()
        .find(|corpus| corpus.name == file)
        .unwrap_or_else(|| panic!("no corpus file {name}"))
}

/// What a newer daemon may send reads as this build's fallbacks: each
/// result as its method's result, with what `reads` names in it.
#[test]
fn future_client_results_read_as_their_fallbacks() {
    let fixture = corpus::future("client.json");
    let methods = corpus::methods("client");
    for entry in fixture["results"].as_array().expect("results") {
        let method = entry["method"].as_str().expect("method");
        let root = methods[method].result.as_deref().expect("a result");
        let read = decode(root, entry["result"].clone())
            .unwrap_or_else(|error| panic!("the future `{method}` result: {error}"));
        for expected in entry["reads"].as_array().expect("reads") {
            let expected = expected.as_str().expect("a string");
            assert!(
                read.debug.contains(expected),
                "the future `{method}` result reads without `{expected}`: {}",
                read.debug
            );
        }
    }
}

/// What a newer daemon may send reaches a client as this build reads it:
/// each notification produces exactly its `expect` deliveries, and never a
/// diagnostic.
#[tokio::test]
async fn future_client_notifications_are_delivered_as_this_build_reads_them() {
    let fixture = corpus::future("client.json");
    let (session, answers, _) =
        subscription(&corpus_file(fixture["corpus"].as_str().expect("corpus")));
    for entry in fixture["notifications"].as_array().expect("notifications") {
        let delivered = deliver(session, answers.clone(), vec![entry["message"].clone()]).await;
        let kinds = delivered
            .iter()
            .filter_map(delivery_kind)
            .collect::<Vec<_>>();
        let expected = entry["expect"]
            .as_array()
            .expect("expect")
            .iter()
            .map(|kind| kind.as_str().expect("a kind"))
            .collect::<Vec<_>>();
        assert_eq!(kinds, expected, "{}: {delivered:?}", entry["about"]);
    }
}
