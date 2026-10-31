use std::collections::BTreeSet;

use cookie_agent_protocol::ToolGroupSource;

use super::{load_result, search_result};
use crate::deferred_tools::{DeferralView, DeferredEntry, Group};

fn entry(name: &str, group: &str, description: &str) -> std::sync::Arc<DeferredEntry> {
    std::sync::Arc::new(DeferredEntry {
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
    })
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
fn keywords_load_the_best_matches() {
    let result = search_result(&view(), "chrome click", 10);
    assert_eq!(result.loaded_tools, ["chrome_click"]);
    assert_eq!(
        result.output,
        "Loaded 1 tool:\n- chrome_click [chrome]: Click an element.\n- chrome_type [chrome] (already loaded): Type text into an element."
    );
}

#[test]
fn keywords_match_parameters_and_load_up_to_the_limit() {
    let result = search_result(&view(), "selector", 1);
    assert_eq!(result.loaded_tools, ["calendar_list"]);
    assert!(
        result
            .output
            .ends_with("2 more match; raise limit or select them by name."),
        "{}",
        result.output
    );
    let every = search_result(&view(), "selector", usize::MAX);
    assert_eq!(every.loaded_tools, ["calendar_list", "chrome_click"]);
    assert!(
        every
            .output
            .contains("chrome_type [chrome] (already loaded)")
    );
}

#[test]
fn a_plus_word_must_be_in_the_tools_name() {
    // "selector" matches every tool's parameters; "+click" keeps the one
    // whose name has it.
    let result = search_result(&view(), "+click selector", 10);
    assert_eq!(result.loaded_tools, ["chrome_click"]);
    assert!(
        !result.output.contains("calendar_list"),
        "{}",
        result.output
    );
    let none = search_result(&view(), "+spreadsheet selector", 10);
    assert!(none.loaded_tools.is_empty());
}

#[test]
fn a_search_says_when_nothing_matches_or_nothing_is_loadable() {
    let none = search_result(&view(), "spreadsheet", 10);
    assert!(none.loaded_tools.is_empty());
    assert!(
        none.output
            .contains("No loadable tools match \"spreadsheet\"")
    );
    assert_eq!(
        search_result(&DeferralView::default(), "anything", 10).output,
        "No tools are loadable in this session."
    );
}

#[test]
fn a_selection_loads_new_tools_and_explains_the_rest() {
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
        None,
    );
    assert_eq!(result.loaded_tools, ["chrome_click", "calendar_list"]);
    assert_eq!(
        result.output,
        "Loaded 2 tools:\n- chrome_click [chrome]: Click an element.\n- calendar_list [calendar]: List calendar events.\n- chrome_type [chrome] (already loaded): Type text into an element.\nread: already callable, no need to load it\nmissing: no loadable tool has this name"
    );
}

#[test]
fn a_load_that_loads_nothing_records_nothing() {
    let result = load_result(&view(), &["chrome_type".into()], None);
    assert!(result.loaded_tools.is_empty());
    assert!(result.output.starts_with("Loaded no new tools."));
}

#[test]
fn select_queries_name_tools_and_keywords_do_not() {
    assert_eq!(
        super::selected("select:a, b,,c"),
        Some(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()])
    );
    assert_eq!(super::selected("select:"), Some(Vec::new()));
    assert_eq!(super::selected("browser select"), None);
}

#[test]
fn a_first_load_carries_its_deferred_providers_sections_once() {
    let mut view = view();
    view.sections.insert(
        ("mcp.chrome".to_owned(), None),
        "<tool_instructions provider=\"mcp.chrome\">\nSnapshot first.\n</tool_instructions>".into(),
    );
    let result = load_result(
        &view,
        &["chrome_click".into(), "calendar_list".into()],
        None,
    );
    assert_eq!(result.additional_messages.len(), 1);
    let cookie_agent_protocol::ToolEmittedContent::Text(text) =
        &result.additional_messages[0].content[0]
    else {
        panic!("text instructions");
    };
    assert!(text.contains("Snapshot first."));
    let plain = load_result(&view, &["calendar_list".into()], None);
    assert!(
        plain.additional_messages.is_empty(),
        "no sections for a provider that has none"
    );
}
