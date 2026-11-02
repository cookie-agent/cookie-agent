//! The `_acp` template an ACP agent without a document of its own follows,
//! and why ACP agents cannot run (`docs/site/specs/acp-onboarding.md`
//! sections 6.3 and 8).

use cookie_agent_protocol::{
    AgentId, AgentUnavailableReason, ClientRunId, EventOrigin, RunSelection, RunStartParams,
};

use super::acp::*;
use super::support::*;

const TEMPLATE: &str = "---\ndescription: An ACP agent driven by cookie\nmode: all\nacp:\n  agent: elsewhere\npermissions:\n  bash: deny\n---\nYou run inside cookie.\n";

fn id(id: &str) -> AgentId {
    AgentId::new(id).unwrap()
}

fn descriptor(
    engine: &crate::Engine,
    agent: &str,
) -> Option<cookie_agent_protocol::AgentDescriptor> {
    engine
        .current_runtime()
        .result
        .snapshot
        .agents
        .iter()
        .find(|descriptor| descriptor.id.as_str() == agent)
        .cloned()
}

async fn answer(engine: &crate::Engine, text: &str) -> String {
    let selection = RunSelection {
        agent: agent_id(),
        model: cookie_agent_protocol::acp_model_selection(&agent_id()),
        preset: None,
        acp_runtime: None,
    };
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    let run = engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new("first").unwrap(),
                selection,
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

#[tokio::test(flavor = "multi_thread")]
async fn a_declaration_without_a_document_follows_the_acp_template() {
    let acp = acp_fixture(&[], &[("_acp.md", TEMPLATE)]).await;
    let engine = acp.engine();
    let fixture = descriptor(engine, "fixture").expect("the declaration is an agent");
    // The declaration's description, the template's rules; the template's
    // `acp.agent` does not move it off its own declaration.
    assert_eq!(fixture.description, "Fixture agent");
    assert!(fixture.runnable_as_root);
    assert_eq!(fixture.runtimes[0].agent, agent_id());
    assert!(fixture.unavailable.is_none());
    assert!(
        descriptor(engine, "acp").is_none(),
        "a template is no agent"
    );
    assert_eq!(
        answer(engine, "permission execute rm -rf build").await,
        "permission: no"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_document_of_the_agents_id_wins_over_the_template() {
    let own = "---\ndescription: Mine\nmode: all\nacp: {}\npermissions:\n  bash: allow\n---\n";
    let acp = acp_fixture(&[], &[("_acp.md", TEMPLATE), ("fixture.md", own)]).await;
    let engine = acp.engine();
    assert_eq!(descriptor(engine, "fixture").unwrap().description, "Mine");
    assert_eq!(
        answer(engine, "permission execute rm -rf build").await,
        "permission: yes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_agents_that_cannot_run_say_why() {
    let off = "---\ndescription: Fixture\nmode: all\nenabled: false\nacp: {}\n---\n";
    let ghost = "---\ndescription: Ghost\nmode: all\nacp:\n  agent: ghost\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", off), ("haunted.md", ghost)]).await;
    let engine = acp.engine();
    let fixture = descriptor(engine, "fixture").unwrap();
    assert!(!fixture.runnable_as_root);
    assert_eq!(
        fixture.unavailable.map(|unavailable| unavailable.reason),
        Some(AgentUnavailableReason::Disabled)
    );
    let haunted = descriptor(engine, "haunted").expect("listed though no runtime declares it");
    assert!(!haunted.runnable_as_root);
    assert_eq!(
        haunted.unavailable.map(|unavailable| unavailable.reason),
        Some(AgentUnavailableReason::AcpNotOffered)
    );
    assert!(
        engine
            .current_runtime()
            .agents
            .get(&id("haunted"))
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declaration_turned_off_everywhere_says_so() {
    let acp = acp_fixture_with(
        &[],
        &[],
        "[runtimes.local.acp.agents.other]\ncommand = [\"true\"]\nenabled = false\n",
    )
    .await;
    let engine = acp.engine();
    let other = descriptor(engine, "other").unwrap();
    assert!(!other.runnable_as_root);
    assert_eq!(
        other.unavailable.map(|unavailable| unavailable.reason),
        Some(AgentUnavailableReason::AcpDeclarationDisabled)
    );
}
