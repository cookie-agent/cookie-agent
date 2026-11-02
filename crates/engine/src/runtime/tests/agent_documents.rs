//! The agents panel's calls (`docs/site/specs/acp-onboarding.md` section
//! 7): rows with where each document comes from, and customize, turn off
//! and on, replace, and reset.

use super::support::*;
use crate::{AgentDocumentKind, AgentDocumentOrigin};

const OFFICIAL_PRIMARY: &str =
    "---\ndescription: The assistant\nmode: primary\nmodels:\n  - ${default_model}\n---\nHelp.\n";
const OFFICIAL_ACP: &str =
    "---\ndescription: An ACP agent\nmode: all\nacp: {}\n---\nYou run inside cookie.\n";

fn root(fixture: &Fixture) -> std::path::PathBuf {
    fixture._directory.path().join(".cookie-agent")
}

async fn fixture() -> Fixture {
    let fixture =
        synthetic_default_fixture_with_config(None, "http://127.0.0.1:9/v1", "").expect("engine");
    // Declared once the engine is open, as a reload applies it.
    write_private_test_file(
        &root(&fixture).join("config.toml"),
        "[runtimes.local.acp.agents.claude]\ncommand = [\"true\"]\n",
    );
    let official = root(&fixture).join("official_agents");
    create_private_test_dir(&official);
    write_private_test_file(&official.join("primary.md"), OFFICIAL_PRIMARY);
    write_private_test_file(&official.join("_acp.md"), OFFICIAL_ACP);
    fixture.engine.reload_configuration().await.expect("reload");
    fixture
}

fn row<'a>(rows: &'a [crate::AgentDocumentEntry], id: &str) -> &'a crate::AgentDocumentEntry {
    rows.iter()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("row `{id}` in {rows:?}"))
}

#[tokio::test]
async fn the_panel_lists_official_acp_and_template_rows() {
    let fixture = fixture().await;
    let rows = fixture.engine.agent_documents().await.expect("rows");
    assert_eq!(row(&rows, "primary").origin, AgentDocumentOrigin::Official);
    assert_eq!(row(&rows, "_acp").kind, AgentDocumentKind::Template);
    let claude = row(&rows, "claude");
    assert_eq!(claude.kind, AgentDocumentKind::Acp);
    assert_eq!(claude.origin, AgentDocumentOrigin::FollowsTemplate);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn customize_turn_off_and_on_replace_and_reset() {
    let fixture = fixture().await;
    let engine = &fixture.engine;
    let document = root(&fixture).join("agents/primary.md");

    // Customize: a document that inherits the official one, its keys as
    // comments; asking again leaves it as it is.
    let path = engine
        .customize_agent("primary", None)
        .await
        .expect("customized");
    assert_eq!(path, "agents/primary.md");
    let text = std::fs::read_to_string(&document).unwrap();
    assert!(
        text.starts_with("---\ninherit: ../official_agents/primary.md\n"),
        "{text}"
    );
    assert!(text.contains("#   description: The assistant"), "{text}");
    engine
        .customize_agent("primary", None)
        .await
        .expect("again");
    assert_eq!(std::fs::read_to_string(&document).unwrap(), text);
    let rows = engine.agent_documents().await.unwrap();
    assert_eq!(
        row(&rows, "primary").origin,
        AgentDocumentOrigin::Customized
    );

    // Off, then on: the document holds only `inherit` again, so it goes.
    engine
        .set_agent_enabled("primary", None, false)
        .await
        .expect("off");
    assert!(
        std::fs::read_to_string(&document)
            .unwrap()
            .contains("enabled: false")
    );
    assert!(row(&engine.agent_documents().await.unwrap(), "primary").disabled);
    let primary = engine
        .runtime_snapshot()
        .unwrap()
        .snapshot
        .agents
        .into_iter()
        .find(|agent| agent.id.as_str() == "primary")
        .unwrap();
    assert!(!primary.enabled);
    engine
        .set_agent_enabled("primary", None, true)
        .await
        .expect("on");
    assert!(!document.exists());

    // Off with no document starts one; replace copies the official one
    // whole over it; reset removes it.
    engine
        .set_agent_enabled("primary", None, false)
        .await
        .expect("off");
    assert!(document.exists());
    engine
        .replace_agent("primary", None)
        .await
        .expect("replaced");
    assert_eq!(
        std::fs::read_to_string(&document).unwrap(),
        OFFICIAL_PRIMARY
    );
    assert_eq!(
        row(&engine.agent_documents().await.unwrap(), "primary").origin,
        AgentDocumentOrigin::Replaced
    );
    engine.reset_agent("primary", None).await.expect("reset");
    assert!(!document.exists());
    assert!(
        engine.reset_agent("primary", None).await.is_err(),
        "nothing to reset"
    );

    // An ACP agent's document inherits the `_acp` template.
    engine
        .customize_agent("claude", None)
        .await
        .expect("claude");
    let claude = std::fs::read_to_string(root(&fixture).join("agents/claude.md")).unwrap();
    assert!(
        claude.starts_with("---\ninherit: ../official_agents/_acp.md\n"),
        "{claude}"
    );
    assert_eq!(
        row(&engine.agent_documents().await.unwrap(), "claude").origin,
        AgentDocumentOrigin::Customized
    );
    fixture.engine.shutdown().await;
}
