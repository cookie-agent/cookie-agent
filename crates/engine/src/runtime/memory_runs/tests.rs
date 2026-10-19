use std::collections::BTreeSet;

use cookie_agent_protocol::{
    AgentId, AgentMode, MachineName, MemorySnapshot, MemorySnapshotAgent, MemorySnapshotMachine,
    PermissionAction, PermissionEffect, PermissionRule, RuntimeId, SessionPermissionOverlay,
    WildcardPattern,
};

use super::{
    MemoryAccess, MemoryCaller, SnapshotChange, SnapshotFacts, memory_access, snapshot_change,
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
    // Write rules without a read rule give nothing.
    let write_only = vec![allow(PermissionAction::MEMORY_WRITE)];
    assert_eq!(
        memory_access(true, &agent(AgentMode::Primary, write_only.clone()), None),
        NONE
    );
    // `ask` counts; `deny` does not.
    let ask = vec![rule(PermissionAction::MEMORY, "*", PermissionEffect::Ask)];
    assert_eq!(
        memory_access(true, &agent(AgentMode::Primary, ask), None),
        READ_ONLY
    );
    let deny = vec![
        rule(PermissionAction::MEMORY, "*", PermissionEffect::Deny),
        allow(PermissionAction::MEMORY_WRITE),
    ];
    assert_eq!(
        memory_access(true, &agent(AgentMode::Primary, deny), None),
        NONE
    );
    // A rule for another runtime does not reach `global`.
    let mut elsewhere = allow(PermissionAction::MEMORY);
    elsewhere.runtime = Some(RuntimeId::new("labpc").unwrap());
    assert_eq!(
        memory_access(true, &agent(AgentMode::Primary, vec![elsewhere]), None),
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

fn facts(access: MemoryAccess, agent: &str, delegated: bool, machines: &[&str]) -> SnapshotFacts {
    SnapshotFacts {
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
                    "agent://self",
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
    };
    let visibility = |url: &str| caller.visibility(&MemoryUrl::parse(url).unwrap());
    assert_eq!(visibility("project://web/build"), Visibility::Full);
    assert_eq!(visibility("user://profile"), Visibility::Summary);
    assert_eq!(visibility("agent://reviewer"), Visibility::Full);
    assert_eq!(visibility("agent://default"), Visibility::Hidden);
    assert_eq!(visibility("machine://labpc/x"), Visibility::Hidden);
    assert_eq!(visibility("machine://labpc/ok"), Visibility::Full);
}
