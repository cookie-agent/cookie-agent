use std::collections::BTreeSet;

use cookie_agent_protocol::ToolGroupSource;

use super::{load_result, search_result};
use crate::deferred_tools::{DeferralView, DeferredEntry, Group};

fn entry(name: &str, group: &str, description: &str) -> DeferredEntry {
    DeferredEntry {
        name: name.into(),
        description: description.into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"selector": {"type": "string", "description": "A CSS selector"}}
        }),
        group: Group {
            name: group.into(),
            source: ToolGroupSource::Mcp,
            summary: Some("Browser automation.".into()),
            untrusted: true,
        },
        provider: (format!("mcp.{group}"), None),
    }
}

fn view() -> DeferralView {
    DeferralView {
        deferred: vec![
            entry(
                "chrome_click",
                "chrome",
                "Click an element. Waits for it first.",
            ),
            entry("chrome_type", "chrome", "Type text into an element."),
            entry("calendar_list", "calendar", "List calendar events."),
        ],
        eager: BTreeSet::from(["read".to_owned()]),
        loaded: BTreeSet::from(["chrome_type".to_owned()]),
        sections: std::collections::BTreeMap::new(),
    }
}

#[test]
fn search_lists_matches_without_loading_them() {
    let result = search_result(&view(), "chrome click", 10);
    assert!(result.loaded_tools.is_empty(), "searching never loads");
    let lines = result.output.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "chrome_click [chrome]: Click an element.");
    assert_eq!(
        lines[1],
        "chrome_type [chrome] (loaded): Type text into an element."
    );
    assert!(!result.output.contains("calendar_list"));
}

#[test]
fn search_matches_parameters_and_lists_up_to_its_limit() {
    let result = search_result(&view(), "selector", 1);
    assert!(result.output.starts_with("calendar_list [calendar]"));
    assert!(result.output.contains("2 more match"));
    let every = search_result(&view(), "selector", usize::MAX);
    assert_eq!(
        every
            .output
            .lines()
            .filter(|line| line.contains(" ["))
            .count(),
        3
    );
}

#[test]
fn search_says_when_nothing_matches_or_nothing_is_loadable() {
    assert!(
        search_result(&view(), "spreadsheet", 10)
            .output
            .contains("No loadable tools match \"spreadsheet\"")
    );
    assert_eq!(
        search_result(&DeferralView::default(), "anything", 10).output,
        "No tools are loadable in this session."
    );
}

#[test]
fn load_records_new_tools_and_explains_the_rest() {
    let result = load_result(
        &view(),
        &[
            "chrome_click".into(),
            "calendar_list".into(),
            "chrome_click".into(),
            "chrome_type".into(),
            "read".into(),
            "missing".into(),
        ],
    );
    assert_eq!(result.loaded_tools, ["chrome_click", "calendar_list"]);
    assert_eq!(
        result.output,
        "Loaded tools: chrome_click, calendar_list.\nchrome_type: already loaded\nread: already callable, no need to load it\nmissing: no loadable tool has this name"
    );
}

#[test]
fn a_load_that_loads_nothing_is_plain_text() {
    let result = load_result(&view(), &["chrome_type".into()]);
    assert!(result.loaded_tools.is_empty());
    assert!(result.output.starts_with("Loaded no new tools."));
}

#[test]
fn a_first_load_carries_its_deferred_providers_sections_once() {
    let mut view = view();
    view.sections.insert(
        ("mcp.chrome".to_owned(), None),
        "<tool_instructions provider=\"mcp.chrome\">\nSnapshot first.\n</tool_instructions>".into(),
    );
    let result = load_result(&view, &["chrome_click".into(), "calendar_list".into()]);
    assert_eq!(result.additional_messages.len(), 1);
    let cookie_agent_protocol::ToolEmittedContent::Text(text) =
        &result.additional_messages[0].content[0]
    else {
        panic!("text instructions");
    };
    assert!(text.contains("Snapshot first."));
    let plain = load_result(&view, &["calendar_list".into()]);
    assert!(
        plain.additional_messages.is_empty(),
        "no sections for a provider that has none"
    );
}
