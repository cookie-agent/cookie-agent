//! What an escalated approval's review costs, the reviewer's own log against
//! today's render of the whole log (`docs/site/specs/forever-sessions.md`
//! section 12, phase 8). Run only when asked, on a log `forever_measure
//! generate` wrote:
//!
//! ```sh
//! COOKIE_REVIEWER_MEASURE_LOG=<dir>/data/sessions/<month>/<id>/events.jsonl \
//!     cargo test --release -p cookie_agent_engine --lib measure_a_review -- --ignored --nocapture
//! ```
//!
//! Each review of the log is given a verdict, so the reviewer's log holds a
//! turn per review as a working reviewer's would, then the last review is
//! timed both ways.

use std::{path::PathBuf, sync::Arc, time::Instant};

use cookie_agent_protocol::{EventPayload, ReviewerTurn, SessionId, StoredEvent};

use crate::runtime::approval::context::{
    Bounds, Verdict, render,
    thread::{self, Review},
};

fn json_bytes(events: &[Arc<StoredEvent>]) -> usize {
    events
        .iter()
        .map(|event| serde_json::to_vec(&**event).map_or(0, |bytes| bytes.len()))
        .sum()
}

#[test]
#[ignore = "measures a log forever_measure generated; run with COOKIE_REVIEWER_MEASURE_LOG"]
fn measure_a_review() {
    let path = PathBuf::from(
        std::env::var_os("COOKIE_REVIEWER_MEASURE_LOG").expect("COOKIE_REVIEWER_MEASURE_LOG"),
    );
    let session = path
        .parent()
        .and_then(|directory| directory.file_name())
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse::<SessionId>().ok())
        .expect("a log's directory names its session");
    crate::session::skip_reference_fold();
    let log = crate::events::EventLog::open_read_only(path.clone(), session).expect("the log");
    let whole = log
        .history()
        .expect("a pass")
        .collect::<Result<Vec<_>, _>>()
        .expect("the log");
    let requests = whole
        .iter()
        .enumerate()
        .filter_map(|(position, event)| match &event.payload {
            EventPayload::ApprovalRequested { request } => Some((position, request.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let bounds = Bounds::new(None, None, &"policy ".repeat(500));
    let verdict = Verdict {
        decision: cookie_agent_protocol::ApprovalInternalDecisionKind::Allow,
        reason: Some("Routine.".into()),
    };
    // The reviewer's log after every review but the last.
    let mut kept = Vec::<ReviewerTurn>::new();
    let mut turns = 0;
    let mut epochs = 0;
    let call_of = |before: &[Arc<StoredEvent>],
                   request: &cookie_agent_protocol::ApprovalRequest| {
        before.iter().rev().find_map(|event| match &event.payload {
            EventPayload::ToolCallStarted { start }
                if start.tool_call_id == request.tool_call_id() =>
            {
                Some(thread::call_facts(
                    event,
                    thread::turn_arguments(before, &start.owner),
                ))
            }
            _ => None,
        })
    };
    let prepare = |kept: &[ReviewerTurn], position: usize, events: Vec<Arc<StoredEvent>>| {
        let (_, request) = &requests[position];
        let before = &whole[..=requests[position].0];
        let lookup = |owner: &cookie_agent_protocol::AssistantToolCallRef| {
            thread::turn_arguments(before, owner)
        };
        let call = call_of(before, request);
        Review {
            thread: kept,
            unrecorded: None,
            inherited: None,
            request,
            cut: whole[requests[position].0].seq,
            call: call.as_ref(),
            turns: Some(&lookup),
            bounds,
        }
        .prepare(events)
    };
    for position in 0..requests.len() - 1 {
        let from = kept.last().map_or(0, |last| last.cut_seq);
        let cut = whole[requests[position].0].seq;
        let delta = whole
            .iter()
            .filter(|event| event.seq > from && event.seq <= cut)
            .cloned()
            .collect();
        let turn = prepare(&kept, position, delta).record(&verdict, cut + 1);
        turns += 1;
        if turn.epoch.is_some() {
            epochs += 1;
            kept.clear();
        }
        kept.push(turn);
    }
    let last = requests.len() - 1;
    let cut = whole[requests[last].0].seq;
    let approval = requests[last].1.approval_id();
    drop(whole);

    // Today: a pass over the whole log, then the render.
    let started = Instant::now();
    let events = log
        .history()
        .expect("a pass")
        .collect::<Result<Vec<_>, _>>()
        .expect("the log");
    let rendered = render(&events, None, approval, bounds).expect("a render");
    let today = started.elapsed();
    let today_records = events.len();
    let today_bytes = json_bytes(&events);
    drop(events);

    // The reviewer's log: the delta after the last turn's cut, and the turn
    // rendered from it and the turns the log holds.
    let from = kept.last().expect("an earlier turn").cut_seq + 1;
    let started = Instant::now();
    let delta = log
        .history_range(from..=cut)
        .expect("a pass")
        .collect::<Result<Vec<_>, _>>()
        .expect("the delta");
    let read = started.elapsed();
    let delta_records = delta.len();
    let delta_bytes = json_bytes(&delta);
    // What the call under review's turn holds comes from the model's context,
    // which the session holds; here from a pass the measurement makes.
    let whole = log
        .history()
        .expect("a pass")
        .collect::<Result<Vec<_>, _>>()
        .expect("the log");
    let before = &whole[..=requests[last].0];
    let lookup =
        |owner: &cookie_agent_protocol::AssistantToolCallRef| thread::turn_arguments(before, owner);
    let call = call_of(before, &requests[last].1);
    let started = Instant::now();
    let prepared = Review {
        thread: &kept,
        unrecorded: None,
        inherited: None,
        request: &requests[last].1,
        cut,
        call: call.as_ref(),
        turns: Some(&lookup),
        bounds,
    }
    .prepare(delta);
    let ours = read + started.elapsed();
    let thread_bytes = kept
        .iter()
        .map(|turn| serde_json::to_vec(turn).map_or(0, |bytes| bytes.len()))
        .sum::<usize>();
    // The log gives no verdicts of its own, so today's render is a fresh turn
    // where the reviewer's log sends a delta: what is compared is the cost.
    assert!(!rendered.request.is_empty() && !prepared.conversation.request.is_empty());
    println!(
        "{}: {} records, {} reviews ({} turns recorded, {} epochs, {} turns held)\n\
         today: {:.1} ms, {} records read, {:.2} MB of them as JSON\n\
         reviewer log: {:.2} ms, {} records read from {from}, {:.1} KB as JSON; the turns since \
         the epoch {:.1} KB",
        path.display(),
        log.record_count(),
        requests.len(),
        turns,
        epochs,
        kept.len(),
        today.as_secs_f64() * 1_000.0,
        today_records,
        today_bytes as f64 / 1_000_000.0,
        ours.as_secs_f64() * 1_000.0,
        delta_records,
        delta_bytes as f64 / 1_000.0,
        thread_bytes as f64 / 1_000.0,
    );
}
