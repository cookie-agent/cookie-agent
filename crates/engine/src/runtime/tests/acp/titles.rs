//! An ACP session's title: the agent's own, else cookie's title
//! generation on the title agent's models, never the synthetic
//! `acp/<agent>`.

use cookie_agent_protocol::{EventPayload, SessionId, SessionTitleChange};

use super::super::support::scripted_text_body;
use super::*;

/// What `session`'s titles were, in order.
fn titles(engine: &Engine, session: SessionId) -> Vec<SessionTitleChange> {
    events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::SessionTitleCommitted { change, .. } => Some(change),
            _ => None,
        })
        .collect()
}

fn title(engine: &Engine, session: SessionId) -> Option<String> {
    engine
        .inner
        .store
        .get(session)
        .unwrap()
        .meta
        .title
        .as_ref()
        .map(|title| title.as_str().to_owned())
}

/// A title model on a scripted server that answers `answers`.
async fn title_model(answers: &[&str]) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let (endpoint, captured, _, _) = super::super::support::scripted_server_with_delayed_response(
        answers
            .iter()
            .map(|answer| scripted_text_body(answer))
            .collect(),
        usize::MAX,
    )
    .await;
    let config = format!(
        "utility_model = \"custom.test/group/model\"\n\n[session_title]\ngenerate_on_first_turn = true\nfallback_to_input_excerpt = false\n\n[providers.\"custom.test\"]\nsource = \"custom\"\nendpoint = \"{endpoint}\"\nadaptor = \"openai-compatible\"\nauth = {{ method = \"no-auth-v1\", values = {{}} }}\n\n[providers.\"custom.test\".models.\"group/model\"]\ndisplay_name = \"Model\"\n\n[providers.\"custom.test\".models.\"group/model\".capabilities]\ninput = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 8192\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = true\nnative_replay = \"unsupported\"\nmedia = {{}}\n"
    );
    (config, captured)
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_utility_model_an_untitled_acp_session_stays_untitled() {
    let acp = acp_fixture_with(
        &[],
        &[],
        "[session_title]\ngenerate_on_first_turn = true\nfallback_to_input_excerpt = true\n",
    )
    .await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "one", "say one").await;
    assert!(titles(engine, session).is_empty());
    assert!(
        !events(engine, session)
            .iter()
            .any(|event| matches!(event.payload, EventPayload::InternalAgentStarted { .. }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untitled_acp_session_gets_a_generated_title_and_the_agents_later_one() {
    let (config, captured) = title_model(&["Parser refactor"]).await;
    let acp = acp_fixture_with(&[], &[], &config).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "one", "say one").await;
    assert_eq!(title(engine, session).as_deref(), Some("Parser refactor"));
    assert!(matches!(
        titles(engine, session).as_slice(),
        [SessionTitleChange::InternalAgentSet { .. }]
    ));
    // The agent's title still applies while the user named nothing.
    turn(engine, session, "two", "title Agent title").await;
    assert_eq!(title(engine, session).as_deref(), Some("Agent title"));
    turn(engine, session, "three", "title Agent title").await;
    assert_eq!(titles(engine, session).len(), 2, "the same title once");
    // Not over the user's.
    engine
        .rename_session(
            cookie_agent_protocol::SessionRenameParams {
                session_id: session,
                client_rename_id: cookie_agent_protocol::ClientRenameId::new("rename-1").unwrap(),
                change: cookie_agent_protocol::SessionRenameChange::Set {
                    title: cookie_agent_protocol::SessionTitle::new("Mine").unwrap(),
                },
            },
            origin(),
        )
        .await
        .unwrap();
    turn(engine, session, "four", "title Agent again").await;
    assert_eq!(title(engine, session).as_deref(), Some("Mine"));
    drop(acp);
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 1, "one title request, on the small model");
    assert!(
        requests[0].contains("\"model\":\"group/model\""),
        "{}",
        requests[0]
    );
}
