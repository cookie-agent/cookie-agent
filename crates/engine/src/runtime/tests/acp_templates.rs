//! ACP agent documents as templates over a declaration
//! (`docs/site/specs/acp-agents.md`, addendum of 2026-10-25): documents
//! that run the runtime's `fixture` under their own IDs, rules, and tool
//! lists.

use cookie_agent_protocol::{
    AgentId, ClientRunId, EventOrigin, EventPayload, RunSelection, RunStartParams, SessionId,
};

use super::acp::*;
use super::support::*;
use crate::Engine;

const REVIEW: &str = "---\ndescription: Fixture, read-only\nmode: all\nacp:\n  agent: fixture\npermissions:\n  bash: deny\n---\n";
const OPEN: &str = "---\ndescription: Fixture, trusted\nmode: all\nacp:\n  agent: fixture\n  hide_tools: [\"web*\"]\npermissions:\n  bash: allow\n  read: { \"${workspace_dir}/*\": allow }\n---\n";

fn id(id: &str) -> AgentId {
    AgentId::new(id).unwrap()
}

fn selection_of(agent: &str) -> RunSelection {
    RunSelection {
        agent: id(agent),
        model: cookie_agent_protocol::acp_model_selection(&id(agent)),
        preset: None,
    }
}

/// Runs one prompt `text` of `agent` in `session` and returns its final text.
async fn turn_of(
    engine: &Engine,
    session: SessionId,
    agent: &str,
    key: &str,
    text: &str,
) -> String {
    let run = engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new(key).unwrap(),
                selection: selection_of(agent),
                content: text.into(),
            },
            EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run started")
        .run_id;
    let outcome = finished(engine, session, run).await;
    wait_for_session_not_running(engine, session).await;
    final_text(&outcome)
}

fn descriptor(engine: &Engine, agent: &str) -> Option<cookie_agent_protocol::AgentDescriptor> {
    engine
        .current_runtime()
        .result
        .snapshot
        .agents
        .iter()
        .find(|descriptor| descriptor.id.as_str() == agent)
        .cloned()
}

fn tools(listed: &str) -> Vec<&str> {
    listed
        .strip_prefix("tools: ")
        .expect("a tool list")
        .split(',')
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_documents_run_one_declaration_each_with_its_own_rules_and_tools() {
    let acp = acp_fixture(&[], &[("review.md", REVIEW), ("open.md", OPEN)]).await;
    let engine = acp.engine();
    // Both are agents over `fixture`, and the bare declaration is one too.
    for agent in ["review", "open"] {
        let descriptor = descriptor(engine, agent).expect("the template is listed");
        assert!(descriptor.runnable_as_root, "{agent}");
        assert_eq!(descriptor.runtimes.len(), 1, "{agent}");
        assert_eq!(descriptor.runtimes[0].runtime_id.as_str(), "local");
        assert_eq!(descriptor.runtimes[0].agent, agent_id(), "{agent}");
        assert_eq!(
            descriptor.resolved_fallback,
            [cookie_agent_protocol::acp_model_selection(&id(agent))]
        );
    }
    let bare = descriptor(engine, "fixture").expect("the bare declaration");
    assert_eq!(bare.description, "Fixture agent");
    assert!(engine.configuration_status().diagnostics.is_empty());

    let review = engine
        .create_session(selection_of("review"))
        .unwrap()
        .session_id;
    let open = engine
        .create_session(selection_of("open"))
        .unwrap()
        .session_id;
    // Each document's rules decide the agent's own requests.
    let ask = "permission execute rm -rf build";
    assert_eq!(
        turn_of(engine, review, "review", "ask", ask).await,
        "permission: no"
    );
    assert_eq!(
        turn_of(engine, open, "open", "ask", ask).await,
        "permission: yes"
    );
    // And its `hide_tools` and rules the bridge's tools.
    let listed = turn_of(engine, review, "review", "list", "mcp list").await;
    assert!(!tools(&listed).contains(&"read"), "{listed}");
    let listed = turn_of(engine, open, "open", "list", "mcp list").await;
    assert!(tools(&listed).contains(&"read"), "{listed}");
    assert!(!tools(&listed).contains(&"webfetch"), "{listed}");

    // One process serves both sessions, each with its own ACP session.
    let methods = acp.received_methods();
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "initialize")
            .count(),
        1,
        "{methods:?}"
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "session/new")
            .count(),
        2,
        "{methods:?}"
    );
    let record = session_starts(engine, review).pop().unwrap();
    assert_eq!(record.agent.as_str(), "review");
    assert_eq!(record.runtime_agent, agent_id());

    // What the agent offered is the declaration's: a draft of the bare
    // agent offers it too.
    assert!(bare.runtimes[0].options.is_empty(), "nothing known before");
    let bare = descriptor(engine, "fixture").unwrap();
    assert!(!bare.runtimes[0].options.is_empty());

    // `list_agents` names the declaration a template runs.
    let listed = turn_of(engine, open, "open", "agents", "mcp call list_agents {}").await;
    assert!(
        listed.contains("review (all, acp) — Fixture, read-only\n  acp agent: fixture\n"),
        "{listed}"
    );
    assert!(
        !listed.contains("fixture (all, acp) — Fixture agent\n  acp agent:"),
        "{listed}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_bare_declaration_leaves_its_templates_running() {
    let hidden = "---\ndescription: Fixture\nmode: all\nenabled: false\nacp: {}\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", hidden), ("review.md", REVIEW)]).await;
    let engine = acp.engine();
    assert!(!descriptor(engine, "fixture").unwrap().runnable_as_root);
    assert!(descriptor(engine, "review").unwrap().runnable_as_root);
    let session = engine
        .create_session(selection_of("review"))
        .unwrap()
        .session_id;
    assert_eq!(
        turn_of(engine, session, "review", "hello", "say hello").await,
        "hello"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_document_over_another_or_no_declaration_is_a_diagnostic() {
    // `fixture.md` runs `ghost`, which no runtime declares: the declaration
    // `fixture` is no agent of its own, and the document none yet.
    let elsewhere = "---\ndescription: Elsewhere\nmode: all\nacp:\n  agent: ghost\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", elsewhere)]).await;
    let engine = acp.engine();
    assert!(descriptor(engine, "fixture").is_none());
    assert_eq!(
        engine.configuration_status().diagnostics,
        [
            "ACP agent `fixture` of runtime `local` is ignored: the agent document `fixture` runs \
             `ghost` (`acp.agent`), so `fixture` is that document's agent",
            "agent `fixture` runs ACP agent `ghost` (`acp.agent`), which no runtime declares \
             yet; it is an agent once one does",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn another_template_or_declaration_starts_a_new_acp_session() {
    let second = format!(
        "[runtimes.local.acp.agents.second]\ncommand = [{}, {}]\n",
        serde_json::to_string(python_command()).unwrap(),
        serde_json::to_string(&fixture_script().to_string_lossy()).unwrap(),
    );
    let acp = acp_fixture_with(&[], &[("review.md", REVIEW), ("open.md", OPEN)], &second).await;
    let engine = acp.engine();
    let session = engine
        .create_session(selection_of("review"))
        .unwrap()
        .session_id;
    turn_of(engine, session, "review", "first", "say hello").await;
    turn_of(engine, session, "review", "again", "say hello").await;
    assert_eq!(session_starts(engine, session).len(), 1, "resumed");
    // Switching to another template starts its own ACP session.
    turn_of(engine, session, "open", "switch", "say hello").await;
    let starts = session_starts(engine, session);
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].agent.as_str(), "open");

    // A document that now runs another declaration starts afresh there.
    let moved = REVIEW.replace("agent: fixture", "agent: second");
    let agents = acp.fixture._directory.path().join("user/agents");
    std::fs::write(agents.join("review.md"), moved).unwrap();
    engine
        .reload_configuration()
        .await
        .expect("reloaded configuration");
    assert_eq!(
        descriptor(engine, "review").unwrap().runtimes[0]
            .agent
            .as_str(),
        "second"
    );
    turn_of(engine, session, "review", "moved", "say hello").await;
    let starts = session_starts(engine, session);
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[2].agent.as_str(), "review");
    assert_eq!(starts[2].runtime_agent.as_str(), "second");
    let notices = events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::Diagnostic { message, .. } => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        notices.iter().any(|notice| notice
            .contains("is of ACP agent `fixture`, and its document now runs `second`")),
        "{notices:?}"
    );
}
