//! Attachments (`docs/site/specs/message-attachments.md` section 6): a
//! reply's `<attach …/>` tag is its file's name in the text, and under the
//! text a line says what it is, with a link into the web page that opens
//! it, or *Allow* and *Deny* while it waits for the user. Each is a click
//! target of its own.

use cookie_agent_protocol::{
    AttachSource, AttachTag, AttachmentOutcome, MimeType, PersistedFilePart, PersistedFileSource,
    Sha256Digest,
};

use crate::state::AssistantChild;
use crate::ui::transcript::BlockId;

use super::support::*;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

/// A committed reply that attaches `loss.html`, resolved as `outcome`.
pub(super) fn attachment_state(outcome: Option<AttachmentOutcome>) -> crate::state::SessionState {
    let mut state = assistant_state(vec![AssistantChild::Text {
        id: 11,
        version: 0,
        markdown: crate::markdown::MarkdownDocument::new(
            "The loss falls fastest in run 14:\n\n<attach src=\"runs/loss.html\" title=\"Loss curves\"/>\n".to_owned(),
        )
        .finished(),
    }]);
    state.text_parts.insert(11, (4, 0));
    if let Some(outcome) = outcome {
        state.attachments.entry((4, 0)).or_default().insert(
            0,
            cookie_agent_client_state::MessageAttachment {
                tag: AttachTag {
                    source: AttachSource::Path {
                        path: "runs/loss.html".into(),
                        machine: None,
                    },
                    title: Some("Loss curves".into()),
                },
                outcome,
            },
        );
    }
    state
}

pub(super) fn attached() -> AttachmentOutcome {
    AttachmentOutcome::Attached {
        file: PersistedFilePart {
            media_type: MimeType::new("text/html").unwrap(),
            filename: Some("loss.html".into()),
            source: PersistedFileSource::Artifact {
                byte_length: 48 * 1024,
                sha256: Sha256Digest::new("c".repeat(64)).unwrap(),
                reference: cookie_agent_protocol::ArtifactReference {
                    uri: format!("artifact://sha256/{}", "c".repeat(64)),
                },
            },
            metadata: None,
        },
    }
}

async fn app_showing(
    outcome: Option<AttachmentOutcome>,
) -> (crate::ui::App, cookie_agent_protocol::SessionId) {
    let (mut app, session, _) = app_with_active_run().await;
    let shown = attachment_state(outcome);
    let state = app.store.sessions.get_mut(&session).expect("session");
    state.transcript.extend(shown.transcript);
    state.text_parts.extend(shown.text_parts);
    state.attachments.extend(shown.attachments);
    (app, session)
}

#[tokio::test]
async fn an_attached_file_is_named_in_the_text_and_linked_under_it() {
    let (mut app, session) = app_showing(Some(attached())).await;
    // Without a page to link to, it says where to see the file.
    let frame = rendered_frame(&mut app, WIDTH, HEIGHT);
    // Named in the text, and on its line under it.
    assert_eq!(frame.matches("Loss curves").count(), 2, "{frame}");
    assert!(!frame.contains("<attach"), "{frame}");
    assert!(frame.contains("loss.html (text/html, 48 KB)"), "{frame}");
    assert!(frame.contains("Open the web client to see it"), "{frame}");
    assert!(
        !app.hit_map
            .blocks
            .iter()
            .any(|hit| matches!(hit.id, BlockId::AttachmentOpen(_))),
        "no link, no target"
    );

    // With one, the link names the session and the stored file.
    app.page_url = Some("http://127.0.0.1:7419/".into());
    app.layout_cache = Default::default();
    let frame = rendered_frame(&mut app, WIDTH, HEIGHT);
    // The link wraps at this width; it starts with the session.
    let link = format!("http://127.0.0.1:7419/#session={session}&attachment=cccc");
    assert!(frame.contains(&link), "{frame}");
    let target = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| matches!(hit.id, BlockId::AttachmentOpen(key) if key.ordinal == 0 && key.model_turn_seq == 4))
        .expect("the link's target");
    assert!(target.toggle_rect.is_some(), "a click row");
}

#[tokio::test]
async fn an_attachment_shows_while_it_attaches_waits_and_fails() {
    let (mut app, _) = app_showing(None).await;
    let frame = rendered_frame(&mut app, WIDTH, HEIGHT);
    assert!(frame.contains("attaching…"), "{frame}");

    let (mut app, _) = app_showing(Some(AttachmentOutcome::PendingApproval)).await;
    let frame = rendered_frame(&mut app, WIDTH, HEIGHT);
    assert!(
        frame.contains("✓ Allow") && frame.contains("✗ Deny"),
        "{frame}"
    );
    for allow in [true, false] {
        assert!(
            app.hit_map.blocks.iter().any(|hit| match hit.id {
                BlockId::AttachmentAllow(_) => allow,
                BlockId::AttachmentDeny(_) => !allow,
                _ => false,
            }),
            "allow {allow}: its target"
        );
    }

    let (mut app, _) = app_showing(Some(AttachmentOutcome::Failed {
        reason: "it does not exist".into(),
    }))
    .await;
    let frame = rendered_frame(&mut app, WIDTH, HEIGHT);
    assert!(frame.contains("not attached: it does not exist"), "{frame}");
}
