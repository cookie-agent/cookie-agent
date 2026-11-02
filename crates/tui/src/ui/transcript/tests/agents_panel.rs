//! The `/agents` panel: rows with where each document comes from and why
//! an agent cannot run, and Reset asking once before it runs.

use cookie_agent_protocol::{AgentDocumentInfo, AgentDocumentKind, AgentDocumentOrigin};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::app::*;
use crate::ui::slash::SlashCommand;

use super::support::*;

fn info(id: &str, kind: AgentDocumentKind, origin: AgentDocumentOrigin) -> AgentDocumentInfo {
    AgentDocumentInfo {
        id: id.into(),
        preset: None,
        kind,
        origin,
        inherits: None,
        path: (origin != AgentDocumentOrigin::Official
            && origin != AgentDocumentOrigin::FollowsTemplate)
            .then(|| format!("agents/{id}.md")),
        official_version: Some("1.2.0".into()),
        disabled: false,
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[tokio::test]
async fn the_panel_lists_documents_and_asks_before_a_reset() {
    let mut app = test_app().await;
    app.run_command(SlashCommand::Agents).await;
    assert_eq!(app.modal, Modal::AgentDocuments);
    app.agent_documents_panel.documents = Some(Ok(vec![
        info(
            "claude",
            AgentDocumentKind::Acp,
            AgentDocumentOrigin::FollowsTemplate,
        ),
        info(
            "primary",
            AgentDocumentKind::Agent,
            AgentDocumentOrigin::Customized,
        ),
    ]));
    let frame = rendered_frame(&mut app, 120, 40);
    assert!(
        frame.contains("primary  Official agents · customized"),
        "{frame}"
    );
    assert!(
        frame.contains("claude  ACP agents · follows _acp"),
        "{frame}"
    );
    // The official group comes first: `primary` is selected.
    app.handle_key(key(KeyCode::Char('x'))).await;
    assert_eq!(
        app.agent_documents_panel.confirm,
        Some((
            "primary".to_owned(),
            cookie_agent_client_state::agent_documents::AgentDocumentAction::Reset
        )),
        "the first x asks"
    );
    assert!(rendered_frame(&mut app, 120, 40).contains("x again to reset"));
    app.handle_key(key(KeyCode::Down)).await;
    assert!(
        app.agent_documents_panel.confirm.is_none(),
        "moving away forgets it"
    );
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.modal, Modal::None);
}
