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
    }
}

fn eager(name: &str) -> Candidate {
    Candidate {
        definition: definition(name),
        group: group("coding", ToolGroupSource::Builtin, None),
        provider: ("runtime.protocol".into(), Some("local".into())),
        deferred: false,
        source: None,
        machines: Vec::new(),
    }
}

fn mcp(name: &str, server: &str) -> Candidate {
    Candidate {
        definition: definition(name),
        group: group(server, ToolGroupSource::Mcp, Some("Browser automation.")),
        provider: (format!("mcp.{server}"), None),
        deferred: true,
        source: None,
        machines: Vec::new(),
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
                .map(|name| round_definition(&definition(name), None, None, &[]))
                .collect(),
            prompt_groups: Vec::new(),
            engine_build: None,
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
fn announcements_render_groups_with_their_summaries() {
    let chrome = AnnouncedToolGroup {
        name: "chrome".into(),
        source: ToolGroupSource::Mcp,
        summary: Some("Use <snapshot> first.".into()),
        untrusted: false,
        tools: vec!["chrome_click".into()],
    };
    let memory = AnnouncedToolGroup {
        name: "memory".into(),
        source: ToolGroupSource::Builtin,
        summary: Some("Durable notes.".into()),
        untrusted: false,
        tools: vec!["memory_write".into()],
    };
    let exa = AnnouncedToolGroup {
        name: "exa".into(),
        source: ToolGroupSource::Mcp,
        summary: None,
        untrusted: false,
        tools: vec!["exa_search".into()],
    };
    assert_eq!(
        render_announcement(&[chrome, memory.clone()], &[exa], true),
        "<system-reminder>\nThe following deferred tool groups are now available via `tool_search`. Their schemas are NOT loaded — calling them directly will fail. Use `tool_search` with query \"select:<name>[,<name>...]\" to load tool schemas before calling them, or with keywords to load the best matches (\"+word\" requires a word in the tool's name):\n\
         <group name=\"chrome\" source=\"mcp\" summary=\"Use &lt;snapshot&gt; first.\">\nchrome_click\n</group>\n\
         <group name=\"memory\" source=\"builtin\" summary=\"Durable notes.\">\nmemory_write\n</group>\n\n\
         The following deferred tool groups are no longer available in this session. Do not search for them — `tool_search` will return no match:\n\
         <group name=\"exa\" source=\"mcp\">\nexa_search\n</group>\n</system-reminder>"
    );
    assert_eq!(
        render_announcement(&[memory], &[], false),
        "<system-reminder>\nMore deferred tool groups are now available via `tool_search`:\n\
         <group name=\"memory\" source=\"builtin\" summary=\"Durable notes.\">\nmemory_write\n</group>\n</system-reminder>"
    );
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

fn eager_candidate(name: &str) -> EagerCandidate {
    EagerCandidate {
        definition: definition(name),
        provider: ("builtin.test".into(), None),
        source: None,
        group: group("test", ToolGroupSource::Builtin, None),
        machines: Vec::new(),
    }
}

#[test]
fn without_deferral_a_first_request_starts_a_round_of_every_tool() {
    let plan = plan_eager(
        vec![eager_candidate("read"), eager_candidate("bash")],
        &DeferralFold::default(),
    );
    assert_eq!(names(&plan.definitions, false), ["bash", "read"]);
    let EventPayload::ToolRoundStarted { tools, .. } = &plan.pending[0] else {
        panic!("a round starts");
    };
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["bash", "read"]
    );
    assert!(plan.departed.is_empty());
}

#[test]
fn without_deferral_departures_stay_declared_and_arrivals_join_up_front() {
    let fold = fold_with_round(&["edit", "read", "tool_search"]);
    // `read`'s live description changed; the round's stays.
    let mut changed = definition("read");
    changed.description = "A new description.".into();
    let plan = plan_eager(
        vec![
            EagerCandidate {
                definition: changed,
                provider: ("builtin.test".into(), None),
                source: None,
                group: group("test", ToolGroupSource::Builtin, None),
                machines: Vec::new(),
            },
            eager_candidate("bash"),
        ],
        &fold,
    );
    assert!(plan.pending.is_empty(), "the round does not change");
    assert_eq!(names(&plan.definitions, false), ["bash", "edit", "read"]);
    assert!(
        plan.definitions
            .iter()
            .all(|definition| definition.description != "A new description.")
    );
    // The loader of a deferring model's round is not declared.
    assert_eq!(plan.departed, HashSet::from(["edit".to_owned()]));
}

#[test]
fn a_tool_that_stays_deferred_keeps_its_entry_until_it_changes() {
    let candidates = || {
        vec![
            eager("read"),
            mcp("chrome_click", "chrome"),
            mcp("chrome_type", "chrome"),
        ]
    };
    let first = plan(candidates(), &loaders(), &DeferralFold::default());
    let mut changed = candidates();
    changed[2].definition.description = "Types into an element.".into();
    let next = plan_reusing(changed, &loaders(), &DeferralFold::default(), &first.view);
    let entry = |view: &DeferralView, name: &str| {
        std::sync::Arc::clone(
            view.deferred
                .iter()
                .find(|entry| entry.name == name)
                .expect("deferred entry"),
        )
    };
    assert!(std::sync::Arc::ptr_eq(
        &entry(&first.view, "chrome_click"),
        &entry(&next.view, "chrome_click")
    ));
    let typed = entry(&next.view, "chrome_type");
    assert!(!std::sync::Arc::ptr_eq(
        &entry(&first.view, "chrome_type"),
        &typed
    ));
    assert_eq!(typed.description, "Types into an element.");
}

/// A round of `tools`, each `(name, group, machines)`.
fn round_with_machines(tools: &[(&str, &str, &[&str])]) -> DeferralFold {
    DeferralFold {
        round: Some(ToolRound {
            tools: tools
                .iter()
                .map(|(name, group_name, machines)| {
                    round_definition(
                        &definition(name),
                        None,
                        Some(&group(group_name, ToolGroupSource::Builtin, None)),
                        &machines
                            .iter()
                            .map(|machine| (*machine).to_owned())
                            .collect::<Vec<_>>(),
                    )
                })
                .collect(),
            prompt_groups: Vec::new(),
            engine_build: None,
        }),
        ..DeferralFold::default()
    }
}

fn now(tools: &[(&str, &[&str])]) -> std::collections::HashMap<String, Vec<String>> {
    tools
        .iter()
        .map(|(name, machines)| {
            (
                (*name).to_owned(),
                machines
                    .iter()
                    .map(|machine| (*machine).to_owned())
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn a_round_tool_that_left_one_machine_or_every_machine_is_announced_once() {
    let mut fold = round_with_machines(&[
        ("bash", "coding", &["laptop", "labpc"]),
        ("read", "coding", &["laptop", "labpc"]),
        ("cron_list", "cron", &[]),
    ]);
    let current = now(&[("bash", &["laptop"]), ("read", &["laptop"])]);
    let Some(EventPayload::ToolsAvailabilityAnnounced {
        unavailable,
        available_again,
    }) = availability_announcement(&current, &fold)
    else {
        panic!("a notice");
    };
    assert!(available_again.is_empty());
    let summary = unavailable
        .iter()
        .map(|group| {
            (
                group.name.as_str(),
                group.machine.as_deref(),
                group.tools.clone(),
                group.available_on.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            (
                "cron",
                None,
                vec!["cron_list".to_owned()],
                Vec::<String>::new()
            ),
            (
                "coding",
                Some("labpc"),
                vec!["bash".to_owned(), "read".to_owned()],
                vec!["laptop".to_owned()]
            ),
        ]
    );
    // Once announced, it is not announced again.
    // As the fold records the notice.
    for group in &unavailable {
        for tool in &group.tools {
            fold.unavailable
                .insert((tool.clone(), group.machine.clone()));
        }
    }
    assert!(availability_announcement(&current, &fold).is_none());
    // Back on labpc while still declared: available again.
    let back = now(&[
        ("bash", &["laptop", "labpc"]),
        ("read", &["laptop", "labpc"]),
    ]);
    let Some(EventPayload::ToolsAvailabilityAnnounced {
        unavailable,
        available_again,
    }) = availability_announcement(&back, &fold)
    else {
        panic!("a notice");
    };
    assert!(unavailable.is_empty());
    assert_eq!(available_again.len(), 1);
    assert_eq!(available_again[0].machine.as_deref(), Some("labpc"));
}

#[test]
fn no_round_no_notice() {
    assert!(availability_announcement(&now(&[]), &DeferralFold::default()).is_none());
}
