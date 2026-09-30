//! Live-only model output reaches the conversation exactly once, through the
//! real client, server and engine, however the view came to watch the
//! session: a first prompt, an attach mid-reply, a switch away and back, or
//! a recovery replay. Each step checks the partial reply while it streams,
//! not only the committed turn that replaces it.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use cookie_agent_protocol::{
    ClientDelivery, ClientRunId, EventSubscriptionMessage, RunStartParams, RunSteerParams,
    SessionCreateParams, SessionId,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::mpsc,
};

use super::support::*;
use crate::{
    state::AssistantChild,
    ui::{App, transcript::TranscriptItem},
};

/// One step of a scripted streaming reply.
enum Chunk {
    Reasoning(&'static str),
    Text(&'static str),
    Stop,
}

/// An OpenAI-compatible endpoint whose streaming replies advance only when
/// the test sends the next chunk.
struct GatedModel {
    endpoint: String,
    chunks: mpsc::UnboundedSender<Chunk>,
    calls: Arc<AtomicUsize>,
}

impl GatedModel {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("gated model listener");
        let address = listener.local_addr().expect("gated model address");
        let (chunks, mut script) = mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&calls);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                read_request(&mut socket).await;
                served.fetch_add(1, Ordering::SeqCst);
                if socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                    )
                    .await
                    .is_err()
                {
                    continue;
                }
                while let Some(chunk) = script.recv().await {
                    let (delta, finish) = match chunk {
                        Chunk::Reasoning(text) => {
                            (serde_json::json!({ "reasoning_content": text }), None)
                        }
                        Chunk::Text(text) => (serde_json::json!({ "content": text }), None),
                        Chunk::Stop => (serde_json::json!({}), Some("stop")),
                    };
                    let data = serde_json::json!({
                        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
                    });
                    let mut frame = format!("data: {data}\n\n");
                    if finish.is_some() {
                        frame.push_str("data: [DONE]\n\n");
                    }
                    if socket.write_all(frame.as_bytes()).await.is_err() || finish.is_some() {
                        break;
                    }
                    let _ = socket.flush().await;
                }
                let _ = socket.shutdown().await;
            }
        });
        Self {
            endpoint: format!("http://{address}/v1"),
            chunks,
            calls,
        }
    }

    fn send(&self, chunk: Chunk) {
        self.chunks.send(chunk).expect("gated model script");
    }

    async fn wait_for_call(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.calls.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("model call timed out");
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = socket.read(&mut buffer).await.unwrap_or(0);
        if read == 0 {
            return;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= end + 4 + length {
            return;
        }
    }
}

/// The thinking and text the conversation shows for `session`, in order.
fn shown(app: &App, session: SessionId) -> (String, String) {
    let mut thinking = String::new();
    let mut text = String::new();
    let Some(state) = app.store.sessions.get(&session) else {
        return (thinking, text);
    };
    for item in &state.transcript {
        let TranscriptItem::Assistant { children, .. } = item else {
            continue;
        };
        for child in children {
            match child {
                AssistantChild::Thinking { text: part, .. } => thinking.push_str(part),
                AssistantChild::Text { markdown, .. } => text.push_str(markdown.as_str()),
                _ => {}
            }
        }
    }
    (thinking, text)
}

/// Drives the app the way its event loop does until `done` holds.
async fn pump_until(
    app: &mut App,
    deliveries: &mut mpsc::UnboundedReceiver<ClientDelivery>,
    session: SessionId,
    what: &str,
    mut done: impl FnMut(&App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done(app) {
        assert!(
            Instant::now() < deadline,
            "{what}: timed out; the conversation shows {:?}",
            shown(app, session)
        );
        pump_once(app, deliveries, Duration::from_millis(10)).await;
    }
}

async fn pump_once(
    app: &mut App,
    deliveries: &mut mpsc::UnboundedReceiver<ClientDelivery>,
    idle: Duration,
) -> bool {
    tokio::select! {
        Some(delivery) = deliveries.recv() => {
            app.handle_delivery(delivery).await;
            true
        }
        Some(update) = app.rpc_updates_rx.recv() => {
            app.handle_rpc_update(update);
            true
        }
        () = tokio::time::sleep(idle) => false,
    }
}

/// Drives the app until nothing has arrived for a while, so any duplicate
/// delivery of what was just streamed has had every chance to land.
async fn settle(app: &mut App, deliveries: &mut mpsc::UnboundedReceiver<ClientDelivery>) {
    while pump_once(app, deliveries, Duration::from_millis(150)).await {}
}

/// Streams `chunk` and requires the partial reply to become exactly
/// `expected` (thinking, text) and stay that way.
async fn stream_step(
    app: &mut App,
    deliveries: &mut mpsc::UnboundedReceiver<ClientDelivery>,
    model: &GatedModel,
    session: SessionId,
    chunk: Chunk,
    expected: (&str, &str),
) {
    model.send(chunk);
    let wanted = (expected.0.to_owned(), expected.1.to_owned());
    pump_until(app, deliveries, session, "streamed chunk", |app| {
        let now = shown(app, session);
        now.0.len() >= wanted.0.len() && now.1.len() >= wanted.1.len()
    })
    .await;
    settle(app, deliveries).await;
    assert_eq!(shown(app, session), wanted, "partial reply while streaming");
}

async fn finish_reply(
    app: &mut App,
    deliveries: &mut mpsc::UnboundedReceiver<ClientDelivery>,
    model: &GatedModel,
    session: SessionId,
    expected: (&str, &str),
) {
    model.send(Chunk::Stop);
    pump_until(app, deliveries, session, "committed reply", |app| {
        app.store.sessions.get(&session).is_some_and(|state| {
            state.transcript.iter().any(|item| {
                matches!(
                    item,
                    TranscriptItem::Assistant {
                        committed_turn_seq: Some(_),
                        ..
                    }
                )
            })
        })
    })
    .await;
    settle(app, deliveries).await;
    assert_eq!(
        shown(app, session),
        (expected.0.to_owned(), expected.1.to_owned()),
        "committed reply"
    );
}

async fn streaming_app(
    model: &GatedModel,
) -> (tempfile::TempDir, Arc<cookie_agent_server::Server>) {
    crate::tests::in_process_streaming_server(&model.endpoint)
}

/// Starts a reply in a fresh session from a separate connection, the way a
/// second `cookie` window (or the headless runner) would.
async fn start_reply_elsewhere(
    server: &Arc<cookie_agent_server::Server>,
    input: &str,
) -> (crate::Client, SessionId, cookie_agent_protocol::RunId) {
    let runner = Arc::clone(server).connect_in_process();
    runner.handshake().await.expect("runner handshake");
    let session = runner
        .create_session(SessionCreateParams {
            selection: crate::tests::test_run_selection(),
        })
        .await
        .expect("create session")
        .session
        .session_id;
    let run = runner
        .start_run(RunStartParams {
            session_id: session,
            client_run_id: ClientRunId::new(format!("live-stream-{}", uuid::Uuid::now_v7()))
                .expect("client run ID"),
            selection: crate::tests::test_run_selection(),
            input: input.into(),
            reset_fallback: false,
        })
        .await
        .expect("start run")
        .run_id;
    (runner, session, run)
}

async fn attached_app(
    server: &Arc<cookie_agent_server::Server>,
) -> (App, mpsc::UnboundedReceiver<ClientDelivery>) {
    let client = Arc::clone(server).connect_in_process();
    client.handshake().await.expect("handshake");
    let mut app = App::new(client).await.expect("app");
    let deliveries = app.take_deliveries();
    (app, deliveries)
}

#[tokio::test]
async fn one_connection_subscribing_twice_receives_each_live_delta_once() {
    let model = GatedModel::start().await;
    let (_directory, server) = streaming_app(&model).await;
    let (_runner, session, _run) = start_reply_elsewhere(&server, "hello").await;
    model.wait_for_call(1).await;

    let client = Arc::clone(&server).connect_in_process();
    client.handshake().await.expect("handshake");
    let mut deliveries = client.subscribe_deliveries().expect("deliveries");
    // A view re-subscribes a session it already watches when it is opened
    // again, reselected, or recovered.
    client
        .subscribe_events(session, None)
        .await
        .expect("first subscription");
    client
        .subscribe_events(session, None)
        .await
        .expect("second subscription");
    for text in ["one ", "two ", "three"] {
        model.send(Chunk::Text(text));
    }
    model.send(Chunk::Stop);

    let mut streamed = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ClientDelivery::Live { message, .. } = deliveries.recv().await.expect("delivery")
            else {
                continue;
            };
            match *message {
                EventSubscriptionMessage::Transient { event } => {
                    if let cookie_agent_protocol::EventPayload::TextDelta { text, .. } =
                        event.payload
                    {
                        streamed.push_str(&text);
                    }
                }
                EventSubscriptionMessage::Event { event }
                    if matches!(
                        event.payload,
                        cookie_agent_protocol::EventPayload::RunCompleted { .. }
                    ) =>
                {
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("run completion timed out");
    assert_eq!(streamed, "one two three");
}

#[tokio::test]
async fn first_prompt_streams_each_delta_once() {
    let model = GatedModel::start().await;
    let (_directory, server) = streaming_app(&model).await;
    let client = Arc::clone(&server).connect_in_process();
    client.handshake().await.expect("handshake");
    let mut app = App::new_with_new_session(client).await.expect("app");
    let mut deliveries = app.take_deliveries();
    type_input(&mut app, "hello").await;
    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ))
    .await;
    let session = app.selected.expect("new session selected");
    model.wait_for_call(1).await;
    settle(&mut app, &mut deliveries).await;

    let steps = [
        (Chunk::Reasoning("plan "), ("plan ", "")),
        (Chunk::Reasoning("more"), ("plan more", "")),
        (Chunk::Text("Hello"), ("plan more", "Hello")),
        (Chunk::Text(", world"), ("plan more", "Hello, world")),
        (
            Chunk::Reasoning(" again"),
            ("plan more again", "Hello, world"),
        ),
        (Chunk::Text("!"), ("plan more again", "Hello, world!")),
    ];
    for (chunk, expected) in steps {
        stream_step(&mut app, &mut deliveries, &model, session, chunk, expected).await;
    }
    finish_reply(
        &mut app,
        &mut deliveries,
        &model,
        session,
        ("plan more again", "Hello, world!"),
    )
    .await;
}

#[tokio::test]
async fn attaching_mid_reply_streams_each_later_delta_once() {
    let model = GatedModel::start().await;
    let (_directory, server) = streaming_app(&model).await;
    let (_runner, session, _run) = start_reply_elsewhere(&server, "hello").await;
    model.wait_for_call(1).await;
    // Output streamed before the view attached is not replayable; the
    // committed turn brings it back.
    let observer = Arc::clone(&server).connect_in_process();
    observer.handshake().await.expect("observer handshake");
    let mut observed = observer
        .subscribe_deliveries()
        .expect("observer deliveries");
    observer
        .subscribe_events(session, None)
        .await
        .expect("observe session");
    model.send(Chunk::Text("before "));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ClientDelivery::Live { message, .. } = observed.recv().await.expect("delivery")
                && matches!(*message, EventSubscriptionMessage::Transient { .. })
            {
                break;
            }
        }
    })
    .await
    .expect("early output published");

    let (mut app, mut deliveries) = attached_app(&server).await;
    assert_eq!(app.selected, Some(session));
    settle(&mut app, &mut deliveries).await;
    assert_eq!(shown(&app, session), (String::new(), String::new()));

    stream_step(
        &mut app,
        &mut deliveries,
        &model,
        session,
        Chunk::Text("after"),
        ("", "after"),
    )
    .await;
    stream_step(
        &mut app,
        &mut deliveries,
        &model,
        session,
        Chunk::Text(" attach"),
        ("", "after attach"),
    )
    .await;
    finish_reply(
        &mut app,
        &mut deliveries,
        &model,
        session,
        ("", "before after attach"),
    )
    .await;
}

#[tokio::test]
async fn switching_away_and_back_mid_reply_streams_each_delta_once() {
    let model = GatedModel::start().await;
    let (_directory, server) = streaming_app(&model).await;
    let (runner, session, _run) = start_reply_elsewhere(&server, "hello").await;
    model.wait_for_call(1).await;
    let (mut app, mut deliveries) = attached_app(&server).await;
    settle(&mut app, &mut deliveries).await;
    stream_step(
        &mut app,
        &mut deliveries,
        &model,
        session,
        Chunk::Text("one"),
        ("", "one"),
    )
    .await;

    let other = runner
        .create_session(SessionCreateParams {
            selection: crate::tests::test_run_selection(),
        })
        .await
        .expect("other session")
        .session
        .session_id;
    // The reply keeps streaming into its own session while another one is
    // on screen, and none of it shows there.
    for (chunk, expected) in [
        (Chunk::Text(" two"), "one two"),
        (Chunk::Text(" three"), "one two three"),
    ] {
        app.select_session(other).await;
        settle(&mut app, &mut deliveries).await;
        stream_step(
            &mut app,
            &mut deliveries,
            &model,
            session,
            chunk,
            ("", expected),
        )
        .await;
        assert_eq!(shown(&app, other), (String::new(), String::new()));
        app.select_session(session).await;
        settle(&mut app, &mut deliveries).await;
        assert_eq!(
            shown(&app, session),
            (String::new(), expected.to_owned()),
            "after switching back"
        );
    }
    stream_step(
        &mut app,
        &mut deliveries,
        &model,
        session,
        Chunk::Reasoning("think"),
        ("think", "one two three"),
    )
    .await;
    stream_step(
        &mut app,
        &mut deliveries,
        &model,
        session,
        Chunk::Text(" four"),
        ("think", "one two three four"),
    )
    .await;
    finish_reply(
        &mut app,
        &mut deliveries,
        &model,
        session,
        ("think", "one two three four"),
    )
    .await;
}

#[tokio::test]
async fn recovery_replay_mid_reply_streams_each_delta_once() {
    let model = GatedModel::start().await;
    let (_directory, server) = streaming_app(&model).await;
    let (_runner, session, _run) = start_reply_elsewhere(&server, "hello").await;
    model.wait_for_call(1).await;
    let (mut app, mut deliveries) = attached_app(&server).await;
    settle(&mut app, &mut deliveries).await;
    stream_step(
        &mut app,
        &mut deliveries,
        &model,
        session,
        Chunk::Text("one"),
        ("", "one"),
    )
    .await;
    for _ in 0..2 {
        app.client.recover_session(session, false);
        settle(&mut app, &mut deliveries).await;
    }
    stream_step(
        &mut app,
        &mut deliveries,
        &model,
        session,
        Chunk::Text(" two"),
        ("", "one two"),
    )
    .await;
    finish_reply(&mut app, &mut deliveries, &model, session, ("", "one two")).await;
}

#[tokio::test]
async fn durable_input_mid_reply_keeps_each_delta_once() {
    let model = GatedModel::start().await;
    let (_directory, server) = streaming_app(&model).await;
    let (runner, session, run) = start_reply_elsewhere(&server, "hello").await;
    model.wait_for_call(1).await;
    let (mut app, mut deliveries) = attached_app(&server).await;
    settle(&mut app, &mut deliveries).await;
    stream_step(
        &mut app,
        &mut deliveries,
        &model,
        session,
        Chunk::Text("one"),
        ("", "one"),
    )
    .await;
    // A steer admitted while the reply streams is a durable event between
    // two live deltas, so the output after it follows a newer sequence.
    let tip = app.store.sessions[&session].last_seq;
    let steer = runner
        .steer_run(RunSteerParams {
            run_id: run,
            input: "also this".into(),
        })
        .await
        .expect("steer");
    assert!(steer.accepted, "{:?}", steer.handled_reason);
    settle(&mut app, &mut deliveries).await;
    assert!(app.store.sessions[&session].last_seq > tip);
    stream_step(
        &mut app,
        &mut deliveries,
        &model,
        session,
        Chunk::Text(" two"),
        ("", "one two"),
    )
    .await;
    finish_reply(&mut app, &mut deliveries, &model, session, ("", "one two")).await;
}
