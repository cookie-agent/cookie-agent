use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    BackgroundActor, BackgroundCall, MessageFrame, MessageStream, SessionId, ToolCallId,
    TransportError,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::super::{App, Modal, RpcUpdate};
use crate::{Client, config::TuiConfig, state::SessionState, theme::Theme};

struct ScriptedStream {
    incoming: mpsc::UnboundedReceiver<MessageFrame>,
    sent: mpsc::UnboundedSender<MessageFrame>,
}

#[async_trait]
impl MessageStream for ScriptedStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.sent.send(frame).map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        Ok(self.incoming.recv().await)
    }
}

/// An app whose daemon answers background requests with `replies` in order
/// and records every request.
async fn app_with_replies(replies: Vec<Value>) -> (App, Arc<Mutex<Vec<Value>>>) {
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = recorded.clone();
    let mut replies = VecDeque::from(replies);
    tokio::spawn(async move {
        while let Some(frame) = sent_rx.recv().await {
            let request: Value = match frame {
                MessageFrame::Value(value) => value,
                MessageFrame::Text(text) => serde_json::from_str(&text).expect("JSON request"),
                MessageFrame::Bytes(bytes) => serde_json::from_slice(&bytes).expect("JSON request"),
            };
            let method = request["method"].as_str().unwrap_or_default().to_owned();
            sink.lock().expect("requests").push(request.clone());
            let mut reply = json!({"jsonrpc": "2.0", "id": request["id"]});
            if method == "session.list" {
                reply["result"] = json!({"sessions": []});
            } else if method.starts_with("session.background.") || method == "run.tool_background" {
                reply["result"] = replies.pop_front().expect("expected background request");
            } else {
                reply["error"] = json!({"code": -32000, "message": "unavailable in test"});
            }
            if incoming.send(MessageFrame::Value(reply)).is_err() {
                break;
            }
        }
    });
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let app = App::new_with_config(
        client,
        false,
        crate::SessionPlacement::default(),
        TuiConfig::default(),
        Theme::default(),
    )
    .await
    .expect("app");
    recorded.lock().expect("requests").clear();
    (app, recorded)
}

fn call(job: &str, tool: &str, display: &str, by: BackgroundActor) -> BackgroundCall {
    BackgroundCall {
        job: job.into(),
        tool: tool.into(),
        tool_call_id: ToolCallId::new_v7(),
        display: Some(display.into()),
        started_at: jiff::Timestamp::now(),
        detached_at: jiff::Timestamp::now(),
        by,
    }
}

fn mount(app: &mut App, calls: Vec<BackgroundCall>) -> SessionId {
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.store.sessions.insert(
        session,
        SessionState {
            background_calls: calls,
            ..Default::default()
        },
    );
    session
}

fn row(terminal: &Terminal<TestBackend>, y: u16, width: u16) -> String {
    (0..width)
        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
        .collect()
}

#[tokio::test]
async fn the_bar_lists_background_calls_and_opens_the_panel() {
    let (mut app, _) = app_with_replies(Vec::new()).await;
    assert!(!app.background_bar_visible());
    mount(
        &mut app,
        vec![
            call("job_1", "bash", "sleep 30", BackgroundActor::Model),
            call("job_2", "job", "finish\n now", BackgroundActor::User),
        ],
    );
    assert!(app.background_bar_visible());
    let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
    terminal
        .draw(|frame| app.render_background_bar(frame, frame.area()))
        .unwrap();
    let line = row(&terminal, 0, 80);
    assert!(
        line.contains("2 in background: bash: sleep 30 · job: finish now"),
        "{line}"
    );
    assert!(line.trim_end().ends_with("[Details]"), "{line}");
    app.handle_click(10, 0).await;
    assert_eq!(app.modal, Modal::Background);
}

#[tokio::test]
async fn the_panel_stops_the_selected_call_as_the_user() {
    let (mut app, recorded) = app_with_replies(vec![json!({})]).await;
    let session = mount(
        &mut app,
        vec![
            call("job_1", "bash", "sleep 30", BackgroundActor::Model),
            call("job_2", "job", "finish", BackgroundActor::User),
        ],
    );
    app.open_background_panel();
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    terminal
        .draw(|frame| app.render_background_panel(frame))
        .unwrap();
    let text = (0..12).map(|y| row(&terminal, y, 80)).collect::<Vec<_>>();
    assert!(
        text.iter()
            .any(|line| line.contains("job_2 job: finish") && line.contains("by you")),
        "{text:#?}"
    );
    app.handle_background_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_background_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("stop answer")
        .expect("update");
    // A stop that worked is no error: it is not reported when the TUI exits.
    let RpcUpdate::Notice(status) = &update else {
        panic!("a notice, not an error");
    };
    assert_eq!(status, "stopped job_2");
    app.handle_rpc_update(update);
    assert_eq!(app.status, "stopped job_2");
    assert!(app.session_errors.format().is_none());
    let requests = recorded.lock().unwrap().clone();
    let stop = requests
        .iter()
        .find(|request| request["method"] == "session.background.stop")
        .expect("stop request");
    assert_eq!(stop["params"]["job"], "job_2");
    assert_eq!(stop["params"]["session_id"], session.to_string());
    app.handle_background_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.modal, Modal::None);
}
