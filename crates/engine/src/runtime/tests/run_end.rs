//! A run's end reaches clients only once the engine treats its session as
//! idle: a client that acts on `RunFinished` at once is never told another
//! run is still active.

use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload, EventSubscriptionMessage, Open, RunId, RunOutcome,
    RunSelection, RunStartParams, SessionId,
};

use super::support::*;
use crate::{Engine, runtime::EngineError};

/// Rounds per test: the window the old ordering left was a few
/// instructions wide, so it takes many to meet it.
const ROUNDS: usize = 100;

fn origin() -> EventOrigin {
    EventOrigin::new("client:test").unwrap()
}

/// A model server that answers every request by its last message: `hang`
/// gets no answer until the connection drops, `fail` a 400, anything else
/// a short reply.
async fn model_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("model listener");
    let address = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt as _;
                let request = read_scripted_http_request(&mut socket).await;
                let last = scripted_effective_last_message(&request)
                    .map(|message| message.to_string())
                    .unwrap_or_default();
                if last.contains("hang") {
                    std::future::pending::<()>().await;
                } else if last.contains("fail") {
                    let body = r#"{"error":{"message":"bad request"}}"#;
                    let response = format!(
                        "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                } else {
                    write_scripted_sse(&mut socket, &scripted_text_body("done")).await;
                }
            });
        }
    });
    format!("http://{address}/v1")
}

async fn start(
    engine: &Engine,
    session: SessionId,
    selection: &RunSelection,
    key: &str,
    text: &str,
) -> Result<RunId, EngineError> {
    engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new(key).unwrap(),
                selection: selection.clone(),
                content: text.into(),
            },
            origin(),
        )
        .await
        .map(|started| started.run_id)
}

/// Waits on `live` for `run`'s `RunFinished` and returns its outcome.
async fn run_end(
    live: &mut tokio::sync::mpsc::Receiver<EventSubscriptionMessage>,
    run: RunId,
) -> RunOutcome {
    with_watchdog("the run's end", async {
        loop {
            if let EventSubscriptionMessage::Event { event } =
                live.recv().await.expect("live subscription")
                && event.run_id == Some(run)
                && let EventPayload::RunFinished {
                    outcome: Open::Known(outcome),
                } = &event.payload
            {
                return outcome.clone();
            }
        }
    })
    .await
}

/// How a run ends.
#[derive(Clone, Copy, Debug)]
enum Ending {
    Completed,
    Failed,
    Cancelled,
}

/// Ends a run of a fresh session as `ending` and, the moment its
/// `RunFinished` arrives, issues a command that needs the session idle,
/// many times over: every other round a delete, which minds the engine's
/// active runs, and between them another run, a revert, or a fork.
async fn idle_commands_are_accepted_on_run_end(ending: Ending) {
    let endpoint = model_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    for round in 0..ROUNDS {
        let session = engine.create_session(selection.clone()).unwrap().session_id;
        let (_, mut live) = engine.subscribe(session, None).await.expect("subscribe");
        let text = match ending {
            Ending::Completed => "hello",
            Ending::Failed => "fail now",
            Ending::Cancelled => "hang on",
        };
        let first = start(engine, session, &selection, "first", text)
            .await
            .expect("first run");
        if matches!(ending, Ending::Cancelled) {
            engine.cancel_run(first, origin()).await.expect("cancel");
        }
        let outcome = run_end(&mut live, first).await;
        match (ending, &outcome) {
            (Ending::Completed, RunOutcome::Completed { .. })
            | (Ending::Failed, RunOutcome::Failed { .. })
            | (Ending::Cancelled, RunOutcome::Cancelled { .. }) => {}
            _ => panic!("round {round}: {ending:?} run ended {outcome:?}"),
        }
        let tip = engine
            .inner
            .store
            .get(session)
            .unwrap()
            .log
            .last_event()
            .map_or(0, |event| event.seq);
        let pick = if round % 2 == 1 { 3 } else { round / 2 % 3 };
        let (command, result) = match pick {
            0 => (
                "run start",
                start(engine, session, &selection, "second", "hello again")
                    .await
                    .map(drop),
            ),
            1 => (
                "revert",
                engine
                    .revert_session(session, 1, origin(), Some(tip))
                    .await
                    .map(drop),
            ),
            2 => (
                "fork",
                engine
                    .fork_session(session, tip, origin(), None)
                    .await
                    .map(drop),
            ),
            _ => (
                "delete",
                engine.delete_session(session, tip).await.map(drop),
            ),
        };
        if let Err(error) = result {
            panic!("round {round}: {command} after a {ending:?} run was refused: {error:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_commands_are_accepted_the_moment_a_completed_run_ends() {
    idle_commands_are_accepted_on_run_end(Ending::Completed).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_commands_are_accepted_the_moment_a_failed_run_ends() {
    idle_commands_are_accepted_on_run_end(Ending::Failed).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_commands_are_accepted_the_moment_a_cancelled_run_ends() {
    idle_commands_are_accepted_on_run_end(Ending::Cancelled).await;
}
