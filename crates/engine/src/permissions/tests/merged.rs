//! The merged rule list of `docs/site/specs/permission-consolidation.md`:
//! defaults, the agent document, and the session's overrides as one list
//! that every decision reads.

use cookie_agent_protocol::{
    AgentMode, AgentSnapshot, PermissionAction,
    PermissionEffect::{self, Allow, Ask, Deny},
    PermissionRule, PermissionRuleSource, SessionPermissionOverlay,
};

use super::{agent_document, operation, policy, qualified, resource, resources, root_rules, rule};
use crate::permissions::{
    MergedRules, OwnedScope, PermissionPipeline, PermissionScope, effective_permission_view,
    rule_effects, stricter,
};

fn session(rules: Vec<PermissionRule>) -> SessionPermissionOverlay {
    SessionPermissionOverlay { rules }
}

fn allow(action: PermissionAction, pattern: &str) -> PermissionRule {
    rule("allow", action, pattern, Allow)
}

fn ask(action: PermissionAction, pattern: &str) -> PermissionRule {
    rule("ask", action, pattern, Ask)
}

fn deny(action: PermissionAction, pattern: &str) -> PermissionRule {
    rule("deny", action, pattern, Deny)
}

/// The frozen policy of a root run of a `primary` agent whose document
/// declares `permissions`, defaults included.
fn root_policy(permissions: &[(&str, cookie_agent_config::PermissionValue)]) -> AgentSnapshot {
    let document = agent_document("builder", AgentMode::Primary, permissions);
    let mut agent = policy(root_rules(&document));
    agent.agent = document.id;
    agent
}

/// The effect of `action` on `label` under `agent` and `overlay` in `scope`.
fn decide(
    agent: &AgentSnapshot,
    overlay: &SessionPermissionOverlay,
    action: PermissionAction,
    label: &str,
    scope: PermissionScope<'_>,
) -> PermissionEffect {
    PermissionPipeline::default()
        .decide_operation_with_overlay(
            agent,
            Some(overlay),
            &operation(vec![resource(action, label, label.as_bytes())]),
            &[Some(label.to_owned())],
            scope,
        )
        .effect
}

/// The effect of `action` checked by name only under `agent` and `overlay`
/// in `scope`.
fn decide_name_only(
    agent: &AgentSnapshot,
    overlay: &SessionPermissionOverlay,
    action: PermissionAction,
    scope: PermissionScope<'_>,
) -> PermissionEffect {
    PermissionPipeline::default()
        .decide_operation_with_overlay(
            agent,
            Some(overlay),
            &operation(vec![resource(action, "name-only", b"name-only")]),
            &[None],
            scope,
        )
        .effect
}

fn bash(
    agent: &AgentSnapshot,
    overlay: &SessionPermissionOverlay,
    command: &str,
) -> PermissionEffect {
    decide(
        agent,
        overlay,
        PermissionAction::BASH,
        command,
        PermissionScope::BASE,
    )
}

fn offered(agent: &AgentSnapshot, overlay: &SessionPermissionOverlay, name: &str) -> bool {
    PermissionPipeline::tool_visible_with_overlay(agent, Some(overlay), name, Some("global"))
}

const GLOBAL: PermissionScope<'static> = PermissionScope::runtime("global", None);

type Row = (
    String,
    Option<String>,
    String,
    PermissionEffect,
    PermissionRuleSource,
);

/// The merged rules of `action`, as `(action, runtime, pattern, effect,
/// source)`.
fn merged_rows(merged: &MergedRules<'_>, action: &PermissionAction) -> Vec<Row> {
    merged
        .rules()
        .iter()
        .filter(|merged| merged.rule.action == *action)
        .map(|merged| {
            (
                merged.rule.action.to_string(),
                merged.rule.runtime.as_ref().map(ToString::to_string),
                merged.rule.resource.as_str().to_owned(),
                merged.rule.effect,
                merged.source,
            )
        })
        .collect()
}

fn row(
    action: &str,
    runtime: Option<&str>,
    pattern: &str,
    effect: PermissionEffect,
    source: PermissionRuleSource,
) -> Row {
    (
        action.to_owned(),
        runtime.map(str::to_owned),
        pattern.to_owned(),
        effect,
        source,
    )
}

// Building the list.

#[test]
fn a_session_rule_replaces_the_rule_with_its_identity_at_any_level() {
    use PermissionRuleSource::{AgentDocument, Default, SessionOverlay};
    let agent = root_policy(&[("bash", resources(&[("*", Ask), ("git push *", Deny)]))]);
    let overlay = session(vec![
        allow(PermissionAction::BASH, "*"),
        allow(PermissionAction::BASH, "cargo *"),
        deny(PermissionAction::LIST_RUNTIMES, "*"),
    ]);
    let merged = MergedRules::of(&agent, Some(&overlay), None).label_defaults(&agent, false);
    // The document's `*` is replaced; its other pattern stays, before the
    // session's rules.
    assert_eq!(
        merged_rows(&merged, &PermissionAction::BASH),
        [
            row("bash", None, "git push *", Deny, AgentDocument),
            row("bash", None, "*", Allow, SessionOverlay),
            row("bash", None, "cargo *", Allow, SessionOverlay),
        ]
    );
    // A default is replaced the same way.
    assert_eq!(
        merged_rows(&merged, &PermissionAction::LIST_RUNTIMES),
        [row("list_runtimes", None, "*", Deny, SessionOverlay)]
    );
    // Untouched defaults keep their source.
    assert_eq!(
        merged_rows(&merged, &PermissionAction::MCP),
        [row("mcp", None, "*", Ask, Default)]
    );
    // Without `label_defaults` the policy's rules all read as the document's.
    let unlabelled = MergedRules::of(&agent, Some(&overlay), None);
    assert_eq!(
        merged_rows(&unlabelled, &PermissionAction::MCP),
        [row("mcp", None, "*", Ask, AgentDocument)]
    );
}

#[test]
fn runtime_selectors_are_distinct_identities() {
    use PermissionRuleSource::{AgentDocument, SessionOverlay};
    let agent = policy(vec![
        ask(PermissionAction::BASH, "*"),
        qualified(PermissionAction::BASH, "labpc", "*", Allow),
    ]);
    let labpc = PermissionScope::runtime("labpc", None);
    let local = PermissionScope::runtime("local", None);
    // `bash@labpc` replaces only the document's `bash@labpc`.
    let overlay = session(vec![qualified(PermissionAction::BASH, "labpc", "*", Deny)]);
    assert_eq!(
        merged_rows(
            &MergedRules::of(&agent, Some(&overlay), None),
            &PermissionAction::BASH
        ),
        [
            row("bash", None, "*", Ask, AgentDocument),
            row("bash", Some("labpc"), "*", Deny, SessionOverlay),
        ]
    );
    assert_eq!(
        decide(&agent, &overlay, PermissionAction::BASH, "ls", labpc),
        Deny
    );
    assert_eq!(
        decide(&agent, &overlay, PermissionAction::BASH, "ls", local),
        Ask
    );
    // A bare `bash` replaces only the bare rule, and at equal specificity the
    // document's runtime-selected rule still wins on its runtime.
    let overlay = session(vec![deny(PermissionAction::BASH, "*")]);
    assert_eq!(
        decide(&agent, &overlay, PermissionAction::BASH, "ls", labpc),
        Allow
    );
    assert_eq!(
        decide(&agent, &overlay, PermissionAction::BASH, "ls", local),
        Deny
    );
}

#[test]
fn home_patterns_are_compared_as_matching_resolves_them() {
    let agent = policy(vec![allow(PermissionAction::READ, "~/notes/*")]);
    let overlay = session(vec![deny(PermissionAction::READ, "/home/ada/notes/*")]);
    let ada = PermissionScope::runtime("labpc", None).with_home(Some("/home/ada"));
    // On a runtime whose home is `/home/ada` the two are one identity: the
    // session's rule replaces the document's, so nothing offers `read`.
    assert_eq!(
        MergedRules::of(&agent, Some(&overlay), ada.home)
            .rules()
            .len(),
        1
    );
    assert_eq!(
        decide(
            &agent,
            &overlay,
            PermissionAction::READ,
            "/home/ada/notes/todo.md",
            ada
        ),
        Deny
    );
    assert!(!PermissionPipeline::tool_visible_with_grants(
        &agent,
        Some(&overlay),
        None,
        "read",
        ada
    ));
    // Elsewhere `~/notes/*` names another home, so both rules stay.
    let bob = PermissionScope::runtime("laptop", None).with_home(Some("/home/bob"));
    assert_eq!(
        MergedRules::of(&agent, Some(&overlay), bob.home)
            .rules()
            .len(),
        2
    );
    assert_eq!(
        decide(
            &agent,
            &overlay,
            PermissionAction::READ,
            "/home/bob/notes/todo.md",
            bob
        ),
        Allow
    );
    assert!(PermissionPipeline::tool_visible_with_grants(
        &agent,
        Some(&overlay),
        None,
        "read",
        bob
    ));
    // The same spelling is one identity with or without a home.
    let overlay = session(vec![deny(PermissionAction::READ, "~/notes/*")]);
    assert_eq!(
        MergedRules::of(&agent, Some(&overlay), None).rules().len(),
        1
    );
}

#[test]
fn memory_rules_are_compared_with_their_variables_expanded() {
    // The frozen policy holds `agent://builder` for the default's
    // `agent://${agent_id}`, which a session rule naming it replaces.
    let agent = root_policy(&[]);
    let overlay = session(vec![deny(
        PermissionAction::MEMORY_WRITE,
        "agent://builder",
    )]);
    let merged = MergedRules::of(&agent, Some(&overlay), None);
    let builder = merged_rows(&merged, &PermissionAction::MEMORY_WRITE)
        .into_iter()
        .filter(|(_, _, pattern, _, _)| pattern == "agent://builder")
        .collect::<Vec<_>>();
    assert_eq!(
        builder,
        [row(
            "memory_write",
            None,
            "agent://builder",
            Deny,
            PermissionRuleSource::SessionOverlay
        )]
    );
    let write = |url: &str| {
        decide(
            &agent,
            &overlay,
            PermissionAction::MEMORY_WRITE,
            url,
            GLOBAL,
        )
    };
    assert_eq!(write("agent://builder"), Deny);
    assert_eq!(write("user://feedback"), Allow);
}

// Which tools are offered.

#[test]
fn a_session_deny_hides_a_tool_a_default_allows() {
    let agent = root_policy(&[]);
    let none = session(Vec::new());
    for action in [
        PermissionAction::MEMORY,
        PermissionAction::CRON,
        PermissionAction::MCP,
        PermissionAction::LIST_RUNTIMES,
    ] {
        assert!(
            offered(&agent, &none, action.as_str()),
            "{action} by default"
        );
        let overlay = session(vec![deny(action.clone(), "*")]);
        assert!(
            !offered(&agent, &overlay, action.as_str()),
            "{action} denied"
        );
    }
}

#[test]
fn a_narrower_session_deny_leaves_the_tool_offered() {
    let agent = root_policy(&[]);
    for (action, pattern) in [
        (PermissionAction::MEMORY, "user://*"),
        (PermissionAction::CRON, "new"),
        (PermissionAction::MCP, "github_*"),
        (PermissionAction::LIST_RUNTIMES, "runtimes"),
    ] {
        let overlay = session(vec![deny(action.clone(), pattern)]);
        assert!(
            offered(&agent, &overlay, action.as_str()),
            "{action} {pattern}"
        );
    }
}

#[test]
fn a_session_allow_offers_a_tool_the_document_denies() {
    let agent = root_policy(&[("bash", cookie_agent_config::PermissionValue::Effect(Deny))]);
    assert!(!offered(&agent, &session(Vec::new()), "bash"));
    let narrow = session(vec![allow(PermissionAction::BASH, "git status")]);
    assert!(offered(&agent, &narrow, "bash"));
    assert_eq!(bash(&agent, &narrow, "git status"), Allow);
    assert_eq!(bash(&agent, &narrow, "ls"), Deny);
    let broad = session(vec![allow(PermissionAction::BASH, "*")]);
    assert!(offered(&agent, &broad, "bash"));
    assert_eq!(bash(&agent, &broad, "ls"), Allow);
}

// The examples of section 3.3, one per row.

#[test]
fn row_1_a_session_cron_deny_replaces_the_default_and_hides_cron() {
    let agent = root_policy(&[]);
    let overlay = session(vec![deny(PermissionAction::CRON, "*")]);
    assert_eq!(
        merged_rows(
            &MergedRules::of(&agent, Some(&overlay), None),
            &PermissionAction::CRON
        ),
        [row(
            "cron",
            None,
            "*",
            Deny,
            PermissionRuleSource::SessionOverlay
        )]
    );
    assert!(!offered(&agent, &overlay, "cron"));
}

#[test]
fn row_2_a_session_memory_deny_for_user_keeps_the_rest_readable() {
    let agent = root_policy(&[]);
    let overlay = session(vec![deny(PermissionAction::MEMORY, "user://*")]);
    assert!(offered(&agent, &overlay, "memory"));
    let read = |url: &str| decide(&agent, &overlay, PermissionAction::MEMORY, url, GLOBAL);
    assert_eq!(read("user://profile"), Deny);
    assert_eq!(read("agent://builder"), Allow);
    assert_eq!(read("project://web/build"), Allow);
}

#[test]
fn row_3_a_session_allow_for_cargo_test_runs_and_the_rest_asks() {
    let agent = policy(vec![ask(PermissionAction::BASH, "*")]);
    let overlay = session(vec![allow(PermissionAction::BASH, "cargo test *")]);
    assert!(offered(&agent, &overlay, "bash"));
    assert_eq!(bash(&agent, &overlay, "cargo test --workspace"), Allow);
    assert_eq!(bash(&agent, &overlay, "ls"), Ask);
}

#[test]
fn row_4_a_broad_session_allow_keeps_the_documents_git_push_deny() {
    let agent = policy(vec![
        ask(PermissionAction::BASH, "*"),
        deny(PermissionAction::BASH, "git push *"),
    ]);
    let overlay = session(vec![allow(PermissionAction::BASH, "*")]);
    assert!(offered(&agent, &overlay, "bash"));
    assert_eq!(bash(&agent, &overlay, "git push origin main"), Deny);
    assert_eq!(bash(&agent, &overlay, "ls"), Allow);
}

#[test]
fn row_5_a_session_allow_for_git_push_replaces_the_documents_deny() {
    let agent = policy(vec![
        ask(PermissionAction::BASH, "*"),
        deny(PermissionAction::BASH, "git push *"),
    ]);
    let overlay = session(vec![allow(PermissionAction::BASH, "git push *")]);
    assert!(offered(&agent, &overlay, "bash"));
    assert_eq!(bash(&agent, &overlay, "git push origin main"), Allow);
    assert_eq!(bash(&agent, &overlay, "ls"), Ask);
}

#[test]
fn row_6_a_session_mcp_deny_replaces_the_default_and_hides_mcp() {
    let agent = root_policy(&[]);
    let overlay = session(vec![deny(PermissionAction::MCP, "*")]);
    assert_eq!(
        merged_rows(
            &MergedRules::of(&agent, Some(&overlay), None),
            &PermissionAction::MCP
        ),
        [row(
            "mcp",
            None,
            "*",
            Deny,
            PermissionRuleSource::SessionOverlay
        )]
    );
    assert!(!offered(&agent, &overlay, "mcp"));
    assert_eq!(
        decide(
            &agent,
            &overlay,
            PermissionAction::MCP,
            "github_list",
            PermissionScope::BASE
        ),
        Deny
    );
}

#[test]
fn a_specificity_tie_across_levels_goes_to_the_session() {
    // Both patterns have five literal characters and one wildcard.
    let agent = policy(vec![deny(PermissionAction::BASH, "git s*")]);
    let overlay = session(vec![allow(PermissionAction::BASH, "git *s")]);
    assert_eq!(bash(&agent, &overlay, "git status"), Allow);
    // The more specific document rule still beats a session rule.
    let overlay = session(vec![allow(PermissionAction::BASH, "git *")]);
    assert_eq!(bash(&agent, &overlay, "git status"), Deny);
}

// Checks without a resource.

#[test]
fn a_name_only_check_reads_the_merged_star_rules() {
    let agent = root_policy(&[]);
    let overlay = session(vec![deny(PermissionAction::LIST_RUNTIMES, "*")]);
    assert_eq!(
        decide_name_only(
            &agent,
            &session(Vec::new()),
            PermissionAction::LIST_RUNTIMES,
            GLOBAL
        ),
        Allow
    );
    assert_eq!(
        decide_name_only(&agent, &overlay, PermissionAction::LIST_RUNTIMES, GLOBAL),
        Deny
    );
}

#[test]
fn a_name_only_check_prefers_a_runtime_selected_star_rule() {
    let agent = policy(vec![qualified(PermissionAction::BASH, "labpc", "*", Allow)]);
    let labpc = PermissionScope::runtime("labpc", None);
    let local = PermissionScope::runtime("local", None);
    // A bare session `*` does not replace the document's `bash@labpc`.
    let bare = session(vec![deny(PermissionAction::BASH, "*")]);
    assert_eq!(
        decide_name_only(&agent, &bare, PermissionAction::BASH, labpc),
        Allow
    );
    assert_eq!(
        decide_name_only(&agent, &bare, PermissionAction::BASH, local),
        Deny
    );
    // A session `bash@labpc` `*` replaces it.
    let selected = session(vec![qualified(PermissionAction::BASH, "labpc", "*", Ask)]);
    assert_eq!(
        decide_name_only(&agent, &selected, PermissionAction::BASH, labpc),
        Ask
    );
    assert_eq!(
        decide_name_only(&agent, &selected, PermissionAction::BASH, local),
        Deny
    );
}

// The `/permissions` view.

#[test]
fn the_view_keeps_the_documents_patterns_under_a_session_star() {
    use PermissionRuleSource::{AgentDocument, SessionOverlay};
    let agent = policy(vec![
        ask(PermissionAction::BASH, "*"),
        deny(PermissionAction::BASH, "git push *"),
        allow(PermissionAction::BASH, "git status"),
    ]);
    let overlay = session(vec![
        allow(PermissionAction::BASH, "*"),
        ask(PermissionAction::BASH, "git status"),
    ]);
    let view = effective_permission_view(&agent, false, &overlay, &[]);
    let bash = view
        .iter()
        .find(|entry| entry.action == PermissionAction::BASH)
        .expect("bash entry");
    assert_eq!((bash.effect, bash.source), (Allow, SessionOverlay));
    let patterns = bash
        .patterns
        .iter()
        .map(|rule| (rule.resource.as_str(), rule.effect, rule.source))
        .collect::<Vec<_>>();
    assert_eq!(
        patterns,
        [
            ("git push *", Deny, AgentDocument),
            ("git status", Ask, SessionOverlay),
        ]
    );
}

#[test]
fn the_view_reports_untouched_defaults_beside_session_rules() {
    let agent = root_policy(&[]);
    let overlay = session(vec![deny(PermissionAction::MEMORY, "user://*")]);
    let view = effective_permission_view(&agent, false, &overlay, &[]);
    let memory = view
        .iter()
        .find(|entry| entry.action == PermissionAction::MEMORY)
        .expect("memory entry");
    assert_eq!(
        (memory.effect, memory.source),
        (Allow, PermissionRuleSource::Default)
    );
    assert_eq!(memory.patterns.len(), 1);
    assert_eq!(memory.patterns[0].resource.as_str(), "user://*");
    assert_eq!(
        memory.patterns[0].source,
        PermissionRuleSource::SessionOverlay
    );
}

// Grants.

#[test]
fn a_grant_never_overrides_a_merged_deny() {
    let agent = policy(vec![ask(PermissionAction::BASH, "*")]);
    let overlay = session(vec![deny(PermissionAction::BASH, "git push *")]);
    let grants = session(vec![allow(PermissionAction::BASH, "git push *")]);
    let decide_granted = |command: &str| {
        PermissionPipeline::default()
            .decide_operation_with_grants(
                &agent,
                Some(&overlay),
                Some(&grants),
                &operation(vec![resource(
                    PermissionAction::BASH,
                    command,
                    command.as_bytes(),
                )]),
                &[Some(command.to_owned())],
                PermissionScope::BASE,
            )
            .effect
    };
    assert_eq!(decide_granted("git push origin"), Deny);
    assert_eq!(decide_granted("ls"), Ask);

    // A grant cannot offer a tool whose every rule the session denies.
    let allowed = policy(vec![allow(PermissionAction::BASH, "*")]);
    let denied = session(vec![deny(PermissionAction::BASH, "*")]);
    let grant = session(vec![allow(PermissionAction::BASH, "git status")]);
    assert!(!PermissionPipeline::tool_visible_with_grants(
        &allowed,
        Some(&denied),
        Some(&grant),
        "bash",
        PermissionScope::BASE
    ));
}

#[test]
fn stricter_is_judged_on_the_merged_list() {
    let agent = policy(vec![
        ask(PermissionAction::BASH, "*"),
        deny(PermissionAction::BASH, "git push *"),
    ]);
    let scopes = [OwnedScope::default()];
    let effects = |overlay: &SessionPermissionOverlay, resource: &str| {
        rule_effects(&agent, overlay, &PermissionAction::BASH, resource, &scopes)
    };
    let empty = session(Vec::new());
    // Allowing `*` loosens; denying it tightens.
    let allowed = session(vec![allow(PermissionAction::BASH, "*")]);
    assert!(!stricter(&effects(&empty, "*"), &effects(&allowed, "*")));
    let denied = session(vec![deny(PermissionAction::BASH, "*")]);
    assert!(stricter(&effects(&empty, "*"), &effects(&denied, "*")));
    // Clearing a session allow for `git push *` falls back to the
    // document's deny, which is stricter.
    let pushed = session(vec![allow(PermissionAction::BASH, "git push *")]);
    assert!(stricter(
        &effects(&pushed, "git push *"),
        &effects(&empty, "git push *")
    ));
    // A broad session allow leaves the document's narrower deny in force,
    // so clearing it does not change `git push *`.
    assert_eq!(effects(&allowed, "git push *"), [Deny]);
}
