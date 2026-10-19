//! The plugin corpora against this SDK: the daemon's messages of each
//! recorded conversation (`corpus/plugin/<version>/`) drive a plugin built
//! with the SDK that does what the reference plugin does, as recorded and
//! with a member no build knows added to every object of them; and what a
//! newer daemon may send (`corpus/future/plugin.json`) reaches the plugin
//! as this build reads it. `cookie_agent_protocol::corpus` describes the
//! corpora and their rules.
//!
//! The replay ([`corpus::play`]) sends the daemon's recorded requests and
//! notifications in their recorded order, waiting before each for the
//! plugin's messages that preceded it, and answers the plugin's requests
//! with the daemon's recorded replies. Each daemon request must be answered
//! as it was.

use std::{collections::BTreeMap, time::Duration};

use cookie_agent_plugin_sdk::{
    DisplayDelta, ExtensionHookOnError, ExtensionInterceptionHook, PluginServer, Presentation,
    ToolDecl, ToolFailure, ToolOutput, allow, block,
};
use cookie_agent_protocol::{
    MessageFrame, Transport,
    corpus::{self, CorpusFile, Substitutions},
    in_process_pair,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// A liveness bound, not a latency assertion.
const WAIT: Duration = Duration::from_secs(10);

/// Members of plugin messages whose contents are data, not types.
const OPAQUE: &[&str] = &[
    "arguments",
    "normalized_arguments",
    "metadata",
    "parameters",
    "value",
];

/// What the plugin saw, in order: its hooks, events, and messages it does
/// not know.
type Observed = mpsc::UnboundedReceiver<String>;

/// A plugin that does what `examples/plugins/python/reference_plugin.py`
/// does, built with the SDK.
fn reference_plugin() -> (PluginServer, Observed) {
    let (seen, observed) = mpsc::unbounded_channel();
    let declaration: ToolDecl = serde_json::from_value(json!({
        "name": "word_count",
        "description": "Count the words and lines of a text.",
        "permission": "text_read",
        "parameters": {
            "type": "object",
            "properties": {"text": {"type": "string"}, "label": {"type": "string"}},
            "required": ["text"],
            "additionalProperties": false,
        },
    }))
    .expect("declaration");
    let counted = std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new()));
    let (hook_seen, event_seen, bus_seen, unknown_seen) =
        (seen.clone(), seen.clone(), seen.clone(), seen);
    let tool_counted = std::sync::Arc::clone(&counted);
    let server = PluginServer::builder("reference", "1.0.0")
        .tool_with_resource(
            declaration,
            |arguments| Some(arguments["label"].as_str().unwrap_or("text").to_owned()),
            move |context, arguments| {
                let counted = std::sync::Arc::clone(&tool_counted);
                async move {
                    let failed = |error: cookie_agent_plugin_sdk::PluginError| {
                        ToolFailure::new(error.to_string())
                    };
                    let delta = |delta: Value| -> DisplayDelta {
                        serde_json::from_value(delta).expect("display delta")
                    };
                    context
                        .progress_display(
                            "counting",
                            delta(json!({"type": "replace",
                                "blocks": [{"type": "text", "text": "Counting words…"}]})),
                        )
                        .await
                        .map_err(failed)?;
                    let text = arguments["text"].as_str().unwrap_or_default();
                    let (words, lines) = (text.split_whitespace().count(), text.lines().count());
                    context
                        .progress_display(
                            "counted",
                            delta(json!({"type": "append", "index": 0, "text": " done."})),
                        )
                        .await
                        .map_err(failed)?;
                    counted
                        .lock()
                        .expect("counted")
                        .insert(context.tool_call_id().to_string(), words);
                    Ok(ToolOutput::success(format!("{words} words, {lines} lines"))
                        .title(format!("Counted {words} words"))
                        .metadata(json!({"words": words, "lines": lines})))
                }
            },
        )
        .prepare_presentation("word_count", |call| {
            Presentation::new("Count words")
                .hint("search")
                .primary_argument(call.resource.unwrap_or("text"))
        })
        .tool_before_call(move |_context, params| {
            let seen = hook_seen.clone();
            async move {
                let _ = seen.send(format!("hook {} {}", params.tool, params.origin.tag()));
                if params.arguments.to_string().contains("forbidden") {
                    block("the reference plugin refuses forbidden text")
                } else {
                    allow()
                }
            }
        })
        .on_hook_error(
            ExtensionInterceptionHook::ToolBeforeCall,
            ExtensionHookOnError::Block,
        )
        .event_patterns(["run_*", "tool_call_*"])
        .on_event(move |context, params| {
            let seen = event_seen.clone();
            let counted = std::sync::Arc::clone(&counted);
            async move {
                let event = serde_json::to_value(&params.event).expect("event");
                let kind = event["type"].as_str().unwrap_or_default().to_owned();
                let _ = seen.send(format!(
                    "event {kind} unknown={}",
                    params.event.is_unknown()
                ));
                if kind != "tool_call_terminated" {
                    return;
                }
                let call = event["tool_call_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let Some(words) = counted.lock().expect("counted").remove(&call) else {
                    return;
                };
                let producer = context
                    .register_producer(params.session_id)
                    .await
                    .expect("producer/register");
                producer
                    .queue(
                        format!("The word count you asked for found {words} words."),
                        "Word count finished",
                        serde_json::from_value(json!(format!("word-count-{call}")))
                            .expect("idempotency key"),
                    )
                    .await
                    .expect("producer/send");
            }
        })
        .on_bus_event(move |_context, event| {
            let seen = bus_seen.clone();
            async move {
                let _ = seen.send(format!("bus {} {}", event.plugin, event.name));
            }
        })
        .on_unknown(move |_context, message| {
            let seen = unknown_seen.clone();
            async move {
                let _ = seen.send(format!("unknown {}", message.method));
            }
        })
        .enable_producers()
        .build()
        .expect("the plugin");
    (server, observed)
}

/// Plays the daemon's side of `file` to `server`, with `transform` applied
/// to every message the daemon sends ([`corpus::play`]).
async fn replay(file: &CorpusFile, server: PluginServer, transform: impl Fn(Value) -> Value) {
    let (daemon, plugin) = in_process_pair(64);
    let running = tokio::spawn(server.run_transport(Box::new(plugin)));
    corpus::play(
        file,
        "daemon",
        daemon,
        transform,
        &mut Substitutions::default(),
    )
    .await;
    tokio::time::timeout(WAIT, running)
        .await
        .expect("the plugin stops")
        .expect("the plugin task")
        .expect("the plugin ends cleanly");
}

fn drain(observed: &mut Observed) -> Vec<String> {
    let mut seen = Vec::new();
    while let Ok(entry) = observed.try_recv() {
        seen.push(entry);
    }
    seen
}

/// Each recorded plugin conversation drives a plugin built with this SDK:
/// every request the daemon sent is answered as it was, every event reaches
/// the plugin as a type it knows, and a member no build knows added at
/// every level of the daemon's messages changes nothing.
#[tokio::test]
async fn recorded_daemon_messages_drive_a_plugin_built_with_this_sdk() {
    let methods = corpus::methods("plugin");
    let versions = corpus::frozen_versions("plugin");
    assert!(!versions.is_empty(), "no frozen plugin corpus");
    for version in versions {
        for file in corpus::frozen("plugin", version) {
            for message in file
                .messages
                .iter()
                .filter(|message| message.from == "daemon")
            {
                if let Some(method) = message.method() {
                    let entry = methods
                        .get(method)
                        .unwrap_or_else(|| panic!("{}: `{method}` is no plugin method", file.name));
                    assert_eq!(entry.direction, "from_daemon", "{}: `{method}`", file.name);
                }
            }

            let (server, mut observed) = reference_plugin();
            replay(&file, server, |value| value).await;
            let seen = drain(&mut observed);
            assert!(
                seen.iter().any(|entry| entry.starts_with("hook")),
                "{}: the hook ran: {seen:?}",
                file.name
            );
            assert!(
                seen.iter().any(|entry| entry.starts_with("event")),
                "{}: events arrived: {seen:?}",
                file.name
            );
            assert!(
                seen.iter()
                    .all(|entry| !entry.contains("unknown=true") && !entry.starts_with("unknown")),
                "{}: the plugin met something it does not know: {seen:?}",
                file.name
            );

            let (server, mut observed) = reference_plugin();
            replay(&file, server, |mut value| {
                corpus::add_future_members(&mut value, OPAQUE);
                value
            })
            .await;
            assert_eq!(
                drain(&mut observed),
                seen,
                "{}: unknown members change what the plugin sees",
                file.name
            );
        }
    }
}

/// What a newer daemon may send reaches a plugin as this SDK reads it: each
/// request of `corpus/future/plugin.json` is answered as `answer` says, and
/// the plugin sees exactly what `sees` lists.
#[tokio::test]
async fn future_daemon_messages_reach_the_plugin_as_this_sdk_reads_them() {
    let fixture = corpus::future("plugin.json");
    let (server, mut observed) = reference_plugin();
    let (mut daemon, plugin) = in_process_pair(64);
    let running = tokio::spawn(server.run_transport(Box::new(plugin)));
    for entry in fixture["messages"].as_array().expect("messages") {
        let message = &entry["message"];
        daemon
            .send(MessageFrame::Value(message.clone()))
            .await
            .expect("the plugin connection");
        if let Some(answer) = entry.get("answer") {
            let frame = tokio::time::timeout(WAIT, daemon.recv())
                .await
                .expect("an answer")
                .expect("the plugin connection")
                .expect("the plugin is connected");
            let reply: Value =
                serde_json::from_str(&frame.into_text().expect("text")).expect("JSON");
            assert_eq!(reply["id"], message["id"], "{}", entry["about"]);
            // `{"error": code}`, or `{"result": members}` with members
            // the result must hold.
            if let Some(code) = answer.get("error") {
                assert_eq!(&reply["error"]["code"], code, "{}: {reply}", entry["about"]);
            } else {
                assert!(reply.get("result").is_some(), "{}: {reply}", entry["about"]);
                for (member, expected) in answer["result"].as_object().into_iter().flatten() {
                    assert_eq!(
                        &reply["result"][member], expected,
                        "{}: {reply}",
                        entry["about"]
                    );
                }
            }
        }
        let mut seen = Vec::new();
        for expected in entry["sees"].as_array().into_iter().flatten() {
            let entry = tokio::time::timeout(WAIT, observed.recv())
                .await
                .unwrap_or_else(|_| panic!("the plugin never saw {expected}"))
                .expect("observations");
            seen.push(Value::String(entry));
        }
        assert_eq!(
            Value::Array(seen),
            entry.get("sees").cloned().unwrap_or(json!([])),
            "{}",
            entry["about"]
        );
    }
    drop(daemon);
    tokio::time::timeout(WAIT, running)
        .await
        .expect("the plugin stops")
        .expect("the plugin task")
        .expect("the plugin ends cleanly");
    assert!(
        drain(&mut observed).is_empty(),
        "the plugin saw more than the fixture lists"
    );
}
