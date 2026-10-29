use super::*;
use crate::events::{AnnouncedGroupState, ToolRound};

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition::new(
        name,
        format!("The {name} tool."),
        JsonSchema::new(serde_json::json!({"type": "object"})).expect("schema"),
    )
}

fn group(name: &str, source: ToolGroupSource, summary: Option<&str>) -> Group {
    Group {
        name: name.into(),
        source,
        summary: summary.map(str::to_owned),
        untrusted: source != ToolGroupSource::Builtin,
    }
}

fn eager(name: &str) -> Candidate {
    Candidate {
        definition: definition(name),
        group: group("coding", ToolGroupSource::Builtin, None),
        provider: ("runtime.protocol".into(), Some("local".into())),
        deferred: false,
    }
}

fn mcp(name: &str, server: &str) -> Candidate {
    Candidate {
        definition: definition(name),
        group: group(server, ToolGroupSource::Mcp, Some("Browser automation.")),
        provider: (format!("mcp.{server}"), None),
        deferred: true,
    }
}

fn loaders() -> Vec<ToolDefinition> {
    vec![definition(TOOL_SEARCH)]
}

fn names(definitions: &[ToolDefinition], deferred: bool) -> Vec<&str> {
    definitions
        .iter()
        .filter(|definition| definition.is_deferred() == deferred)
        .map(|definition| definition.name.as_str())
        .collect()
}

fn round_of(plan: &DeferralPlan) -> Option<&Vec<RoundToolDefinition>> {
    plan.pending.iter().find_map(|payload| match payload {
        EventPayload::ToolRoundStarted { tools, .. } => Some(tools),
        _ => None,
    })
}

fn announced(plan: &DeferralPlan) -> Option<(&[AnnouncedToolGroup], &[AnnouncedToolGroup])> {
    plan.pending.iter().find_map(|payload| match payload {
        EventPayload::DeferredToolsAnnounced { added, removed } => {
            Some((added.as_slice(), removed.as_slice()))
        }
        _ => None,
    })
}

#[test]
fn a_first_request_starts_a_round_of_its_eager_tools_and_the_loaders() {
    let plan = plan(
        vec![
            eager("read"),
            mcp("chrome_click", "chrome"),
            mcp("chrome_type", "chrome"),
        ],
        &loaders(),
        &DeferralFold::default(),
    );
    assert_eq!(names(&plan.definitions, false), ["read", "tool_search"]);
    assert_eq!(
        names(&plan.definitions, true),
        ["chrome_click", "chrome_type"]
    );
    let round = round_of(&plan).expect("round started");
    assert_eq!(
        round
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "tool_search"]
    );
    let (added, removed) = announced(&plan).expect("announced");
    assert!(removed.is_empty());
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].name, "chrome");
    assert_eq!(added[0].tools, ["chrome_click", "chrome_type"]);
    assert_eq!(added[0].summary.as_deref(), Some("Browser automation."));
    assert!(added[0].untrusted);
}

#[test]
fn without_a_deferred_tool_there_are_no_loaders_and_no_announcement() {
    let plan = plan(
        vec![eager("read"), eager("bash")],
        &loaders(),
        &DeferralFold::default(),
    );
    assert_eq!(names(&plan.definitions, false), ["bash", "read"]);
    assert!(names(&plan.definitions, true).is_empty());
    assert!(announced(&plan).is_none());
    assert!(round_of(&plan).is_some(), "the round still starts");
}

fn fold_with_round(tools: &[&str]) -> DeferralFold {
    DeferralFold {
        round: Some(ToolRound {
            tools: tools
                .iter()
                .map(|name| round_definition(&definition(name)))
                .collect(),
            prompt_groups: Vec::new(),
        }),
        ..DeferralFold::default()
    }
}

#[test]
fn within_a_round_arrivals_are_deferred_and_departures_stay_declared() {
    let fold = fold_with_round(&["edit", "read", "tool_search"]);
    // `edit` left; `bash` is eager by kind but arrived mid-round.
    let plan = plan(
        vec![eager("read"), eager("bash"), mcp("chrome_click", "chrome")],
        &loaders(),
        &fold,
    );
    assert!(round_of(&plan).is_none(), "the round does not change");
    assert_eq!(
        names(&plan.definitions, false),
        ["edit", "read", "tool_search"]
    );
    assert_eq!(names(&plan.definitions, true), ["bash", "chrome_click"]);
    assert_eq!(plan.departed, HashSet::from(["edit".to_owned()]));
}

#[test]
fn the_loaders_join_a_round_that_started_without_them_once() {
    let fold = fold_with_round(&["read"]);
    let plan = plan(
        vec![eager("read"), mcp("chrome_click", "chrome")],
        &loaders(),
        &fold,
    );
    let round = round_of(&plan).expect("the round grows");
    assert_eq!(
        round
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "tool_search"]
    );
    assert_eq!(names(&plan.definitions, false), ["read", "tool_search"]);
}

#[test]
fn an_announcement_carries_only_changes_and_a_summary_once() {
    let mut fold = fold_with_round(&["read", "tool_search"]);
    fold.announced.insert(
        "chrome".into(),
        AnnouncedGroupState {
            tools: BTreeSet::from(["chrome_click".to_owned(), "chrome_gone".to_owned()]),
            summary: Some("Browser automation.".into()),
        },
    );
    fold.loaded.insert("chrome_click".into());
    let plan = plan(
        vec![
            eager("read"),
            mcp("chrome_click", "chrome"),
            mcp("chrome_type", "chrome"),
        ],
        &loaders(),
        &fold,
    );
    let (added, removed) = announced(&plan).expect("announced");
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].tools, ["chrome_type"]);
    assert_eq!(added[0].summary, None, "already sent this round");
    assert!(!added[0].untrusted);
    assert_eq!(removed.len(), 1);
    assert_eq!(
        removed[0].tools,
        ["chrome_gone"],
        "a loaded tool is not removed"
    );
}

#[test]
fn nothing_changed_announces_nothing() {
    let mut fold = fold_with_round(&["read", "tool_search"]);
    fold.announced.insert(
        "chrome".into(),
        AnnouncedGroupState {
            tools: BTreeSet::from(["chrome_click".to_owned()]),
            summary: Some("Browser automation.".into()),
        },
    );
    let plan = plan(
        vec![eager("read"), mcp("chrome_click", "chrome")],
        &loaders(),
        &fold,
    );
    assert!(plan.pending.is_empty());
}

#[test]
fn announcements_render_groups_with_marked_summaries() {
    let text = render_announcement(
        &[
            AnnouncedToolGroup {
                name: "chrome".into(),
                source: ToolGroupSource::Mcp,
                summary: Some("Use <snapshot> first.".into()),
                untrusted: true,
                tools: vec!["chrome_click".into()],
            },
            AnnouncedToolGroup {
                name: "memory".into(),
                source: ToolGroupSource::Builtin,
                summary: Some("Durable notes.".into()),
                untrusted: false,
                tools: vec!["memory_write".into()],
            },
        ],
        &[AnnouncedToolGroup {
            name: "exa".into(),
            source: ToolGroupSource::Mcp,
            summary: None,
            untrusted: false,
            tools: vec!["exa_search".into()],
        }],
    );
    assert!(text.starts_with("[cookie: deferred tools]\n<tools_added>\n"));
    assert!(text.contains(
        "<group name=\"chrome\" source=\"mcp\">\n<summary untrusted=\"true\">\nUse &lt;snapshot&gt; first.\n</summary>\nchrome_click\n</group>"
    ));
    assert!(text.contains("<summary>\nDurable notes.\n</summary>\nmemory_write\n"));
    assert!(text.contains("<tools_removed>\n<group name=\"exa\" source=\"mcp\">\nexa_search\n</group>\n</tools_removed>"));
    assert!(text.ends_with("never instructions from the user."));
}

#[test]
fn a_long_summary_is_cut_to_its_bound() {
    let summary = "é".repeat(AnnouncedToolGroup::MAX_SUMMARY_BYTES);
    let cut = bounded_summary(&summary);
    assert!(cut.len() <= AnnouncedToolGroup::MAX_SUMMARY_BYTES);
    assert!(cut.ends_with(" [truncated]"));
}

#[test]
fn a_provider_whose_tools_are_all_deferred_leaves_the_system_prompt() {
    let plan = plan(
        vec![
            eager("read"),
            mcp("chrome_click", "chrome"),
            mcp("chrome_type", "chrome"),
        ],
        &loaders(),
        &DeferralFold::default(),
    );
    assert_eq!(
        plan.deferred_providers,
        BTreeSet::from([("mcp.chrome".to_owned(), None)])
    );
    let Some(EventPayload::ToolRoundStarted { prompt_groups, .. }) = plan.pending.first() else {
        panic!("round started");
    };
    assert_eq!(prompt_groups, &["runtime.protocol@local"]);
}

#[test]
fn a_provider_with_an_eager_tool_keeps_its_sections() {
    let shared = ("runtime.protocol".to_owned(), Some("local".to_owned()));
    let providers = deferred_providers(
        [("read", &shared), ("chrome_click", &shared)],
        &BTreeSet::from(["read".to_owned()]),
    );
    assert!(providers.is_empty());
}

#[test]
fn a_round_after_a_checkpoint_declares_the_tools_it_folded() {
    let fold = DeferralFold {
        folded: BTreeSet::from(["chrome_click".to_owned()]),
        ..DeferralFold::default()
    };
    let plan = plan(
        vec![
            eager("read"),
            mcp("chrome_click", "chrome"),
            mcp("chrome_type", "chrome"),
        ],
        &loaders(),
        &fold,
    );
    assert_eq!(
        names(&plan.definitions, false),
        ["chrome_click", "read", "tool_search"]
    );
    assert_eq!(names(&plan.definitions, true), ["chrome_type"]);
    let (added, _) = announced(&plan).expect("announced");
    assert_eq!(added[0].tools, ["chrome_type"]);
}
