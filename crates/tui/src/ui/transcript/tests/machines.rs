//! The machine a call ran on, on its row
//! (`docs/site/specs/tool-call-machines.md`).

use crate::ui::transcript::*;

use cookie_agent_client_state::call_machines::RuntimeMachines;
use cookie_agent_client_state::transcript_mode::TranscriptMode;
use cookie_agent_protocol::{
    AttachmentName, CapabilityName, EventPayload, MachineName, Open, RuntimeId, RuntimeKind,
    RuntimeLifetime, RuntimeListing, RuntimesListResult, SessionId, ToolSource,
};

use crate::state::{EventLevel, SessionState};

use super::support::*;

const LABSERVER: &str = "yunhao-labserver";
const LABPC: &str = "yunhao-labpc";

fn listing(pairs: &[(&str, &str)]) -> RuntimeMachines {
    RuntimeMachines::from_listing(&RuntimesListResult {
        runtimes: pairs
            .iter()
            .map(|(id, machine)| RuntimeListing {
                runtime_id: RuntimeId::new(*id).unwrap(),
                lifetime: RuntimeLifetime::Spawned,
                connected: true,
                capabilities: Vec::new(),
                platform: None,
                machine: Some(MachineName::new(*machine).unwrap()),
                home: None,
                message: None,
                active_runs: 0,
                background_calls: 0,
            })
            .collect(),
        instances: Vec::new(),
    })
}

fn two_machines() -> RuntimeMachines {
    listing(&[("local", LABSERVER), ("labpc", LABPC)])
}

/// A session with a read that ran on `local`, attaching `attached`.
fn session(attached: &[&str]) -> SessionState {
    let mut state = read_tool_state("unused", ToolStatus::Completed, "");
    let call = read_tool_id(&state);
    state.tools.get_mut(&call).unwrap().presentation.tool.source =
        Some(Open::Known(ToolSource::Runtime {
            runtime_id: RuntimeId::new("local").unwrap(),
            attachment: AttachmentName::new("local_work").unwrap(),
            capability: CapabilityName::coding(),
        }));
    state.call_runtimes.insert(RuntimeId::new("local").unwrap());
    for id in attached {
        state
            .runtime_attachments
            .observe(&EventPayload::RuntimeAttached {
                name: AttachmentName::new(format!("{id}_work")).unwrap(),
                runtime_id: RuntimeId::new(*id).unwrap(),
                capability: CapabilityName::coding(),
                kind: RuntimeKind::new("coding").unwrap(),
                workspace: None,
            });
    }
    state
}

fn draw(
    cache: &mut LayoutCache,
    state: &SessionState,
    machines: Option<&RuntimeMachines>,
    width: u16,
) -> String {
    ensure_cached_transcript_layout_in(
        cache,
        SessionId::new_v7(),
        state,
        None,
        None,
        width,
        &Theme::default(),
        &crate::markdown::PlainHighlighter,
        TranscriptView {
            minimum_event_level: EventLevel::Warning,
            transcript: TranscriptMode::Full,
            now: jiff::Timestamp::UNIX_EPOCH,
            machines,
        },
        0,
        &crate::state::ModelDisplayNames::default(),
        None,
    );
    snapshot_lines(&cache.layout.lines.to_vec())
}

fn row_of(text: &str) -> &str {
    text.lines()
        .find(|line| line.contains("read"))
        .expect("the call's row")
}

#[test]
fn a_call_names_its_machine_once_the_session_has_two() {
    let machines = two_machines();
    let labelled = draw(
        &mut LayoutCache::default(),
        &session(&["local", "labpc"]),
        Some(&machines),
        100,
    );
    assert!(row_of(&labelled).contains(LABSERVER), "{labelled}");

    let alone = draw(
        &mut LayoutCache::default(),
        &session(&["local"]),
        Some(&machines),
        100,
    );
    assert!(!row_of(&alone).contains(LABSERVER), "{alone}");
}

#[test]
fn nothing_shows_before_a_listing_answers_and_the_row_draws_again_when_one_does() {
    let state = session(&["local", "labpc"]);
    let machines = two_machines();
    let mut cache = LayoutCache::default();
    let before = draw(&mut cache, &state, None, 100);
    assert!(!row_of(&before).contains(LABSERVER), "{before}");
    let after = draw(&mut cache, &state, Some(&machines), 100);
    assert!(row_of(&after).contains(LABSERVER), "{after}");
}

#[test]
fn a_narrow_row_keeps_the_label_short_and_cuts_the_title_first() {
    let state = session(&["local", "labpc"]);
    let machines = two_machines();
    let width = MACHINE_SHORT_BELOW - 1;
    let text = draw(&mut LayoutCache::default(), &state, Some(&machines), width);
    let row = row_of(&text);
    assert!(
        row.contains("labserver") && !row.contains(LABSERVER),
        "{text}"
    );
    // Too narrow for a title beside it, the label goes rather than the title.
    let text = draw(&mut LayoutCache::default(), &state, Some(&machines), 24);
    assert!(!row_of(&text).contains("labserver"), "{text}");
}

#[test]
fn the_background_button_stays_the_row_s_end_after_the_label() {
    let mut state = session(&["local", "labpc"]);
    let call = read_tool_id(&state);
    let tool = state.tools.get_mut(&call).unwrap();
    tool.status = ToolStatus::Running;
    tool.backgroundable = true;
    let machines = two_machines();
    let text = draw(&mut LayoutCache::default(), &state, Some(&machines), 100);
    let row = row_of(&text).trim_end();
    assert!(row.ends_with(BACKGROUND_BUTTON), "{text}");
    assert!(row.contains(LABSERVER), "{text}");
    assert!(row.find(LABSERVER) < row.find(BACKGROUND_BUTTON), "{text}");
}

#[test]
fn every_label_starts_in_the_same_column() {
    let mut state = session(&["local", "labpc"]);
    let call = read_tool_id(&state);
    state
        .tools
        .get_mut(&call)
        .unwrap()
        .presentation
        .primary_argument = None;
    let machines = two_machines();
    let wide = draw(&mut LayoutCache::default(), &state, Some(&machines), 100);
    let other = listing(&[("local", "yunhao-labpc"), ("labpc", LABSERVER)]);
    let short = draw(&mut LayoutCache::default(), &state, Some(&other), 100);
    let column = |text: &str| {
        let row = row_of(text).to_owned();
        unicode_width::UnicodeWidthStr::width(&row[..row.find('●').expect("the dot")])
    };
    // The shorter name's dot stands where the longer one's does.
    assert_eq!(column(&wide), column(&short));
}
