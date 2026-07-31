//! Explainable permission evaluation and tree-scoped runtime approvals.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookiecode_config::{PermissionEffect, PolicySnapshot, RuleSource, simple_wildcard_match};
use cookiecode_protocol::{
    ActionKind, ApprovalResource, DecisionTrace, Effect, MatchedPermissionRule, SessionId,
};
use thiserror::Error;
use tree_sitter::Parser;

#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("unknown permission action `{0}`")]
    UnknownAction(String),
    #[error("could not canonicalize path {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ApprovalKey {
    root: SessionId,
    action: ActionKind,
    pattern: String,
}

#[derive(Debug, Default)]
pub struct ApprovalStore {
    grants: Mutex<HashMap<ApprovalKey, ()>>,
}

impl ApprovalStore {
    pub fn grant(&self, root: SessionId, action: ActionKind, pattern: String) {
        self.grants.lock().expect("approval lock poisoned").insert(
            ApprovalKey {
                root,
                action,
                pattern,
            },
            (),
        );
    }
    #[must_use]
    pub fn allows(&self, root: SessionId, action: ActionKind, resource: &str) -> bool {
        self.grants
            .lock()
            .expect("approval lock poisoned")
            .keys()
            .any(|key| {
                key.root == root
                    && key.action == action
                    && simple_wildcard_match(&key.pattern, resource)
            })
    }
}

#[derive(Clone, Debug)]
pub struct PermissionDecision {
    pub effect: Effect,
    pub trace: DecisionTrace,
    pub always_allowed: bool,
    pub asking_resources: Vec<ApprovalResource>,
}

#[derive(Debug, Default)]
pub struct PermissionPipeline {
    consecutive: Mutex<HashMap<SessionId, (ActionKind, String, u8)>>,
}

impl PermissionPipeline {
    pub fn action_for_tool(tool: &str) -> Result<ActionKind, PermissionError> {
        match tool {
            "read" => Ok(ActionKind::Read),
            "write" | "edit" => Ok(ActionKind::Write),
            "bash" => Ok(ActionKind::Bash),
            "grep" => Ok(ActionKind::Grep),
            "glob" => Ok(ActionKind::Glob),
            "list" => Ok(ActionKind::List),
            "delegate" => Ok(ActionKind::Delegate),
            "external_directory" => Ok(ActionKind::ExternalDirectory),
            other => Err(PermissionError::UnknownAction(other.into())),
        }
    }

    pub fn decide(
        &self,
        policy: &PolicySnapshot,
        approvals: &ApprovalStore,
        root: SessionId,
        session: SessionId,
        action: ActionKind,
        resource: String,
    ) -> PermissionDecision {
        self.decide_resources(policy, approvals, root, session, vec![(action, resource)])
    }

    pub fn decide_resources(
        &self,
        policy: &PolicySnapshot,
        approvals: &ApprovalStore,
        root: SessionId,
        session: SessionId,
        resources: Vec<(ActionKind, String)>,
    ) -> PermissionDecision {
        let signature = resources
            .iter()
            .map(|(action, resource)| format!("{action:?}:{resource}"))
            .collect::<Vec<_>>()
            .join("\u{1f}");
        let action = resources
            .first()
            .map(|(action, _)| *action)
            .unwrap_or(ActionKind::Read);
        let decisions = resources
            .iter()
            .map(|(action, resource)| {
                base_decide(policy, approvals, root, *action, resource.clone())
            })
            .collect::<Vec<_>>();
        let mut selected = decisions
            .iter()
            .find(|decision| {
                decision.effect == Effect::Deny
                    && decision.trace.candidates.iter().any(|rule| rule.hard)
            })
            .or_else(|| {
                decisions
                    .iter()
                    .find(|decision| decision.effect == Effect::Deny)
            })
            .or_else(|| {
                decisions
                    .iter()
                    .find(|decision| decision.effect == Effect::Ask)
            })
            .or_else(|| decisions.first())
            .cloned()
            .expect("permission resources are non-empty");
        let count = {
            let mut calls = self.consecutive.lock().expect("permission lock poisoned");
            match calls.get_mut(&session) {
                Some((previous_action, previous_resource, count))
                    if *previous_action == action && *previous_resource == signature =>
                {
                    *count = count.saturating_add(1);
                    *count
                }
                _ => {
                    calls.insert(session, (action, signature, 1));
                    1
                }
            }
        };
        if count >= 3 && selected.effect != Effect::Deny {
            selected.effect = Effect::Ask;
            selected.trace.effect = Effect::Ask;
            selected.trace.precedence_reason = "doom-loop guard (third identical call)".into();
            selected.always_allowed = false;
            selected.asking_resources = resources
                .iter()
                .map(|(action, resource)| ApprovalResource {
                    action: *action,
                    resource: resource.clone(),
                    suggested_pattern: format!("{resource} *"),
                })
                .collect();
        }
        if selected.effect == Effect::Ask && selected.asking_resources.is_empty() {
            selected.asking_resources = decisions
                .iter()
                .filter(|decision| decision.effect == Effect::Ask)
                .map(|decision| ApprovalResource {
                    action: decision.trace.action,
                    resource: decision.trace.normalized_resource.clone(),
                    suggested_pattern: format!("{} *", decision.trace.normalized_resource),
                })
                .collect();
        }
        selected
    }

    pub fn reset_call_streak(&self, session: SessionId) {
        self.consecutive
            .lock()
            .expect("permission lock poisoned")
            .retain(|id, _| *id != session);
    }
}

fn base_decide(
    policy: &PolicySnapshot,
    approvals: &ApprovalStore,
    root: SessionId,
    action: ActionKind,
    resource: String,
) -> PermissionDecision {
    let candidates = matching_rules(policy, action, &resource);
    if let Some(rule) = candidates
        .iter()
        .find(|rule| rule.hard && rule.effect == Effect::Deny)
    {
        let reason = format!(
            "hard deny rule {}",
            rule.rule_id.as_deref().unwrap_or("builtin")
        );
        return decision(action, resource, candidates, Effect::Deny, reason, false);
    }
    if approvals.allows(root, action, &resource) {
        return decision(
            action,
            resource,
            candidates,
            Effect::Allow,
            "tree-shared runtime approval".into(),
            true,
        );
    }
    if let Some(rule) = candidates.last() {
        let effect = rule.effect;
        return decision(
            action,
            resource,
            candidates,
            effect,
            "last matching rule".into(),
            effect != Effect::Ask,
        );
    }
    let effect = tier(policy, action);
    decision(
        action,
        resource,
        candidates,
        effect,
        "tier default".into(),
        effect != Effect::Ask,
    )
}

fn decision(
    action: ActionKind,
    normalized_resource: String,
    candidates: Vec<MatchedPermissionRule>,
    effect: Effect,
    precedence_reason: String,
    always_allowed: bool,
) -> PermissionDecision {
    PermissionDecision {
        effect,
        trace: DecisionTrace {
            action,
            normalized_resource,
            candidates,
            effect,
            precedence_reason,
        },
        always_allowed,
        asking_resources: Vec::new(),
    }
}

fn matching_rules(
    policy: &PolicySnapshot,
    action: ActionKind,
    resource: &str,
) -> Vec<MatchedPermissionRule> {
    let mut rules = Vec::new();
    // The documented guard defaults are lower priority than configuration.
    if action == ActionKind::Read && is_dotenv(resource) {
        rules.push(MatchedPermissionRule {
            rule_id: None,
            source_layer: "builtin".into(),
            effect: Effect::Ask,
            hard: false,
        });
    }
    if action == ActionKind::ExternalDirectory {
        rules.push(MatchedPermissionRule {
            rule_id: None,
            source_layer: "builtin".into(),
            effect: Effect::Ask,
            hard: false,
        });
    }
    for rule in &policy.permissions.rules {
        if action_from_config(&rule.action).ok() == Some(action)
            && simple_wildcard_match(&rule.resource, resource)
        {
            rules.push(MatchedPermissionRule {
                rule_id: Some(rule.id.clone()),
                source_layer: source(rule.source).into(),
                effect: effect(&rule.effect),
                hard: rule.hard,
            });
        }
    }
    rules
}

fn action_from_config(value: &str) -> Result<ActionKind, PermissionError> {
    PermissionPipeline::action_for_tool(value)
}
fn effect(value: &str) -> Effect {
    match value {
        "allow" => Effect::Allow,
        "deny" => Effect::Deny,
        _ => Effect::Ask,
    }
}
fn source(source: RuleSource) -> &'static str {
    match source {
        RuleSource::Builtin => "builtin",
        RuleSource::User => "user",
        RuleSource::Workspace => "workspace",
        RuleSource::Env => "env",
        RuleSource::Profile => "profile",
    }
}
fn tier(policy: &PolicySnapshot, action: ActionKind) -> Effect {
    let source = match action {
        ActionKind::Read | ActionKind::List | ActionKind::Grep | ActionKind::Glob => {
            policy.permissions.read
        }
        ActionKind::Write => policy.permissions.write,
        ActionKind::Bash => policy.permissions.exec,
        ActionKind::Delegate => policy.permissions.delegate,
        // Config has no separate external-directory tier yet.  The built-in
        // ask guard is the documented fallback; matching configured rules may
        // still override it above.
        ActionKind::ExternalDirectory => return Effect::Ask,
    };
    match source {
        PermissionEffect::Allow => Effect::Allow,
        PermissionEffect::Ask => Effect::Ask,
        PermissionEffect::Deny => Effect::Deny,
    }
}
fn is_dotenv(resource: &str) -> bool {
    let name = Path::new(resource)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    (name == ".env" || name.starts_with(".env.")) && !name.ends_with(".example")
}

/// Canonicalizes the nearest existing ancestor so a not-yet-created write
/// target is classified safely and symlink-aware.
pub fn canonical_resource(
    workspace: &Path,
    target: &Path,
) -> Result<(String, bool), PermissionError> {
    let workspace = workspace
        .canonicalize()
        .map_err(|source| PermissionError::Canonicalize {
            path: workspace.into(),
            source,
        })?;
    let absolute = if target.is_absolute() {
        target.to_owned()
    } else {
        workspace.join(target)
    };
    let mut ancestor = absolute.clone();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name().map(|name| name.to_owned()) else {
            break;
        };
        suffix.push(name);
        if !ancestor.pop() {
            break;
        }
    }
    let mut normalized =
        ancestor
            .canonicalize()
            .map_err(|source| PermissionError::Canonicalize {
                path: ancestor.clone(),
                source,
            })?;
    for component in suffix.iter().rev() {
        normalized.push(component);
    }
    let external = !normalized.starts_with(&workspace);
    Ok(if external {
        (normalized.to_string_lossy().into_owned(), true)
    } else {
        (
            normalized
                .strip_prefix(&workspace)
                .unwrap_or(&normalized)
                .to_string_lossy()
                .into_owned(),
            false,
        )
    })
}

/// Extracts every tree-sitter `command` node, including commands nested in
/// pipelines, substitutions, and boolean lists. On parse failure it returns
/// the whole input, as required by the policy contract.
#[must_use]
pub fn bash_subcommands(source: &str) -> Vec<String> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return vec![source.into()];
    }
    let Some(tree) = parser.parse(source, None) else {
        return vec![source.into()];
    };
    let mut nodes = Vec::new();
    let mut iterator = tree.walk();
    loop {
        let node = iterator.node();
        if node.kind() == "command" {
            nodes.push(
                node.utf8_text(source.as_bytes())
                    .unwrap_or(source)
                    .trim()
                    .to_owned(),
            );
        }
        if iterator.goto_first_child() {
            continue;
        }
        loop {
            if iterator.goto_next_sibling() {
                break;
            }
            if !iterator.goto_parent() {
                return if nodes.is_empty() {
                    vec![source.into()]
                } else {
                    nodes
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::Path};

    use cookiecode_config::{
        DelegationPolicy, DepthLimit, PermissionEffect, PermissionRule, PolicySnapshot,
        ProfileSnapshot, ResolvedPermissions, ResultLimits, RuleSource,
    };
    use cookiecode_protocol::{ActionKind, Effect, SessionId};
    use uuid::Uuid;

    use super::{ApprovalStore, PermissionPipeline, bash_subcommands, canonical_resource};

    fn policy(rules: Vec<PermissionRule>) -> PolicySnapshot {
        PolicySnapshot {
            profile: ProfileSnapshot {
                name: "test".into(),
                r#type: cookiecode_config::AgentType::All,
            },
            models: Vec::new(),
            tools: BTreeSet::new(),
            permissions: ResolvedPermissions {
                read: PermissionEffect::Ask,
                write: PermissionEffect::Ask,
                exec: PermissionEffect::Ask,
                delegate: PermissionEffect::Ask,
                rules,
            },
            delegation: DelegationPolicy {
                enabled: false,
                allowed_profiles: BTreeSet::new(),
                depth_limit: DepthLimit::Unlimited,
            },
            result_limits: ResultLimits::default(),
        }
    }

    #[test]
    fn hard_deny_precedes_approval_and_last_match_wins() {
        let id = SessionId(Uuid::from_u128(1));
        let rules = vec![
            PermissionRule {
                id: "allow".into(),
                action: "bash".into(),
                resource: "git *".into(),
                effect: "allow".into(),
                hard: false,
                source: RuleSource::Profile,
            },
            PermissionRule {
                id: "deny".into(),
                action: "bash".into(),
                resource: "git push *".into(),
                effect: "deny".into(),
                hard: true,
                source: RuleSource::Profile,
            },
        ];
        let approvals = ApprovalStore::default();
        approvals.grant(id, ActionKind::Bash, "*".into());
        let decision = PermissionPipeline::default().decide(
            &policy(rules),
            &approvals,
            id,
            id,
            ActionKind::Bash,
            "git push --force".into(),
        );
        assert_eq!(decision.effect, Effect::Deny);
        assert!(decision.trace.precedence_reason.contains("hard deny"));
    }

    #[test]
    fn shell_parser_visits_boolean_list_commands() {
        let commands = bash_subcommands("git status && git log -1");
        assert!(commands.iter().any(|command| command == "git status"));
        assert!(commands.iter().any(|command| command == "git log -1"));
    }

    #[test]
    fn aggregate_resources_denies_regardless_of_ask_deny_order_and_discloses_all_asks() {
        let id = SessionId(Uuid::from_u128(2));
        let rules = vec![
            PermissionRule {
                id: "ask".into(),
                action: "bash".into(),
                resource: "ask command".into(),
                effect: "ask".into(),
                hard: false,
                source: RuleSource::Profile,
            },
            PermissionRule {
                id: "deny".into(),
                action: "bash".into(),
                resource: "deny command".into(),
                effect: "deny".into(),
                hard: false,
                source: RuleSource::Profile,
            },
            PermissionRule {
                id: "ask-two".into(),
                action: "bash".into(),
                resource: "another ask".into(),
                effect: "ask".into(),
                hard: false,
                source: RuleSource::Profile,
            },
        ];
        let pipeline = PermissionPipeline::default();
        let approvals = ApprovalStore::default();
        for resources in [
            vec![
                (ActionKind::Bash, "ask command".into()),
                (ActionKind::Bash, "deny command".into()),
            ],
            vec![
                (ActionKind::Bash, "deny command".into()),
                (ActionKind::Bash, "ask command".into()),
            ],
        ] {
            assert_eq!(
                pipeline
                    .decide_resources(&policy(rules.clone()), &approvals, id, id, resources)
                    .effect,
                Effect::Deny
            );
        }
        let decision = pipeline.decide_resources(
            &policy(rules),
            &approvals,
            id,
            id,
            vec![
                (ActionKind::Bash, "ask command".into()),
                (ActionKind::Bash, "another ask".into()),
            ],
        );
        assert_eq!(decision.effect, Effect::Ask);
        assert_eq!(
            decision
                .asking_resources
                .iter()
                .map(|resource| resource.resource.as_str())
                .collect::<Vec<_>>(),
            vec!["ask command", "another ask"]
        );
    }

    #[test]
    fn doom_loop_uses_complete_signature_and_different_signature_resets_streak() {
        let id = SessionId(Uuid::from_u128(3));
        let rules = vec![PermissionRule {
            id: "allow-bash".into(),
            action: "bash".into(),
            resource: "*".into(),
            effect: "allow".into(),
            hard: false,
            source: RuleSource::Profile,
        }];
        let pipeline = PermissionPipeline::default();
        let approvals = ApprovalStore::default();
        let signature = || {
            vec![
                (ActionKind::Bash, "git status".into()),
                (ActionKind::Bash, "git log -1".into()),
            ]
        };
        assert_eq!(
            pipeline
                .decide_resources(&policy(rules.clone()), &approvals, id, id, signature())
                .effect,
            Effect::Allow
        );
        assert_eq!(
            pipeline
                .decide_resources(&policy(rules.clone()), &approvals, id, id, signature())
                .effect,
            Effect::Allow
        );
        let doom =
            pipeline.decide_resources(&policy(rules.clone()), &approvals, id, id, signature());
        assert_eq!(doom.effect, Effect::Ask);
        assert_eq!(doom.asking_resources.len(), 2);
        assert!(doom.trace.precedence_reason.starts_with("doom-loop guard"));
        assert_eq!(
            pipeline
                .decide_resources(
                    &policy(rules),
                    &approvals,
                    id,
                    id,
                    vec![
                        (ActionKind::Bash, "git status".into()),
                        (ActionKind::Bash, "git diff".into()),
                    ],
                )
                .effect,
            Effect::Allow
        );
    }

    #[test]
    fn external_directory_resource_remains_asked_when_read_tier_allows() {
        let id = SessionId(Uuid::from_u128(4));
        let mut policy = policy(Vec::new());
        policy.permissions.read = PermissionEffect::Allow;
        let approvals = ApprovalStore::default();
        let pipeline = PermissionPipeline::default();
        let resources = vec![
            (
                ActionKind::ExternalDirectory,
                "/outside/workspace/file".into(),
            ),
            (ActionKind::Read, "/outside/workspace/file".into()),
        ];
        let decision = pipeline.decide_resources(&policy, &approvals, id, id, resources.clone());
        assert_eq!(decision.effect, Effect::Ask);
        assert_eq!(decision.trace.action, ActionKind::ExternalDirectory);
        assert_eq!(decision.asking_resources.len(), 1);
        assert_eq!(
            decision.asking_resources[0].action,
            ActionKind::ExternalDirectory
        );
        approvals.grant(
            id,
            ActionKind::ExternalDirectory,
            "/outside/workspace/file *".into(),
        );
        assert_eq!(
            pipeline
                .decide_resources(&policy, &approvals, id, id, resources)
                .effect,
            Effect::Allow
        );
    }

    #[test]
    fn canonicalization_failure_fallback_still_uses_external_directory_guard() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing_workspace = directory.path().join("missing-workspace");
        assert!(canonical_resource(&missing_workspace, Path::new("outside-file")).is_err());

        let id = SessionId(Uuid::from_u128(5));
        let mut policy = policy(Vec::new());
        policy.permissions.read = PermissionEffect::Allow;
        let decision = PermissionPipeline::default().decide_resources(
            &policy,
            &ApprovalStore::default(),
            id,
            id,
            vec![
                (ActionKind::ExternalDirectory, "outside-file".into()),
                (ActionKind::Read, "outside-file".into()),
            ],
        );
        assert_eq!(decision.effect, Effect::Ask);
        assert_eq!(decision.trace.action, ActionKind::ExternalDirectory);
    }
}
