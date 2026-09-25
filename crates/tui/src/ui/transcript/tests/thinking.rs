use std::time::Duration;

use crate::ui::transcript::*;

use cookie_agent_protocol::{AttemptId, SessionId};

use crate::state::{AssistantChild, StateStore};
use crate::theme::{ColorLevel, ThemeKind};

use super::support::*;

#[test]
fn merged_thinking_children_have_distinct_regions_and_collapse_state() {
    let state = assistant_state(vec![
        AssistantChild::Thinking {
            id: 10,
            version: 0,
            text: "first thought".into(),
        },
        AssistantChild::Thinking {
            id: 20,
            version: 0,
            text: "second thought".into(),
        },
    ]);
    let expanded = HashSet::from([BlockId::Thinking(10)]);
    let layout = transcript_layout(&state, Some(&expanded), 60);
    assert_eq!(
        layout
            .regions
            .iter()
            .map(|region| region.id)
            .collect::<Vec<_>>(),
        vec![BlockId::Thinking(10), BlockId::Thinking(20)]
    );
    let rendered = snapshot_lines(&layout.lines);
    assert!(rendered.contains("💭 ▾ thought"));
    assert!(rendered.contains("first thought"));
    assert!(rendered.contains("💭 ▸ thought"));
    assert!(!rendered.contains("second thought"));
}

#[test]
fn streaming_thinking_header_animates_its_ellipsis_with_the_clock() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let mut store = StateStore::default();
    for event in [
        attempt_started(session, 1, run, attempt, None),
        reasoning_delta(session, 2, run, attempt, "pondering"),
    ] {
        assert!(store.apply_event(event));
    }
    let state = &store.sessions[&session];
    assert!(state.has_open_thinking());
    let mut cache = LayoutCache::default();
    let mut previous = String::new();
    for (bucket, expected) in ["thinking", "thinking.", "thinking..", "thinking..."]
        .iter()
        .enumerate()
    {
        ensure_cached_transcript_layout(
            &mut cache,
            session,
            state,
            None,
            None,
            60,
            &Theme::default(),
            &crate::markdown::SyntectHighlighter::default(),
            crate::state::EventLevel::Debug,
            u8::try_from(bucket).expect("bucket"),
        );
        let rendered = snapshot_lines(&cache.layout.lines);
        assert!(
            rendered.contains(&format!("💭 ▸ {expected} ")),
            "bucket {bucket}: {rendered}"
        );
        if bucket > 0 {
            // Each clock bucket invalidates the cached label in place:
            // no transcript mutation is needed to advance the ellipsis.
            assert_ne!(rendered, previous, "bucket {bucket}");
        }
        previous = rendered;
    }
}

#[test]
fn sealed_thinking_header_reports_the_recorded_duration() {
    let mut state = assistant_state(vec![AssistantChild::Thinking {
        id: 7,
        version: 0,
        text: "pondered".into(),
    }]);
    state
        .thinking_durations
        .insert((1, 7), Duration::from_secs(95));
    let collapsed = snapshot_lines(&transcript_layout(&state, None, 60).lines);
    assert!(collapsed.contains("💭 ▸ thought for 1m 35s"), "{collapsed}");
    let expanded = HashSet::from([BlockId::Thinking(7)]);
    let expanded = snapshot_lines(&transcript_layout(&state, Some(&expanded), 60).lines);
    assert!(expanded.contains("💭 ▾ thought for 1m 35s"), "{expanded}");

    // Sub-second streams settle to the plain label: "thought for 0s"
    // would read as noise.
    state
        .thinking_durations
        .insert((1, 7), Duration::from_millis(400));
    let rendered = snapshot_lines(&transcript_layout(&state, None, 60).lines);
    assert!(rendered.contains("💭 ▸ thought "), "{rendered}");
    assert!(!rendered.contains("thought for"), "{rendered}");
}

#[tokio::test]
async fn thinking_clock_cycles_buckets_only_while_thinking_streams() {
    let mut app = test_app().await;
    assert!(!app.animation_active());

    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    for event in [
        session_created(session, 1),
        attempt_started(session, 2, run, attempt, None),
        reasoning_delta(session, 3, run, attempt, "pondering"),
    ] {
        assert!(app.store.apply_event(event));
    }
    app.selected = Some(session);
    assert!(app.animation_active());

    // Twelve 33ms frames per step ≈ 400ms per ellipsis dot, wrapping
    // after "thinking..." back to the bare label.
    assert_eq!(app.clock_bucket(), 0);
    for expected in [1, 2, 3, 0] {
        for _ in 0..12 {
            app.animation_tick();
        }
        assert_eq!(app.clock_bucket(), expected);
    }

    // Sealing the part (any other part opening, or a commit) stops the
    // animation; the UI is event-driven again.
    assert!(
        app.store
            .apply_event(text_delta(session, 4, run, attempt, "answer"))
    );
    assert!(!app.animation_active());
}

#[test]
fn thinking_reads_as_muted_text_collapsed_and_expanded() {
    let state = assistant_state(vec![AssistantChild::Thinking {
        id: 10,
        version: 0,
        text: "a thought".into(),
    }]);
    let theme = Theme::default();
    let header_style = |expanded: Option<&HashSet<BlockId>>| {
        transcript_layout(&state, expanded, 60)
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains("💭"))
            .expect("thinking header")
            .style
    };
    assert_eq!(header_style(None).fg, theme.muted_text().fg);
    let expanded = HashSet::from([BlockId::Thinking(10)]);
    assert_eq!(header_style(Some(&expanded)).fg, theme.muted_text().fg);
    // Expanded, the text is muted italics inset on the grey output band
    // under a title-band header, with no dashed `┆` marker anywhere.
    let layout = transcript_layout(&state, Some(&expanded), 60);
    let rows = layout
        .lines
        .iter()
        .map(|line| line.to_string().trim_end().to_owned())
        .collect::<Vec<_>>();
    assert!(rows.iter().all(|row| !row.contains('┆')), "{rows:?}");
    let header = rows.iter().position(|row| row.contains("💭")).unwrap();
    assert_eq!(rows[header + 1], "│", "{rows:?}");
    assert_eq!(rows[header + 2], "│  a thought", "{rows:?}");
    assert_eq!(rows[header + 3], "│", "{rows:?}");
    assert_eq!(rows[header + 4], "│", "{rows:?}");
    let text = &layout.lines[header + 2];
    let span = text
        .spans
        .iter()
        .find(|span| span.content.contains("a thought"))
        .unwrap();
    assert_eq!(span.style.fg, theme.muted_text().fg, "{span:?}");
    assert!(
        span.style
            .add_modifier
            .contains(ratatui::style::Modifier::ITALIC)
    );
    assert_eq!(span.style.bg, theme.terminal_background());
    assert!(
        layout.lines[header]
            .spans
            .iter()
            .skip(1)
            .all(|span| span.style.bg == theme.tool_title_background()),
        "{:?}",
        layout.lines[header]
    );
    // The padding column is not copied with the text.
    assert_eq!(
        super::super::wrap::extract_line(text, 0, u16::MAX, &theme).as_deref(),
        Some("a thought")
    );
    // A theme only changes colours: mono keeps the same panel layout, and
    // copying still skips the uncoloured margin.
    let mono = Theme::new(ThemeKind::Mono, ColorLevel::None);
    let mono_layout = transcript_layout_with(
        &state,
        Some(&expanded),
        60,
        &mono,
        &crate::markdown::PlainHighlighter,
    );
    let mono_rows = mono_layout
        .lines
        .iter()
        .map(|line| line.to_string().trim_end().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(mono_rows, rows);
    assert_eq!(
        super::super::wrap::extract_line(&mono_layout.lines[header + 2], 0, u16::MAX, &mono)
            .as_deref(),
        Some("a thought")
    );
}
