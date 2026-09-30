use crate::ui::transcript::*;

use cookie_agent_protocol::{ModelKey, ModelSelection, RunSelection, SessionId, VariantId};

use crate::markdown::MarkdownDocument;

use crate::state::{AssistantChild, ModelDisplayNames, RuntimeState};

use crate::ui::app::*;

use super::support::*;

fn runtime_with(
    digit: &str,
    providers: Vec<cookie_agent_protocol::ProviderDescriptor>,
    models: Vec<cookie_agent_protocol::AvailableModelDescriptor>,
) -> RuntimeState {
    let mut runtime = RuntimeState::default();
    runtime.install_initial(runtime_snapshot(
        digit,
        providers,
        models,
        vec![descriptor("primary", true)],
    ));
    runtime
}

fn named_runtime() -> RuntimeState {
    runtime_with("1", Vec::new(), vec![model_descriptor()])
}

fn key(value: &str) -> ModelKey {
    value.parse().expect("model key")
}

#[test]
fn display_names_come_from_runnable_and_unavailable_models_and_skip_id_echoes() {
    let mut echo = catalog_model("other/model-b", &[], None);
    echo.display_name = "model-b".into();
    let mut full_echo = catalog_model("other/model-c", &[], None);
    full_echo.display_name = "other/model-c".into();
    let mut blank = catalog_model("other/model-d", &[], None);
    blank.display_name = "  ".into();
    let runtime = runtime_with(
        "1",
        vec![unusable_provider(true)],
        vec![model_descriptor(), echo, full_echo, blank],
    );
    let names = runtime.model_names();

    assert_eq!(
        names.name(&key("gateway/arbitrary-model")),
        Some("Arbitrary Model")
    );
    // A model that dropped out of the runnable set keeps its name.
    assert_eq!(
        names.name(&key("kimi-code/k3-vision")),
        Some("Kimi K3 Vision")
    );
    // Names that only repeat the id, blank names, and unknown models all
    // label by the bare id.
    for id in [
        "kimi-code/k2",
        "other/model-b",
        "other/model-c",
        "other/model-d",
        "gone/retired-model",
    ] {
        assert_eq!(names.name(&key(id)), None, "{id}");
        assert_eq!(names.short_label(&key(id)), id);
        assert_eq!(names.long_label(&key(id)), id);
    }
    assert_eq!(
        names.short_label(&key("gateway/arbitrary-model")),
        "Arbitrary Model"
    );
    assert_eq!(
        names.long_label(&key("gateway/arbitrary-model")),
        "Arbitrary Model / gateway/arbitrary-model"
    );
}

#[test]
fn display_name_revision_moves_only_when_a_name_changes() {
    let mut runtime = named_runtime();
    let first = runtime.model_names().revision();
    assert_ne!(first, ModelDisplayNames::default().revision());

    // A newer snapshot with identical names keeps the revision, so cached
    // transcript layouts survive unrelated runtime changes.
    let baseline = runtime.revision().cloned();
    assert!(runtime.install_response(
        baseline.as_ref(),
        runtime_snapshot(
            "2",
            Vec::new(),
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        ),
    ));
    assert_eq!(runtime.model_names().revision(), first);

    let mut renamed = model_descriptor();
    renamed.display_name = "Renamed Model".into();
    let baseline = runtime.revision().cloned();
    assert!(runtime.install_response(
        baseline.as_ref(),
        runtime_snapshot(
            "3",
            Vec::new(),
            vec![renamed],
            vec![descriptor("primary", true)],
        ),
    ));
    assert_ne!(runtime.model_names().revision(), first);
    assert_eq!(
        runtime.model_names().name(&key("gateway/arbitrary-model")),
        Some("Renamed Model")
    );
}

#[test]
fn assistant_header_shows_name_and_id_or_falls_back_to_the_id() {
    let runtime = named_runtime();
    assert_eq!(
        attribution(Some("high")).header(runtime.model_names()),
        "primary • Arbitrary Model / gateway/arbitrary-model[high]"
    );
    assert_eq!(
        attribution(None).header(runtime.model_names()),
        "primary • Arbitrary Model / gateway/arbitrary-model[base]"
    );
    // Unknown (an old session replaying a retired model) and id-echo names
    // both render the bare id, never blank and never `id / id`.
    assert_eq!(
        attribution(None).header(&ModelDisplayNames::default()),
        "primary • gateway/arbitrary-model[base]"
    );
    let mut echo = model_descriptor();
    echo.display_name = "arbitrary-model".into();
    assert_eq!(
        attribution(None).header(runtime_with("1", Vec::new(), vec![echo]).model_names()),
        "primary • gateway/arbitrary-model[base]"
    );
}

fn switched_model_item() -> TranscriptItem {
    TranscriptItem::Assistant {
        id: 1,
        version: 0,
        attribution: attribution(None),
        committed_turn_seq: Some(2),
        child_times: Vec::new(),
        children: vec![
            AssistantChild::Text {
                id: 10,
                version: 0,
                markdown: MarkdownDocument::new("before".into()),
            },
            AssistantChild::Attribution {
                resolved_model: resolved_model(Some("high")),
            },
            AssistantChild::Text {
                id: 11,
                version: 0,
                markdown: MarkdownDocument::new("after".into()),
            },
        ],
    }
}

#[test]
fn now_using_notice_and_continuation_header_share_the_header_label() {
    let runtime = named_runtime();
    let theme = Theme::default();
    let item = switched_model_item();
    let state = SessionState {
        transcript: vec![item.clone()],
        ..SessionState::default()
    };
    let highlighter = crate::markdown::SyntectHighlighter::default();
    let mut cache = LayoutCache::default();
    ensure_cached_transcript_layout(
        &mut cache,
        SessionId::new_v7(),
        &state,
        None,
        None,
        100,
        &theme,
        &highlighter,
        crate::state::EventLevel::Debug,
        0,
        runtime.model_names(),
    );
    let rendered = snapshot_lines(&cache.layout.lines);
    assert!(
        rendered.contains("╭─ primary • Arbitrary Model / gateway/arbitrary-model[base]"),
        "{rendered}"
    );
    assert!(
        rendered.contains("├─ now using Arbitrary Model / gateway/arbitrary-model[high]"),
        "{rendered}"
    );

    // Resuming after the switch names the model in effect there.
    let resumed = snapshot_lines(&continuation_header(
        &item,
        2,
        100,
        &theme,
        runtime.model_names(),
    ));
    assert!(
        resumed.contains("╭─ primary • Arbitrary Model / gateway/arbitrary-model[high]"),
        "{resumed}"
    );
    let unnamed = snapshot_lines(&continuation_header(
        &item,
        2,
        100,
        &theme,
        &ModelDisplayNames::default(),
    ));
    assert!(
        unnamed.contains("╭─ primary • gateway/arbitrary-model[high]"),
        "{unnamed}"
    );
}

#[test]
fn a_name_arriving_later_relabels_cached_assistant_headers_only() {
    let mut state = assistant_state(vec![AssistantChild::Text {
        id: 10,
        version: 0,
        markdown: MarkdownDocument::new("answer".into()),
    }]);
    state.transcript.insert(
        0,
        TranscriptItem::User {
            id: 99,
            version: 0,
            text: "question".into(),
            seq: 1,
        },
    );
    let session = SessionId::new_v7();
    let theme = Theme::default();
    let highlighter = crate::markdown::SyntectHighlighter::default();
    let mut cache = LayoutCache::default();
    let layout = |cache: &mut LayoutCache, names: &ModelDisplayNames| {
        let unchanged = ensure_cached_transcript_layout(
            cache,
            session,
            &state,
            None,
            None,
            80,
            &theme,
            &highlighter,
            crate::state::EventLevel::Debug,
            0,
            names,
        );
        (unchanged, snapshot_lines(&cache.layout.lines))
    };

    let (_, before) = layout(&mut cache, &ModelDisplayNames::default());
    assert!(
        before.contains("╭─ primary • gateway/arbitrary-model[base]"),
        "{before}"
    );
    let item_passes = cache.item_layout_passes;
    let part_passes = cache.assistant_part_layout_passes;

    // The runtime snapshot delivers the name after the first layout.
    let runtime = named_runtime();
    let (unchanged, after) = layout(&mut cache, runtime.model_names());
    assert!(!unchanged);
    assert!(
        after.contains("╭─ primary • Arbitrary Model / gateway/arbitrary-model[base]"),
        "{after}"
    );
    // Only the assistant item relayouts; its prose part is reused.
    assert_eq!(cache.item_layout_passes, item_passes + 1);
    assert_eq!(cache.assistant_part_layout_passes, part_passes);

    // Same names on the next frame: a pure cache hit.
    let item_passes = cache.item_layout_passes;
    let (unchanged, _) = layout(&mut cache, runtime.model_names());
    assert!(unchanged);
    assert_eq!(cache.item_layout_passes, item_passes);
}

#[tokio::test]
async fn composer_title_shows_display_name_with_intact_hit_regions() {
    let mut app = test_app().await;
    app.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_key(),
            variant: Some(VariantId::new("high").expect("variant")),
        },
        preset: None,
    });
    let rows = frame_rows(&mut app, 100, 30);
    let rendered = rows.join("\n");
    assert!(
        rendered.contains("╭ primary • Arbitrary Model[high] "),
        "{rendered}"
    );
    let rect = |app: &App, segment: TitleSegment| {
        app.hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == segment)
            .expect("title segment")
            .rect
    };
    assert_eq!(rect_text(&rows, rect(&app, TitleSegment::Agent)), "primary");
    assert_eq!(
        rect_text(&rows, rect(&app, TitleSegment::Model)),
        "Arbitrary Model"
    );
    assert_eq!(
        rect_text(&rows, rect(&app, TitleSegment::Variant)),
        "[high]"
    );
    let model = rect(&app, TitleSegment::Model);
    app.handle_click(model.x, model.y).await;
    assert_eq!(app.modal, Modal::Models);
    app.modal = Modal::None;

    // A selection the runtime has no distinct name for keeps its id.
    let mut echo = model_descriptor();
    echo.display_name = "arbitrary-model".into();
    app.runtime = RuntimeState::default();
    app.install_initial_runtime(runtime_snapshot(
        "1",
        Vec::new(),
        vec![echo],
        vec![descriptor("primary", true)],
    ));
    let rows = frame_rows(&mut app, 100, 30);
    assert!(
        rows.join("\n")
            .contains("╭ primary • gateway/arbitrary-model[high] ")
    );
    assert_eq!(
        rect_text(&rows, rect(&app, TitleSegment::Model)),
        "gateway/arbitrary-model"
    );
}
