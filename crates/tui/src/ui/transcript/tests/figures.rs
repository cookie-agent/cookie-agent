//! Shelves (`docs/site/specs/assets.md` section 9): a call that shows the
//! user an asset folds as any does, and the asset shows, with its caption
//! and a link into the web page, under the text that follows the call; the
//! link is a click target of its own.

use cookie_agent_protocol::{AssetBlock, AssetHandle, AssetId, AssetStore, DisplayBlock, ToolHint};

use crate::state::ToolStatus;
use crate::ui::transcript::BlockId;

use super::support::*;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

/// A finished `show_asset` of one HTML asset, captioned.
pub(super) fn figure_call() -> crate::state::SessionState {
    display_tool_state(
        hinted_presentation("Show Loss curves", ToolHint::FIGURE, None),
        ToolStatus::Completed,
        vec![
            DisplayBlock::text("Training loss over 40 epochs."),
            DisplayBlock::Asset(AssetBlock {
                store: AssetStore::Shared,
                asset: AssetId::new_v7(),
                handle: AssetHandle::new("asset_3f9a1c2e").expect("handle"),
                title: "Loss curves".into(),
                media_type: "text/html".into(),
                byte_length: 48 * 1024,
                sha256: "a".repeat(64),
                filename: Some("loss.html".into()),
            }),
        ],
    )
}

#[tokio::test]
async fn a_figures_asset_shows_on_a_shelf_with_its_caption_and_link() {
    let (mut app, session, _) = app_with_active_run().await;
    let figure = figure_call();
    let call_id = *figure.tools.keys().next().expect("the call");
    {
        let state = app.store.sessions.get_mut(&session).expect("session");
        state.transcript.extend(figure.transcript);
        state.tools.extend(figure.tools);
    }
    // Without a page to link to, it says where to see the asset.
    let frame = rendered_frame(&mut app, WIDTH, HEIGHT);
    assert!(frame.contains("From this reply"), "{frame}");
    assert!(frame.contains("Training loss over 40 epochs."), "{frame}");
    assert!(frame.contains("Loss curves"), "{frame}");
    assert!(frame.contains("loss.html"), "{frame}");
    assert!(
        frame.contains("Open the web client to see asset_3f9a1c2e"),
        "{frame}"
    );
    assert!(
        !app.hit_map
            .blocks
            .iter()
            .any(|hit| matches!(hit.id, BlockId::ToolAsset { .. })),
        "no link, no target"
    );

    // With one, the link names the session and the asset, and is a target
    // of its own. The call open lists the asset without a second link.
    app.page_url = Some("http://127.0.0.1:7419/".into());
    app.layout_cache = Default::default();
    let link = format!("http://127.0.0.1:7419/#session={session}&asset=asset_3f9a1c2e");
    for open in [false, true] {
        if open {
            app.expanded_blocks
                .entry(session)
                .or_default()
                .insert(BlockId::Tool(call_id));
        }
        let frame = rendered_frame(&mut app, WIDTH, HEIGHT);
        assert_eq!(frame.matches(&link).count(), 1, "open {open}: {frame}");
        let target = app
            .hit_map
            .blocks
            .iter()
            .find(|hit| matches!(hit.id, BlockId::ToolAsset { call_id: id, block: 1 } if id == call_id))
            .unwrap_or_else(|| panic!("open {open}: the link's target"));
        assert!(target.toggle_rect.is_some(), "a click row");
    }
}

/// The shelf goes under the first text after the call, not under the call.
#[tokio::test]
async fn a_shelf_shows_under_the_text_after_its_figure() {
    let (mut app, session, _) = app_with_active_run().await;
    let mut figure = figure_call();
    if let Some(crate::state::TranscriptItem::Assistant { children, .. }) =
        figure.transcript.first_mut()
    {
        children.push(crate::state::AssistantChild::Text {
            id: 77,
            version: 0,
            markdown: cookie_agent_client_state::markdown::MarkdownDocument::new(
                "The loss falls fastest in run 14.".to_owned(),
            ),
        });
    }
    {
        let state = app.store.sessions.get_mut(&session).expect("session");
        state.transcript.extend(figure.transcript);
        state.tools.extend(figure.tools);
    }
    let frame = rendered_frame(&mut app, WIDTH, HEIGHT);
    let text = frame.find("The loss falls fastest").expect("the text");
    let shelf = frame.find("From this reply").expect("the shelf");
    assert!(text < shelf, "{frame}");
}
