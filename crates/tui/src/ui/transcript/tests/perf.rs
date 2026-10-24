//! Render-path timings over a synthetic long session: replay, first draw,
//! unchanged redraws, a streaming text delta, a tool progress delta, an
//! animation step with a running tool, and a resize — with and without
//! descendant warnings spliced into the view — and what parsing its markdown
//! from source costs. Run in release:
//!
//! ```sh
//! cargo test --release -p cookie_agent_tui transcript_perf -- --ignored --nocapture
//! ```
//!
//! Optional: `PERF_RUNS` (150 runs ≈ 15k transcript lines, 300 code blocks)
//! and `PERF_REPEAT` (30 samples per measurement).

use std::time::{Duration, Instant};

use cookie_agent_protocol::{
    AttemptId, EventPayload, RunId, SafeDisplayText, SessionId, SessionTree, StoredEvent,
    StreamedOutputKind, ToolCallId, ToolTerminationOutcome,
};
use jiff::Timestamp;
use ratatui::{Terminal, backend::TestBackend};

use crate::ui::app::App;

use super::support::*;

const WIDTH: u16 = 160;
const HEIGHT: u16 = 50;
const CHILDREN: usize = 6;

fn at(mut event: StoredEvent, second: i64) -> StoredEvent {
    event.timestamp = Timestamp::new(second, 0).expect("timestamp");
    event
}

fn code_block(run: usize, index: usize) -> String {
    let mut code = format!("```rust\n// run {run} block {index}\n");
    for line in 0..24 {
        code.push_str(&format!(
            "    let value_{line} = compute(\"{run}-{index}-{line}\", {line}) * {index};\n"
        ));
    }
    code.push_str("```\n");
    code
}

fn reply_markdown(run: usize) -> String {
    let mut text = String::new();
    for index in 0..2 {
        text.push_str(&format!(
            "Paragraph {index} of run {run}: the transcript renders this prose with **bold** \
             text, `inline code`, and enough words to wrap across a couple of rows at the \
             benchmark width so wrapping costs are realistic.\n\n- first item {index}\n- second \
             item with a longer body that wraps {index}\n\n"
        ));
        text.push_str(&code_block(run, index));
        text.push('\n');
    }
    text
}

struct Seq(u64);

impl Seq {
    fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// One model turn of `run_id` — reasoning, a reply with two code blocks, and
/// one tool call that runs to completion — committed `base` seconds in.
fn push_turn(
    events: &mut Vec<StoredEvent>,
    root: SessionId,
    seq: &mut Seq,
    run_id: RunId,
    base: i64,
    turn: usize,
) {
    let attempt = AttemptId::new_v7();
    let call = ToolCallId::new_v7();
    let call_name = format!("call-{turn}");
    events.push(at(
        attempt_started(root, seq.next(), run_id, attempt, None),
        base + 1,
    ));
    let commit = seq.next();
    events.push(at(
        turn_committed(
            root,
            commit,
            run_id,
            attempt,
            commit,
            vec![
                reasoning_part(&format!(
                    "thinking about turn {turn}\nconsidering options\nchoosing one"
                )),
                text_part(&reply_markdown(turn)),
                tool_part(&call_name),
            ],
            Vec::new(),
            None,
        ),
        base + 10,
    ));
    events.push(at(
        tool_started_at(
            root,
            seq.next(),
            run_id,
            call,
            commit,
            &call_name,
            2,
            &call_name,
            Some("cargo test"),
        ),
        base + 11,
    ));
    events.push(at(
        tool_terminated(
            root,
            seq.next(),
            run_id,
            call,
            commit,
            &call_name,
            ToolTerminationOutcome::Completed,
        ),
        base + 30,
    ));
}

fn user_input(root: SessionId, seq: &mut Seq, run_id: RunId, base: i64, text: &str) -> StoredEvent {
    at(
        event(
            root,
            seq.next(),
            run_id,
            EventPayload::UserInputSubmitted {
                content: text.into(),
            },
        ),
        base,
    )
}

/// `runs` finished runs of one turn each, then one live run that has
/// already committed `live_turns` turns into a single assistant block. Runs
/// are numbered from `first_run`, which makes their code distinct.
fn root_history(
    root: SessionId,
    runs: usize,
    live_turns: usize,
    live_run: RunId,
    seq: &mut Seq,
    first_run: usize,
) -> Vec<StoredEvent> {
    let mut events = vec![at(session_created(root, seq.next()), 0)];
    for run in 0..runs {
        let base = 1_000 + (run as i64) * 100;
        let run_id = RunId::new_v7();
        events.push(user_input(
            root,
            seq,
            run_id,
            base,
            &format!("Please do step {run}"),
        ));
        push_turn(&mut events, root, seq, run_id, base, first_run + run);
        events.push(at(
            event(
                root,
                seq.next(),
                run_id,
                EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Completed {
                    final_text: None,
                }),
            ),
            base + 41,
        ));
    }
    let base = 1_000 + (runs as i64) * 100;
    events.push(user_input(root, seq, live_run, base, "one long live run"));
    for turn in 0..live_turns {
        push_turn(
            &mut events,
            root,
            seq,
            live_run,
            base + 40 * turn as i64,
            first_run + runs + turn,
        );
    }
    events
}

/// One child session per `CHILDREN`, each committing turns with warnings at
/// times interleaved with the parent's runs.
fn child_history(child: SessionId, index: usize, runs: usize) -> Vec<StoredEvent> {
    let mut seq = Seq(0);
    let run_id = RunId::new_v7();
    let mut events = Vec::new();
    for warning in 0..4 {
        let run = (index * 4 + warning) * runs / (CHILDREN * 4);
        let time = 1_000 + (run as i64) * 100 + 20;
        let attempt = AttemptId::new_v7();
        events.push(at(
            attempt_started(child, seq.next(), run_id, attempt, None),
            time,
        ));
        let commit = seq.next();
        events.push(at(
            turn_committed(
                child,
                commit,
                run_id,
                attempt,
                commit,
                vec![text_part("child output")],
                vec![&format!("child {index} provider hiccup {warning}")],
                None,
            ),
            time + 1,
        ));
    }
    events
}

fn draw(terminal: &mut Terminal<TestBackend>, app: &mut App) -> Duration {
    let start = Instant::now();
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("draw");
    start.elapsed()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn report(label: &str, samples: Vec<Duration>, app: &App, passes_before: (u64, u64)) {
    let count = samples.len() as u64;
    let median = median(samples);
    let items = app.layout_cache.item_layout_passes - passes_before.0;
    let parts = app.layout_cache.assistant_part_layout_passes - passes_before.1;
    eprintln!(
        "  {label:<34} median {:>9.3} ms   item layouts/frame {:>6.1}   part layouts/frame {:>6.1}",
        median.as_secs_f64() * 1000.0,
        items as f64 / count as f64,
        parts as f64 / count as f64,
    );
}

fn passes(app: &App) -> (u64, u64) {
    (
        app.layout_cache.item_layout_passes,
        app.layout_cache.assistant_part_layout_passes,
    )
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

async fn scenario(with_descendants: bool) {
    let runs: usize = env_or("PERF_RUNS", 150);
    let live_turns: usize = env_or("PERF_LIVE_TURNS", 30);
    let repeat: usize = env_or("PERF_REPEAT", 30);
    let root = SessionId::new_v7();
    let children = (0..CHILDREN)
        .map(|_| SessionId::new_v7())
        .collect::<Vec<_>>();
    let mut app = test_app().await;
    if with_descendants {
        app.tree = Some(SessionTree {
            session: titled_meta(root, "root session", 1),
            children: children
                .iter()
                .enumerate()
                .map(|(index, child)| SessionTree {
                    session: titled_meta(*child, &format!("child {index}"), 1),
                    children: Vec::new(),
                })
                .collect(),
        });
    }
    app.tree_root = Some(root);
    app.selected = Some(root);

    let run_id = RunId::new_v7();
    let mut seq = Seq(0);
    let history = root_history(root, runs, live_turns, run_id, &mut seq, 0);
    let start = Instant::now();
    assert!(app.store.rebuild_session(root, 0, history));
    if with_descendants {
        for (index, child) in children.iter().enumerate() {
            assert!(
                app.store
                    .rebuild_session(*child, 0, child_history(*child, index, runs))
            );
        }
    }
    let replay = start.elapsed();

    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
    let first = draw(&mut terminal, &mut app);
    let state = &app.store.sessions[&root];
    eprintln!(
        "scenario: descendants={with_descendants} runs={runs} live turns={live_turns} items={} lines={} descendant rows={}",
        state.transcript.len(),
        app.layout_cache.layout.lines.len(),
        app.descendant_events(root, crate::state::EventLevel::Warning)
            .len(),
    );
    eprintln!(
        "  {:<34} {:>9.3} ms",
        "replay",
        replay.as_secs_f64() * 1000.0
    );
    eprintln!(
        "  {:<34} {:>9.3} ms",
        "first draw",
        first.as_secs_f64() * 1000.0
    );

    let before = passes(&app);
    let samples = (0..repeat).map(|_| draw(&mut terminal, &mut app)).collect();
    report("unchanged redraw", samples, &app, before);

    // The layout cache's own check of an unchanged transcript, apart from
    // drawing.
    let before = passes(&app);
    let width = app.layout_cache.key.expect("laid out").width;
    let samples = (0..repeat)
        .map(|_| {
            let clock = app.clock_bucket();
            let start = Instant::now();
            assert!(crate::ui::transcript::ensure_cached_transcript_layout(
                &mut app.layout_cache,
                root,
                &app.store.sessions[&root],
                None,
                app.expanded_blocks.get(&root),
                width,
                &app.theme,
                app.highlighter.as_ref(),
                app.tui_config.minimum_event_level,
                clock,
                app.runtime.model_names(),
            ));
            start.elapsed()
        })
        .collect();
    report("unchanged layout check", samples, &app, before);

    // The live run streams a long reply with an open code block into its
    // already long assistant block.
    let base = 1_000 + (runs as i64) * 100 + 40 * live_turns as i64;
    let attempt = AttemptId::new_v7();
    assert!(app.store.apply_event(at(
        attempt_started(root, seq.next(), run_id, attempt, None),
        base + 1
    )));
    assert!(app.store.apply_event(at(
        event(
            root,
            seq.next(),
            run_id,
            EventPayload::ModelOutputStarted {
                attempt_id: attempt,
                kind: StreamedOutputKind::Text,
            },
        ),
        base + 2,
    )));
    let mut streamed = (0..8)
        .map(|index| reply_markdown(runs + index))
        .collect::<String>();
    streamed.push_str("```rust\n");
    assert!(app.store.apply_event(at(
        text_delta(root, seq.0, run_id, attempt, &streamed),
        base + 2
    )));
    draw(&mut terminal, &mut app);
    let before = passes(&app);
    let samples = (0..repeat)
        .map(|index| {
            let delta = format!("    let streamed_{index} = more(\"{index}\");\n");
            let start = Instant::now();
            assert!(app.store.apply_event(at(
                text_delta(root, seq.0, run_id, attempt, &delta),
                base + 3
            )));
            terminal
                .draw(|frame| app.draw_for_test(frame))
                .expect("draw");
            start.elapsed()
        })
        .collect();
    report("text delta + redraw", samples, &app, before);

    // The turn commits with a tool call that keeps running.
    let commit = seq.next();
    let call = ToolCallId::new_v7();
    assert!(app.store.apply_event(at(
        turn_committed(
            root,
            commit,
            run_id,
            attempt,
            commit,
            vec![text_part(&streamed), tool_part("running-call")],
            Vec::new(),
            None,
        ),
        base + 10,
    )));
    assert!(app.store.apply_event(at(
        tool_started_at(
            root,
            seq.next(),
            run_id,
            call,
            commit,
            "running-call",
            1,
            "running-call",
            Some("cargo build"),
        ),
        base + 11,
    )));
    draw(&mut terminal, &mut app);
    let before = passes(&app);
    let samples = (0..repeat)
        .map(|index| {
            let start = Instant::now();
            assert!(
                app.store.apply_event(at(
                    event(
                        root,
                        seq.0,
                        run_id,
                        EventPayload::ToolCallProgress {
                            tool_call_id: call,
                            message: SafeDisplayText::new("bash stdout").expect("message"),
                            display: Some(
                                cookie_agent_protocol::DisplayDelta::append(
                                    0,
                                    format!("   Compiling crate-{index} v0.1.0\n")
                                )
                                .into()
                            ),
                        },
                    ),
                    base + 12,
                ))
            );
            terminal
                .draw(|frame| app.draw_for_test(frame))
                .expect("draw");
            start.elapsed()
        })
        .collect();
    report("tool progress + redraw", samples, &app, before);

    let before = passes(&app);
    let samples = (0..repeat)
        .map(|_| {
            app.animation_ticks += 12;
            draw(&mut terminal, &mut app)
        })
        .collect();
    report("animation step (running tool)", samples, &app, before);

    let before = passes(&app);
    let samples = (0..repeat).map(|_| draw(&mut terminal, &mut app)).collect();
    report("unchanged redraw (running tool)", samples, &app, before);

    let before = passes(&app);
    let samples = (0..repeat.min(10))
        .map(|index| {
            let width = if index % 2 == 0 { WIDTH - 10 } else { WIDTH };
            terminal.backend_mut().resize(width, HEIGHT);
            terminal.autoresize().expect("resize");
            draw(&mut terminal, &mut app)
        })
        .collect();
    report("resize + redraw", samples, &app, before);

    // Viewing another session and coming back.
    let other = SessionId::new_v7();
    assert!(
        app.store
            .rebuild_session(other, 0, vec![at(session_created(other, 1), 0)])
    );
    let before = passes(&app);
    let samples = (0..repeat.min(10))
        .map(|_| {
            app.selected = Some(other);
            draw(&mut terminal, &mut app);
            app.selected = Some(root);
            draw(&mut terminal, &mut app)
        })
        .collect();
    report("switch back + redraw", samples, &app, before);

    // Scrolling up a page at a time from the bottom.
    let before = passes(&app);
    let samples = (0..repeat.min(20))
        .map(|_| {
            app.conversation_scroll.up(usize::from(HEIGHT));
            draw(&mut terminal, &mut app)
        })
        .collect();
    report("page up + redraw", samples, &app, before);
    app.conversation_scroll.bottom();

    // A long session never viewed before, with code of its own.
    let before = passes(&app);
    let samples = (0..repeat.min(4))
        .map(|index| {
            let unseen = SessionId::new_v7();
            let history = root_history(
                unseen,
                runs,
                live_turns,
                RunId::new_v7(),
                &mut Seq(0),
                (index + 1) * 10_000,
            );
            assert!(app.store.rebuild_session(unseen, 0, history));
            app.selected = Some(unseen);
            draw(&mut terminal, &mut app)
        })
        .collect();
    report("switch to unseen + redraw", samples, &app, before);
}

/// What keeping only the markdown source in the shared state would add:
/// parsing every reply of the long session again, as a client rebuilding a
/// view with no parse at hand must, and a streamed reply parsed whole after
/// every delta instead of only its open tail.
fn markdown_parse_cost() {
    use crate::markdown::MarkdownDocument;

    let runs: usize = env_or("PERF_RUNS", 150);
    let repeat: usize = env_or("PERF_REPEAT", 30);
    let replies = (0..runs).map(reply_markdown).collect::<Vec<_>>();
    let bytes = replies.iter().map(String::len).sum::<usize>();
    let samples = (0..repeat)
        .map(|_| {
            let start = Instant::now();
            for reply in &replies {
                std::hint::black_box(MarkdownDocument::new(reply.clone()).finished());
            }
            start.elapsed()
        })
        .collect::<Vec<_>>();
    eprintln!(
        "markdown: {runs} replies, {bytes} bytes\n  parse every reply                  median {:>9.3} ms",
        median(samples).as_secs_f64() * 1000.0
    );
    let reply = (0..8).map(reply_markdown).collect::<String>();
    let deltas = reply
        .as_bytes()
        .chunks(24)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>();
    let incremental = (0..repeat)
        .map(|_| {
            let start = Instant::now();
            let mut document = MarkdownDocument::new(String::new());
            for delta in &deltas {
                document.append(delta);
            }
            std::hint::black_box(document);
            start.elapsed()
        })
        .collect::<Vec<_>>();
    let whole = (0..repeat)
        .map(|_| {
            let start = Instant::now();
            let mut source = String::new();
            for delta in &deltas {
                source.push_str(delta);
                std::hint::black_box(MarkdownDocument::new(source.clone()));
            }
            start.elapsed()
        })
        .collect::<Vec<_>>();
    eprintln!(
        "  stream {} deltas, open tail        median {:>9.3} ms\n  stream {} deltas, whole reply     median {:>9.3} ms",
        deltas.len(),
        median(incremental).as_secs_f64() * 1000.0,
        deltas.len(),
        median(whole).as_secs_f64() * 1000.0
    );
    // One long code block streaming in a line at a time stays open the
    // whole time.
    let code_lines = 5_000;
    let fence = (0..repeat.min(5))
        .map(|_| {
            let start = Instant::now();
            let mut document = MarkdownDocument::new("Here it is:\n\n```rust\n".to_owned());
            for line in 0..code_lines {
                document.append(&format!("    let value_{line} = compute({line});\n"));
            }
            std::hint::black_box(document);
            start.elapsed()
        })
        .collect::<Vec<_>>();
    eprintln!(
        "  stream a {code_lines}-line code block     median {:>9.3} ms",
        median(fence).as_secs_f64() * 1000.0
    );
}

#[tokio::test]
#[ignore = "manual performance measurement; run in release"]
async fn transcript_perf() {
    let start = Instant::now();
    let syntaxes = syntect::parsing::SyntaxSet::load_defaults_newlines();
    let themes = syntect::highlighting::ThemeSet::load_defaults();
    eprintln!(
        "syntax assets load: {:.3} ms ({} syntaxes, {} themes)",
        start.elapsed().as_secs_f64() * 1000.0,
        syntaxes.syntaxes().len(),
        themes.themes.len()
    );
    scenario(false).await;
    scenario(true).await;
    markdown_parse_cost();
}
