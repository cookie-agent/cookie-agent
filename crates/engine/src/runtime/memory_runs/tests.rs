use std::collections::BTreeSet;

use cookie_agent_protocol::{
    AgentId, AgentMode, MachineName, MemorySnapshot, MemorySnapshotAgent, MemorySnapshotMachine,
    PermissionAction, PermissionEffect, PermissionRule, RuntimeId, SessionPermissionOverlay,
    WildcardPattern,
};

use super::{
    MemoryAccess, MemoryCaller, SnapshotChange, SnapshotFacts, memory_access,
    memory_rules_fingerprint, snapshot_change,
};
use crate::memory::{search::Visibility, url::MemoryUrl};

fn rule(action: PermissionAction, resource: &str, effect: PermissionEffect) -> PermissionRule {
    PermissionRule::new(action, WildcardPattern::new(resource).unwrap(), effect)
}

fn agent(mode: AgentMode, rules: Vec<PermissionRule>) -> cookie_agent_protocol::AgentSnapshot {
    let mut agent = crate::test_support::agent_snapshot("reviewer", mode);
    agent.permissions = rules;
    agent
}

const BOTH: MemoryAccess = MemoryAccess {
    available: true,
    writable: true,
};
const READ_ONLY: MemoryAccess = MemoryAccess {
    available: true,
    writable: false,
};
const NONE: MemoryAccess = MemoryAccess {
    available: false,
    writable: false,
};

#[test]
fn memory_needs_global_a_memory_rule_and_a_non_internal_agent() {
    let allow = |action| rule(action, "*", PermissionEffect::Allow);
    let both = vec![
        allow(PermissionAction::MEMORY),
        allow(PermissionAction::MEMORY_WRITE),
    ];
    for mode in [AgentMode::Primary, AgentMode::Subagent, AgentMode::All] {
        assert_eq!(memory_access(true, &agent(mode, both.clone()), None), BOTH);
        assert_eq!(memory_access(false, &agent(mode, both.clone()), None), NONE);
    }
    assert_eq!(
        memory_access(true, &agent(AgentMode::Internal, both.clone()), None),
        NONE
    );
    // Write rules beside a read deny give nothing.
    let write_only = vec![allow(PermissionAction::MEMORY_WRITE)];
    let read_denied = [
        vec![rule(PermissionAction::MEMORY, "*", PermissionEffect::Deny)],
        write_only.clone(),
    ]
    .concat();
    assert_eq!(
        memory_access(true, &agent(AgentMode::Primary, read_denied), None),
        NONE
    );
    // `ask` counts; `deny` does not. Writes no rule names ask too.
    let ask = vec![rule(PermissionAction::MEMORY, "*", PermissionEffect::Ask)];
    assert_eq!(
        memory_access(true, &agent(AgentMode::Primary, ask), None),
        BOTH
    );
    let deny = vec![
        rule(PermissionAction::MEMORY, "*", PermissionEffect::Deny),
        allow(PermissionAction::MEMORY_WRITE),
    ];
    assert_eq!(
        memory_access(true, &agent(AgentMode::Primary, deny), None),
        NONE
    );
    // A rule for another runtime does not reach `global`, where a `*` deny
    // applies.
    let mut elsewhere = allow(PermissionAction::MEMORY);
    elsewhere.runtime = Some(RuntimeId::new("labpc").unwrap());
    let denied_here = rule(PermissionAction::MEMORY, "*", PermissionEffect::Deny);
    assert_eq!(
        memory_access(
            true,
            &agent(AgentMode::Primary, vec![denied_here, elsewhere]),
            None
        ),
        NONE
    );
    // The session overlay can give memory, or the write tools.
    let overlay = SessionPermissionOverlay {
        rules: both.clone(),
    };
    assert_eq!(
        memory_access(true, &agent(AgentMode::Primary, Vec::new()), Some(&overlay)),
        BOTH
    );
    let overlay = SessionPermissionOverlay {
        rules: write_only.clone(),
    };
    assert_eq!(
        memory_access(
            true,
            &agent(AgentMode::Primary, vec![allow(PermissionAction::MEMORY)]),
            Some(&overlay)
        ),
        BOTH
    );
}

/// The write tools need some memory URL both readable and writable
/// (section 16.9.1), decided over the rules' patterns.
#[test]
fn the_write_tools_need_memory_both_readable_and_writable() {
    // Each case closes both actions first: what no rule matches would ask,
    // and so be both readable and writable.
    let access = |read: &[(&str, PermissionEffect)], write: &[(&str, PermissionEffect)]| {
        let closed = [
            rule(PermissionAction::MEMORY, "*", PermissionEffect::Deny),
            rule(PermissionAction::MEMORY_WRITE, "*", PermissionEffect::Deny),
        ];
        let rules =
            closed
                .into_iter()
                .chain(
                    read.iter()
                        .map(|(pattern, effect)| rule(PermissionAction::MEMORY, pattern, *effect)),
                )
                .chain(write.iter().map(|(pattern, effect)| {
                    rule(PermissionAction::MEMORY_WRITE, pattern, *effect)
                }))
                .collect();
        memory_access(true, &agent(AgentMode::Primary, rules), None)
    };
    let (allow, ask, deny) = (
        PermissionEffect::Allow,
        PermissionEffect::Ask,
        PermissionEffect::Deny,
    );
    assert_eq!(
        access(&[("user://*", allow)], &[("project://*", allow)]),
        READ_ONLY
    );
    assert_eq!(
        access(&[("user://*", allow)], &[("user://feedback", ask)]),
        BOTH
    );
    assert_eq!(
        access(&[("agent://reviewer", ask)], &[("agent://*", allow)]),
        BOTH
    );
    assert_eq!(
        access(
            &[("project://web/*", allow), ("user://*", allow)],
            &[("project://api/*", allow), ("user://*", deny)]
        ),
        READ_ONLY
    );
    // No file has to exist for a URL to count.
    assert_eq!(
        access(
            &[("machine://*/gpu", allow)],
            &[("machine://labpc/*", allow)]
        ),
        BOTH
    );
}

/// The fingerprint of the test runs' `memory` rules.
fn rules() -> cookie_agent_protocol::Sha256Digest {
    cookie_agent_protocol::Sha256Digest::of_bytes(b"memory rules")
}

fn facts(access: MemoryAccess, agent: &str, delegated: bool, machines: &[&str]) -> SnapshotFacts {
    SnapshotFacts {
        project: None,
        rules: rules(),
        access,
        agent: AgentId::new(agent).unwrap(),
        delegated,
        machines: machines
            .iter()
            .map(|machine| MachineName::new(*machine).unwrap())
            .collect::<BTreeSet<_>>(),
    }
}

fn taken(agent: &str, writable: bool, delegated: bool, machines: &[&str]) -> MemorySnapshot {
    MemorySnapshot {
        delegated,
        writable,
        agent: Some(MemorySnapshotAgent {
            agent: AgentId::new(agent).unwrap(),
            memory: None,
            omitted: false,
            listed: None,
        }),
        machines: machines
            .iter()
            .map(|machine| MemorySnapshotMachine {
                machine: MachineName::new(*machine).unwrap(),
                attachments: Vec::new(),
                memories: Vec::new(),
                omitted: 0,
            })
            .collect(),
        rules: Some(rules()),
        taken_at: Some("2026-10-22T12:00:00Z".parse().unwrap()),
        ..MemorySnapshot::default()
    }
}

#[test]
fn a_first_snapshot_is_taken_only_when_memory_is_available() {
    assert_eq!(
        snapshot_change(None, &facts(BOTH, "default", false, &["laptop"])),
        SnapshotChange::Take
    );
    assert_eq!(
        snapshot_change(None, &facts(NONE, "default", false, &["laptop"])),
        SnapshotChange::Keep
    );
    // After an empty snapshot, the next available run takes one.
    let empty = MemorySnapshot::unavailable();
    assert_eq!(
        snapshot_change(
            Some((&empty, false)),
            &facts(BOTH, "default", false, &["laptop"])
        ),
        SnapshotChange::Take
    );
    assert_eq!(
        snapshot_change(
            Some((&empty, false)),
            &facts(NONE, "default", false, &["laptop"])
        ),
        SnapshotChange::Keep
    );
}

#[test]
fn each_refresh_trigger_takes_a_new_snapshot() {
    let latest = taken("default", true, false, &["laptop", "labpc"]);
    let same = facts(BOTH, "default", false, &["labpc", "laptop"]);
    // Nothing changed (machine order does not matter): keep.
    assert_eq!(
        snapshot_change(Some((&latest, false)), &same),
        SnapshotChange::Keep
    );
    // A compaction finished after it.
    assert_eq!(
        snapshot_change(Some((&latest, true)), &same),
        SnapshotChange::Take
    );
    // The set of machines changed.
    assert_eq!(
        snapshot_change(
            Some((&latest, false)),
            &facts(BOTH, "default", false, &["laptop"])
        ),
        SnapshotChange::Take
    );
    // The run's agent changed.
    assert_eq!(
        snapshot_change(
            Some((&latest, false)),
            &facts(BOTH, "reviewer", false, &["laptop", "labpc"])
        ),
        SnapshotChange::Take
    );
    // Write access changed.
    assert_eq!(
        snapshot_change(
            Some((&latest, false)),
            &facts(READ_ONLY, "default", false, &["laptop", "labpc"])
        ),
        SnapshotChange::Take
    );
    // The `memory` rules changed (section 7.5), as when the session
    // overlay or the agent's document does; and a snapshot an earlier
    // daemon took records none.
    let mut changed_rules = same.clone();
    changed_rules.rules = cookie_agent_protocol::Sha256Digest::of_bytes(b"other rules");
    assert_eq!(
        snapshot_change(Some((&latest, false)), &changed_rules),
        SnapshotChange::Take
    );
    let earlier = MemorySnapshot {
        rules: None,
        ..latest.clone()
    };
    assert_eq!(
        snapshot_change(Some((&earlier, false)), &same),
        SnapshotChange::Take
    );
    // A snapshot that records no time, which an earlier daemon took, could
    // show no ages or fullness (section 16.9.7).
    let untimed = MemorySnapshot {
        taken_at: None,
        ..latest.clone()
    };
    assert_eq!(
        snapshot_change(Some((&untimed, false)), &same),
        SnapshotChange::Take
    );
    // Memory became unavailable: an empty snapshot.
    assert_eq!(
        snapshot_change(
            Some((&latest, false)),
            &facts(NONE, "default", false, &["laptop", "labpc"])
        ),
        SnapshotChange::Clear
    );
}

#[test]
fn the_rules_fingerprint_follows_only_the_memory_rules_reaching_global() {
    let allow = |action| rule(action, "*", PermissionEffect::Allow);
    let base = agent(AgentMode::Primary, vec![allow(PermissionAction::MEMORY)]);
    let overlay = SessionPermissionOverlay::empty();
    let fingerprint = memory_rules_fingerprint(&base, &overlay);
    // Another action's rule, or a memory rule for another runtime, does
    // not change it.
    let mut other = base.clone();
    other.permissions.push(allow(PermissionAction::BASH));
    let mut elsewhere = allow(PermissionAction::MEMORY);
    elsewhere.runtime = Some(RuntimeId::new("labpc").unwrap());
    other.permissions.push(elsewhere);
    assert_eq!(memory_rules_fingerprint(&other, &overlay), fingerprint);
    // An overlay memory rule does.
    let overlay = SessionPermissionOverlay {
        rules: vec![allow(PermissionAction::MEMORY)],
    };
    assert_ne!(memory_rules_fingerprint(&base, &overlay), fingerprint);
}

/// An agent memory with an empty body is left out of the prompt but stays
/// in the event, so the snapshot still records that memory is available:
/// later runs keep it, and the agent's first body shows from the next
/// refresh, like any other write (section 16.7).
#[test]
fn an_empty_agent_memory_keeps_the_snapshot_until_a_refresh() {
    let mut empty = taken("reviewer", true, false, &["labpc"]);
    empty.agent.as_mut().unwrap().memory = Some(cookie_agent_protocol::MemorySnapshotMemory {
        description: "Memory of the `reviewer` agent".into(),
        keywords: vec!["reviewer".into(), "subagent".into()],
        body: String::new(),
        status: cookie_agent_protocol::MemoryDocumentStatus::default(),
    });
    assert_eq!(
        crate::memory::render::agent_section(empty.agent.as_ref().unwrap(), None),
        None
    );
    let same = facts(BOTH, "reviewer", false, &["labpc"]);
    assert_eq!(
        snapshot_change(Some((&empty, false)), &same),
        SnapshotChange::Keep
    );
    assert_eq!(
        snapshot_change(Some((&empty, true)), &same),
        SnapshotChange::Take
    );
}

#[test]
fn a_delegated_snapshot_ignores_machines() {
    let latest = taken("reviewer", false, true, &[]);
    assert_eq!(
        snapshot_change(
            Some((&latest, false)),
            &facts(READ_ONLY, "reviewer", true, &["labpc"])
        ),
        SnapshotChange::Keep
    );
}

#[test]
fn search_results_follow_the_memory_rule_for_their_url() {
    let caller = MemoryCaller {
        agent: agent(
            AgentMode::Primary,
            vec![
                rule(
                    PermissionAction::MEMORY,
                    "project://*",
                    PermissionEffect::Allow,
                ),
                rule(PermissionAction::MEMORY, "user://*", PermissionEffect::Ask),
                rule(
                    PermissionAction::MEMORY,
                    "agent://reviewer",
                    PermissionEffect::Allow,
                ),
                rule(
                    PermissionAction::MEMORY,
                    "machine://labpc/*",
                    PermissionEffect::Deny,
                ),
            ],
        ),
        overlay: SessionPermissionOverlay {
            rules: vec![rule(
                PermissionAction::MEMORY,
                "machine://labpc/ok",
                PermissionEffect::Allow,
            )],
        },
        related: super::RelatedMemory::default(),
    };
    let visibility = |url: &str| caller.visibility(&MemoryUrl::parse(url).unwrap());
    assert_eq!(visibility("project://web/build"), Visibility::Full);
    assert_eq!(visibility("user://profile"), Visibility::Summary);
    assert_eq!(visibility("agent://reviewer"), Visibility::Full);
    // No rule matches it, so reading it asks.
    assert_eq!(visibility("agent://default"), Visibility::Summary);
    assert_eq!(visibility("machine://labpc/x"), Visibility::Hidden);
    assert_eq!(visibility("machine://labpc/ok"), Visibility::Full);
}

#[test]
fn a_root_runs_project_or_its_name_changing_takes_a_new_snapshot() {
    let project = |name: &str| cookie_agent_protocol::MemorySnapshotProject {
        project: cookie_agent_protocol::ProjectName::new(name).unwrap(),
        overview: None,
        memories: Vec::new(),
        omitted: 0,
    };
    let in_project = |name: Option<&str>| SnapshotFacts {
        project: name.map(|name| cookie_agent_protocol::ProjectName::new(name).unwrap()),
        ..facts(BOTH, "default", false, &["laptop"])
    };
    let mut latest = taken("default", true, false, &["laptop"]);
    latest.project = Some(project("web"));
    assert_eq!(
        snapshot_change(Some((&latest, false)), &in_project(Some("web"))),
        SnapshotChange::Keep
    );
    // Renamed, moved elsewhere, or moved out.
    for moved in [Some("shop"), None] {
        assert_eq!(
            snapshot_change(Some((&latest, false)), &in_project(moved)),
            SnapshotChange::Take
        );
    }
    // Moved in.
    let outside = taken("default", true, false, &["laptop"]);
    assert_eq!(
        snapshot_change(Some((&outside, false)), &in_project(Some("web"))),
        SnapshotChange::Take
    );
    // A delegated run never shows the project.
    let delegated = taken("default", true, true, &[]);
    assert_eq!(
        snapshot_change(
            Some((&delegated, false)),
            &SnapshotFacts {
                project: Some(cookie_agent_protocol::ProjectName::new("web").unwrap()),
                ..facts(BOTH, "default", true, &[])
            }
        ),
        SnapshotChange::Keep
    );
}

/// Memory, the write tools, and the snapshot's read rules follow the merged
/// list of the agent's rules and the session overlay
/// (`docs/site/specs/permission-consolidation.md` section 3.2): a session
/// rule replaces the agent rule with its pattern, and the most specific rule
/// decides a URL.
#[test]
fn memory_follows_the_merged_rules() {
    let agent = agent(
        AgentMode::Primary,
        vec![
            // Writes stay to `user://`: what no rule matches would ask.
            rule(PermissionAction::MEMORY_WRITE, "*", PermissionEffect::Deny),
            rule(PermissionAction::MEMORY, "*", PermissionEffect::Allow),
            rule(
                PermissionAction::MEMORY,
                "agent://reviewer",
                PermissionEffect::Allow,
            ),
            rule(
                PermissionAction::MEMORY_WRITE,
                "user://*",
                PermissionEffect::Allow,
            ),
        ],
    );
    let overlay = |rules| SessionPermissionOverlay { rules };
    assert_eq!(memory_access(true, &agent, None), BOTH);
    // A session `*` deny replaces only the agent's `*`: its
    // `agent://reviewer` rule still offers memory, which `user://*` no
    // longer overlaps.
    let star_denied = overlay(vec![rule(
        PermissionAction::MEMORY,
        "*",
        PermissionEffect::Deny,
    )]);
    assert_eq!(memory_access(true, &agent, Some(&star_denied)), READ_ONLY);
    // Denying every rule that reads turns memory off.
    let all_denied = overlay(vec![
        rule(PermissionAction::MEMORY, "*", PermissionEffect::Deny),
        rule(
            PermissionAction::MEMORY,
            "agent://reviewer",
            PermissionEffect::Deny,
        ),
    ]);
    assert_eq!(memory_access(true, &agent, Some(&all_denied)), NONE);
    // Replacing the only write rule leaves reading.
    let write_denied = overlay(vec![rule(
        PermissionAction::MEMORY_WRITE,
        "user://*",
        PermissionEffect::Deny,
    )]);
    assert_eq!(memory_access(true, &agent, Some(&write_denied)), READ_ONLY);
    // A narrower deny keeps both.
    let narrow = overlay(vec![rule(
        PermissionAction::MEMORY,
        "user://profile",
        PermissionEffect::Deny,
    )]);
    assert_eq!(memory_access(true, &agent, Some(&narrow)), BOTH);

    let caller = MemoryCaller {
        agent,
        overlay: star_denied,
        related: super::RelatedMemory::default(),
    };
    let visibility = |url: &str| caller.visibility(&MemoryUrl::parse(url).unwrap());
    assert_eq!(visibility("user://profile"), Visibility::Hidden);
    assert_eq!(visibility("agent://reviewer"), Visibility::Full);
    assert!(caller.editable(&MemoryUrl::parse("user://profile").unwrap()));
}
