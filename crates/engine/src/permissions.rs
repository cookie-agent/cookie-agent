//! Permission evaluation over immutable prepared-operation manifests.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use cookie_agent_config::simple_wildcard_match;
use cookie_agent_protocol::{
    AgentId, AgentMode, AgentSnapshot, ApprovalEvaluation, DecisionTrace,
    EffectivePermissionAction, EffectivePermissionRule, EventPayload, MatchedPermissionRule,
    OperationFingerprint, PermissionAction, PermissionEffect, PermissionRule, PermissionRuleSource,
    PreparedOperationIdentity, RunSelection, RuntimeId, SafeCode, SessionId, SessionOrigin,
    SessionPermissionGetResult, SessionPermissionMutationResult, SessionPermissionOverlay,
    TreeApprovalGrant, TreeApprovalGrantId, WildcardPattern,
};
use thiserror::Error;

/// Display stand-in for a permission resource the tool did not scope.
pub(crate) const UNSCOPED_PERMISSION_RESOURCE_DISPLAY: &str = "<permission-name-only>";

/// Where a permission is evaluated: the runtime that offers the tool, that
/// runtime's workspace, and the home directory it reported.
///
/// A rule with a runtime selector (`bash@labpc`) only applies when `runtime`
/// is that runtime ID; the base layer (`runtime: None`) only sees unqualified
/// rules. `${workspace_dir}` and relative resources resolve against
/// `workspace`; without one they never match. A pattern `~` or `~/…`
/// expands against `home`; without one it stays literal, so it matches no
/// absolute path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PermissionScope<'a> {
    pub runtime: Option<&'a str>,
    pub workspace: Option<&'a Path>,
    pub home: Option<&'a str>,
}

impl<'a> PermissionScope<'a> {
    /// The base layer: no runtime, no workspace, no home.
    pub const BASE: Self = Self {
        runtime: None,
        workspace: None,
        home: None,
    };

    /// The scope of a tool from `origin`.
    #[must_use]
    pub fn of(origin: &'a crate::ToolOrigin) -> Self {
        let runtime = origin.runtime();
        Self {
            runtime: runtime.map(cookie_agent_runtime_api::RuntimeInfo::id),
            workspace: origin.workspace_root(),
            home: runtime.and_then(cookie_agent_runtime_api::RuntimeInfo::home),
        }
    }

    /// The scope of the runtime with ID `runtime` working in `workspace`,
    /// which reported no home.
    #[must_use]
    pub const fn runtime(runtime: &'a str, workspace: Option<&'a Path>) -> Self {
        Self {
            runtime: Some(runtime),
            workspace,
            home: None,
        }
    }

    /// This scope on a runtime that reported `home`.
    #[must_use]
    pub const fn with_home(self, home: Option<&'a str>) -> Self {
        Self { home, ..self }
    }
}

/// Only unqualified rules, resolved against `workspace`.
impl<'a> From<Option<&'a Path>> for PermissionScope<'a> {
    fn from(workspace: Option<&'a Path>) -> Self {
        Self {
            runtime: None,
            workspace,
            home: None,
        }
    }
}

/// Visibility is action-level opt-in, not a resource authorization decision.
/// `runtime` is the runtime ID of the tool (`None` for the base layer).
#[must_use]
pub fn tool_visible(
    rules: &[PermissionRule],
    overlay: Option<&SessionPermissionOverlay>,
    action: &PermissionAction,
    runtime: Option<&str>,
) -> bool {
    rules
        .iter()
        .chain(overlay.into_iter().flat_map(|overlay| &overlay.rules))
        .any(|rule| {
            rule.action == *action
                && rule.applies_to(runtime)
                && matches!(rule.effect, PermissionEffect::Allow | PermissionEffect::Ask)
        })
}

#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("invalid permission name `{0}`")]
    UnknownAction(String),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ApprovalKey {
    root: SessionId,
    fingerprint: OperationFingerprint,
    /// Runtime ID of the granted tool; `None` for the base layer.
    runtime: Option<cookie_agent_protocol::RuntimeId>,
}

#[derive(Debug, Default)]
pub struct ApprovalStore {
    grants: Mutex<HashMap<ApprovalKey, TreeApprovalGrant>>,
}

impl ApprovalStore {
    pub fn replace(&self, grants: impl IntoIterator<Item = TreeApprovalGrant>) {
        let mut stored = self
            .grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stored.clear();
        for grant in grants {
            stored.insert(
                ApprovalKey {
                    root: grant.root_session_id,
                    fingerprint: grant.operation_fingerprint.clone(),
                    runtime: grant.runtime_id.clone(),
                },
                grant,
            );
        }
    }

    pub fn grant(&self, grant: TreeApprovalGrant) {
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                ApprovalKey {
                    root: grant.root_session_id,
                    fingerprint: grant.operation_fingerprint.clone(),
                    runtime: grant.runtime_id.clone(),
                },
                grant,
            );
    }

    /// The grant of tree `root` for `operation` on the runtime with ID
    /// `runtime` (`None` for the base layer). A grant only applies to calls
    /// on the runtime ID it was given for.
    #[must_use]
    pub fn matching(
        &self,
        root: SessionId,
        operation: &PreparedOperationIdentity,
        runtime: Option<&cookie_agent_protocol::RuntimeId>,
    ) -> Option<TreeApprovalGrant> {
        let fingerprint = OperationFingerprint::from_prepared_operation(operation);
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&ApprovalKey {
                root,
                fingerprint,
                runtime: runtime.cloned(),
            })
            .filter(|grant| {
                grant.capabilities == operation.capabilities()
                    && grant.resources == operation.resources()
            })
            .cloned()
    }

    #[must_use]
    pub fn for_root(&self, root: SessionId) -> Vec<TreeApprovalGrant> {
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(key, _)| key.root == root)
            .map(|(_, grant)| grant.clone())
            .collect()
    }

    pub fn invalidate_grants(&self, ids: &HashSet<TreeApprovalGrantId>) {
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, grant| !ids.contains(&grant.grant_id));
    }
}

#[derive(Clone, Debug)]
pub struct PermissionDecision {
    pub effect: PermissionEffect,
    pub evaluations: Vec<ApprovalEvaluation>,
}

impl PermissionDecision {
    /// The decision for a tool that needs no permission: allowed, with no
    /// rule evaluated.
    #[must_use]
    pub fn permission_free() -> Self {
        Self {
            effect: PermissionEffect::Allow,
            evaluations: Vec::new(),
        }
    }
}

#[derive(Debug, Default)]
pub struct PermissionPipeline {
    _private: (),
}

impl PermissionPipeline {
    /// The action a tool's permission name checks: the name itself, which
    /// must be an action label.
    pub fn action_for_permission_name(
        permission_name: &str,
    ) -> Result<PermissionAction, PermissionError> {
        PermissionAction::new(permission_name)
            .map_err(|_| PermissionError::UnknownAction(permission_name.into()))
    }

    #[must_use]
    pub fn decide_operation<'a>(
        &self,
        policy: &AgentSnapshot,
        operation: &PreparedOperationIdentity,
        policy_labels: &[Option<String>],
        scope: impl Into<PermissionScope<'a>>,
    ) -> PermissionDecision {
        self.decide_operation_with_overlay(policy, None, operation, policy_labels, scope)
    }

    #[must_use]
    pub fn decide_operation_with_overlay<'a>(
        &self,
        policy: &AgentSnapshot,
        overlay: Option<&SessionPermissionOverlay>,
        operation: &PreparedOperationIdentity,
        policy_labels: &[Option<String>],
        scope: impl Into<PermissionScope<'a>>,
    ) -> PermissionDecision {
        let scope = scope.into();
        assert!(!operation.resources().is_empty());
        assert_eq!(operation.resources().len(), policy_labels.len());
        let evaluations = operation
            .resources()
            .iter()
            .zip(policy_labels)
            .map(|(resource, normalized)| {
                let (candidates, effect, reason) = match normalized {
                    Some(normalized) => {
                        let candidates = matching_rules(
                            policy,
                            overlay,
                            &resource.capability,
                            normalized,
                            scope,
                        );
                        let (effect, reason) = effective_permission_with_overlay(
                            policy,
                            overlay,
                            &resource.capability,
                            normalized,
                            scope,
                        );
                        (candidates, effect, reason)
                    }
                    None => {
                        let candidates =
                            matching_loose_rules(policy, overlay, &resource.capability, scope);
                        let (effect, reason) = effective_loose_permission_with_overlay(
                            policy,
                            overlay,
                            &resource.capability,
                            scope,
                        );
                        (candidates, effect, reason)
                    }
                };
                ApprovalEvaluation {
                    resource_digest: resource.binding_digest.clone(),
                    effect,
                    trace: DecisionTrace {
                        action: resource.capability.clone(),
                        normalized_resource: normalized
                            .clone()
                            .unwrap_or_else(|| UNSCOPED_PERMISSION_RESOURCE_DISPLAY.to_owned()),
                        candidates,
                        effect,
                        precedence_reason: reason,
                    },
                }
            })
            .collect::<Vec<_>>();
        let effect = aggregate_effect(&evaluations);
        PermissionDecision {
            effect,
            evaluations,
        }
    }

    #[must_use]
    pub fn decide_operation_with_grants<'a>(
        &self,
        policy: &AgentSnapshot,
        overlay: Option<&SessionPermissionOverlay>,
        grants: Option<&SessionPermissionOverlay>,
        operation: &PreparedOperationIdentity,
        policy_labels: &[Option<String>],
        scope: impl Into<PermissionScope<'a>>,
    ) -> PermissionDecision {
        let scope = scope.into();
        let mut decision =
            self.decide_operation_with_overlay(policy, overlay, operation, policy_labels, scope);
        let Some(grants) = grants else {
            return decision;
        };
        for (evaluation, (resource, normalized)) in decision
            .evaluations
            .iter_mut()
            .zip(operation.resources().iter().zip(policy_labels))
        {
            if evaluation.effect != PermissionEffect::Ask {
                continue;
            }
            let granted = grants.rules.iter().any(|rule| {
                rule.effect == PermissionEffect::Allow
                    && rule.action == resource.capability
                    && rule.applies_to(scope.runtime)
                    && normalized.as_ref().is_some_and(|normalized| {
                        permission_pattern_matches(
                            &expand_home_pattern(rule.resource.as_str(), scope.home),
                            &resource.capability,
                            normalized,
                            scope.workspace,
                        )
                    })
            });
            if granted {
                evaluation.effect = PermissionEffect::Allow;
                evaluation.trace.effect = PermissionEffect::Allow;
                evaluation.trace.precedence_reason =
                    "turn-scoped skill grant allows an otherwise ask operation".into();
            }
        }
        decision.effect = aggregate_effect(&decision.evaluations);
        decision
    }

    /// Whether a base-layer tool with `permission_name` is visible.
    #[must_use]
    pub fn tool_visible(policy: &AgentSnapshot, permission_name: &str) -> bool {
        Self::tool_visible_with_overlay(policy, None, permission_name, None)
    }

    /// Whether a tool with `permission_name` of the runtime with ID `runtime`
    /// (`None` for the base layer) is visible. A tool that needs no permission
    /// has no name to ask about and is always visible.
    #[must_use]
    pub fn tool_visible_with_overlay(
        policy: &AgentSnapshot,
        overlay: Option<&SessionPermissionOverlay>,
        permission_name: &str,
        runtime: Option<&str>,
    ) -> bool {
        let Ok(action) = Self::action_for_permission_name(permission_name) else {
            return false;
        };
        tool_visible(&policy.permissions, overlay, &action, runtime)
    }

    #[must_use]
    pub fn tool_visible_with_grants<'a>(
        policy: &AgentSnapshot,
        overlay: Option<&SessionPermissionOverlay>,
        grants: Option<&SessionPermissionOverlay>,
        permission_name: &str,
        scope: impl Into<PermissionScope<'a>>,
    ) -> bool {
        let scope = scope.into();
        if Self::tool_visible_with_overlay(policy, overlay, permission_name, scope.runtime) {
            return true;
        }
        let Ok(action) = Self::action_for_permission_name(permission_name) else {
            return false;
        };
        grants.is_some_and(|grants| {
            grants.rules.iter().any(|grant| {
                grant.action == action
                    && grant.effect == PermissionEffect::Allow
                    && grant.applies_to(scope.runtime)
                    && effective_permission_with_overlay(
                        policy,
                        overlay,
                        &action,
                        &expand_home_pattern(grant.resource.as_str(), scope.home),
                        scope,
                    )
                    .0 != PermissionEffect::Deny
            })
        })
    }
}

fn aggregate_effect(evaluations: &[ApprovalEvaluation]) -> PermissionEffect {
    if evaluations
        .iter()
        .any(|evaluation| evaluation.effect == PermissionEffect::Deny)
    {
        PermissionEffect::Deny
    } else if evaluations
        .iter()
        .any(|evaluation| evaluation.effect == PermissionEffect::Ask)
    {
        PermissionEffect::Ask
    } else {
        PermissionEffect::Allow
    }
}

/// The precedence of a rule among the rules that match: a qualified rule
/// ranks above an unqualified one, then later declarations win.
fn loose_rank(rule: &PermissionRule, declaration_index: usize) -> (bool, usize) {
    (rule.runtime.is_some(), declaration_index)
}

pub(crate) fn effective_loose_permission<'a>(
    policy: &AgentSnapshot,
    action: &PermissionAction,
    scope: impl Into<PermissionScope<'a>>,
) -> (PermissionEffect, String) {
    let scope = scope.into();
    policy
        .permissions
        .iter()
        .enumerate()
        .filter(|(_, rule)| {
            rule.action == *action
                && rule.applies_to(scope.runtime)
                && rule.resource.as_str() == "*"
        })
        .max_by_key(|(index, rule)| loose_rank(rule, *index))
        .map_or_else(
            || {
                (
                    PermissionEffect::Deny,
                    "no bare or `*` rule; deny by default for a permission-name-only check".into(),
                )
            },
            |(_, rule)| {
                (
                    rule.effect,
                    "bare or `*` permission rule applies to the permission-name-only check".into(),
                )
            },
        )
}

pub(crate) fn effective_loose_permission_with_overlay<'a>(
    policy: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
    action: &PermissionAction,
    scope: impl Into<PermissionScope<'a>>,
) -> (PermissionEffect, String) {
    let scope = scope.into();
    overlay
        .and_then(|overlay| {
            overlay
                .rules
                .iter()
                .enumerate()
                .filter(|(_, rule)| {
                    rule.action == *action
                        && rule.applies_to(scope.runtime)
                        && rule.resource.as_str() == "*"
                })
                .max_by_key(|(index, rule)| loose_rank(rule, *index))
        })
        .map_or_else(
            || effective_loose_permission(policy, action, scope),
            |(_, rule)| {
                (
                    rule.effect,
                    "session overlay wildcard rule takes precedence over the agent document".into(),
                )
            },
        )
}

pub(crate) fn effective_permission<'a>(
    policy: &AgentSnapshot,
    action: &PermissionAction,
    resource: &str,
    scope: impl Into<PermissionScope<'a>>,
) -> (PermissionEffect, String) {
    let scope = scope.into();
    let winner = policy
        .permissions
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule_matches(rule, action, resource, scope, &policy.agent))
        .max_by_key(|(index, rule)| specificity(rule, *index, &policy.agent, scope.home));
    winner
        .map_or_else(
            || {
                (
                    PermissionEffect::Deny,
                    "no matching rule; deny by default".into(),
                )
            },
            |(_, rule)| {
                (
                    rule.effect,
                    "most-specific matching pattern: more literal characters, then fewer wildcards, then a runtime selector, then later declaration".into(),
                )
            },
        )
}

pub(crate) fn effective_permission_with_overlay<'a>(
    policy: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
    action: &PermissionAction,
    resource: &str,
    scope: impl Into<PermissionScope<'a>>,
) -> (PermissionEffect, String) {
    let scope = scope.into();
    let winner = overlay.and_then(|overlay| {
        overlay
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule_matches(rule, action, resource, scope, &policy.agent))
            .max_by_key(|(index, rule)| specificity(rule, *index, &policy.agent, scope.home))
    });
    winner.map_or_else(
        || effective_permission(policy, action, resource, scope),
        |(_, rule)| {
            (
                rule.effect,
                "session overlay matching rule takes precedence over the agent document".into(),
            )
        },
    )
}

/// Whether `rule` governs `resource` of `action` in `scope` for a run of
/// `agent`.
fn rule_matches(
    rule: &PermissionRule,
    action: &PermissionAction,
    resource: &str,
    scope: PermissionScope<'_>,
    agent: &AgentId,
) -> bool {
    rule.action == *action
        && rule.applies_to(scope.runtime)
        && permission_pattern_matches(
            &rule_pattern(rule, agent, scope.home),
            action,
            resource,
            scope.workspace,
        )
}

/// In a rule pattern, `agent://self` stands for the run's own agent
/// (`docs/site/specs/memory.md` section 9.1).
pub(crate) const SELF_AGENT_PATTERN: &str = "agent://self";

/// The pattern `rule` matches with for a run of `agent` on a runtime with
/// `home`: its resource, with `agent://self` read as `agent://<agent>` and a
/// leading `~` expanded against `home` ([`expand_home_pattern`]).
fn rule_pattern<'rule>(
    rule: &'rule PermissionRule,
    agent: &AgentId,
    home: Option<&str>,
) -> Cow<'rule, str> {
    if rule.resource.as_str() == SELF_AGENT_PATTERN {
        Cow::Owned(format!("agent://{agent}"))
    } else {
        expand_home_pattern(rule.resource.as_str(), home)
    }
}

/// `pattern` with a leading `~` (the whole pattern, or `~/` before the
/// rest) replaced by `home`, the home directory the tool's runtime
/// reported, spelled with `/` as path labels are. Without a home, and for
/// any other pattern (`~user/…` included), `pattern` as it is: a literal `~`
/// then matches no absolute path. Pure and allocation-free when nothing
/// expands.
pub(crate) fn expand_home_pattern<'p>(pattern: &'p str, home: Option<&str>) -> Cow<'p, str> {
    let Some(home) = home else {
        return Cow::Borrowed(pattern);
    };
    let rest = if pattern == "~" {
        ""
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        rest
    } else {
        return Cow::Borrowed(pattern);
    };
    let home = home.replace('\\', "/");
    let home = home.trim_end_matches('/');
    Cow::Owned(match (home.is_empty(), rest.is_empty()) {
        // The home is the root.
        (true, true) => "/".to_owned(),
        (true, false) => format!("/{rest}"),
        (false, true) => home.to_owned(),
        (false, false) => format!("{home}/{rest}"),
    })
}

/// The rules an agent document gives its runs (`docs/site/specs/memory.md`
/// section 9.2 and `docs/site/specs/runtime-delegation.md` section 4): the
/// document's own, then, for each memory action and `list_runtimes` the
/// document names no key for (with or without a runtime selector), the
/// default of the agent's mode. `primary` and `all` agents may read and
/// write all memory and list runtimes, `subagent` agents read all memory and
/// write their own agent file, and internal agents get nothing.
pub(crate) fn agent_permission_rules(
    document: &cookie_agent_config::AgentDocument,
) -> Vec<PermissionRule> {
    let permissions = &document.frontmatter.permissions;
    let mut rules = permissions
        .iter()
        .flat_map(|(key, value)| value.rules(key))
        .collect::<Vec<_>>();
    let defaults: &[(PermissionAction, &str)] = match document.frontmatter.mode {
        AgentMode::Primary | AgentMode::All => &[
            (PermissionAction::MEMORY, "*"),
            (PermissionAction::MEMORY_WRITE, "*"),
            (PermissionAction::LIST_RUNTIMES, "*"),
        ],
        AgentMode::Subagent => &[
            (PermissionAction::MEMORY, "*"),
            (PermissionAction::MEMORY_WRITE, SELF_AGENT_PATTERN),
        ],
        AgentMode::Internal => &[],
    };
    for (action, resource) in defaults {
        if !permissions.keys().any(|key| key.action == *action) {
            rules.push(PermissionRule::new(
                action.clone(),
                WildcardPattern::new(*resource).expect("static default pattern"),
                PermissionEffect::Allow,
            ));
        }
    }
    rules
}

/// Whether `pattern` matches `resource` for a tool whose runtime has
/// `workspace`. `${workspace_dir}` patterns match the resource made absolute
/// against the workspace. Without a workspace (the base layer and the global
/// runtime) nothing can be resolved: `${workspace_dir}` patterns never match,
/// and neither does any pattern for a relative path resource of a path action
/// (`read`, `write`).
fn permission_pattern_matches(
    pattern: &str,
    action: &PermissionAction,
    resource: &str,
    workspace: Option<&Path>,
) -> bool {
    let workspace_pattern =
        pattern.contains(cookie_agent_protocol::WildcardPattern::WORKSPACE_DIR_EXPRESSION);
    match workspace {
        Some(workspace) if workspace_pattern => simple_wildcard_match(
            &expand_workspace_pattern(pattern, workspace),
            &absolute_resource(workspace, resource),
        ),
        Some(_) => simple_wildcard_match(pattern, resource),
        None if workspace_pattern => false,
        None if action.is_path() && !Path::new(resource).is_absolute() => false,
        None => simple_wildcard_match(pattern, resource),
    }
}

fn expand_workspace_pattern(pattern: &str, workspace: &Path) -> String {
    let workspace = normalized_path(&canonical_workspace(workspace));
    if workspace == "/" {
        pattern.replace("${workspace_dir}/", "/").replace(
            cookie_agent_protocol::WildcardPattern::WORKSPACE_DIR_EXPRESSION,
            "/",
        )
    } else {
        pattern.replace(
            cookie_agent_protocol::WildcardPattern::WORKSPACE_DIR_EXPRESSION,
            workspace.trim_end_matches('/'),
        )
    }
}

fn absolute_resource(workspace: &Path, resource: &str) -> String {
    let resource_path = Path::new(resource);
    if resource_path.is_absolute() {
        normalized_path(resource_path)
    } else if resource_path == Path::new(".") {
        normalized_path(&canonical_workspace(workspace))
    } else {
        normalized_path(&canonical_workspace(workspace).join(resource_path))
    }
}

fn canonical_workspace(workspace: &Path) -> PathBuf {
    cookie_agent_protocol::paths::canonicalize(workspace).unwrap_or_else(|_| workspace.to_owned())
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Rule precedence among matching rules: more literal characters, then fewer
/// wildcards, then a runtime selector over none, then later declaration.
fn specificity(
    rule: &PermissionRule,
    declaration_index: usize,
    agent: &AgentId,
    home: Option<&str>,
) -> (usize, std::cmp::Reverse<usize>, bool, usize) {
    let pattern = rule_pattern(rule, agent, home);
    let wildcards = pattern
        .chars()
        .filter(|character| matches!(character, '*' | '?'))
        .count();
    let literals = pattern.chars().count() - wildcards;
    (
        literals,
        std::cmp::Reverse(wildcards),
        rule.runtime.is_some(),
        declaration_index,
    )
}

fn matching_rules(
    policy: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
    action: &PermissionAction,
    resource: &str,
    scope: PermissionScope<'_>,
) -> Vec<MatchedPermissionRule> {
    matched(policy, overlay, |rule| {
        rule_matches(rule, action, resource, scope, &policy.agent)
    })
}

fn matching_loose_rules(
    policy: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
    action: &PermissionAction,
    scope: PermissionScope<'_>,
) -> Vec<MatchedPermissionRule> {
    matched(policy, overlay, |rule| {
        rule.action == *action && rule.applies_to(scope.runtime) && rule.resource.as_str() == "*"
    })
}

/// The agent-document rules, then the overlay rules, that `keep` admits.
fn matched(
    policy: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
    keep: impl Fn(&PermissionRule) -> bool,
) -> Vec<MatchedPermissionRule> {
    let traced = |layer: &'static str| {
        move |rule: &PermissionRule| MatchedPermissionRule {
            source_layer: SafeCode::new(layer).expect("static safe code"),
            action: rule.action.clone(),
            runtime: rule.runtime.clone(),
            resource: rule.resource.clone(),
            effect: rule.effect,
        }
    };
    policy
        .permissions
        .iter()
        .filter(|rule| keep(rule))
        .map(traced("agent_document"))
        .chain(
            overlay
                .into_iter()
                .flat_map(|overlay| overlay.rules.iter())
                .filter(|rule| keep(rule))
                .map(traced("session_overlay")),
        )
        .collect()
}

impl crate::Engine {
    /// The effective permissions a session created with `selection` and the
    /// overlay `rules` would have, computed exactly as for a live session.
    pub fn preview_session_permissions(
        &self,
        selection: &RunSelection,
        rules: Vec<PermissionRule>,
    ) -> Result<Vec<EffectivePermissionAction>, crate::EngineError> {
        let overlay = SessionPermissionOverlay { rules };
        overlay
            .validate()
            .map_err(|error| crate::EngineError::Permission(error.to_string()))?;
        let (_, policy) = self.freeze_selection_policy(selection)?;
        Ok(effective_permission_view(&policy.agent, &overlay, &[]))
    }

    pub fn get_session_permissions(
        &self,
        session_id: SessionId,
    ) -> Result<SessionPermissionGetResult, crate::EngineError> {
        let session = self.inner.store.get(session_id)?;
        let policy = governing_agent(&session);
        let declared = self.declared_permission_actions(session_id)?;
        Ok(SessionPermissionGetResult {
            permissions: effective_permission_view(&policy, &session.permission_overlay, &declared),
            current_mode: Some(self.permission_mode(session_id)),
        })
    }

    /// The actions the tools of every source of `session` are checked under,
    /// visible or not, in source order.
    fn declared_permission_actions(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PermissionAction>, crate::EngineError> {
        let context = cookie_agent_runtime_api::SessionToolContext::new(session_id);
        let mut actions = Vec::new();
        for source in self.session_tool_sources(session_id)? {
            for tool in source
                .provider
                .tools_for_session(&context)
                .map_err(|error| crate::EngineError::ToolFailed(error.to_string()))?
            {
                if let Some(Ok(action)) = tool
                    .permission_name
                    .as_deref()
                    .map(PermissionPipeline::action_for_permission_name)
                    && !actions.contains(&action)
                {
                    actions.push(action);
                }
            }
        }
        Ok(actions)
    }

    /// Sets the overlay rule for `action` (qualified by `runtime` when given)
    /// on `resource`. A change that makes the rule stricter for any runtime
    /// the session uses invalidates that action's tree grants there.
    pub async fn set_session_permission(
        &self,
        session_id: SessionId,
        action: &PermissionAction,
        runtime: Option<RuntimeId>,
        resource: WildcardPattern,
        effect: PermissionEffect,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<SessionPermissionMutationResult, crate::EngineError> {
        let _mutation = self
            .inner
            .approvals
            .permission_overlay_mutation
            .lock()
            .await;
        let session = self.inner.store.get(session_id)?;
        let policy = governing_agent(&session);
        let scopes = self.overlay_effect_scopes(&session, runtime.as_ref());
        let mut overlay = session.permission_overlay.clone();
        let old_effects = rule_effects(&policy, &overlay, action, resource.as_str(), &scopes);
        overlay.rules.retain(|rule| {
            rule.action != *action || rule.runtime != runtime || rule.resource != resource
        });
        overlay.rules.push(PermissionRule {
            action: action.clone(),
            runtime: runtime.clone(),
            resource: resource.clone(),
            effect,
        });
        overlay
            .validate()
            .map_err(|error| crate::EngineError::Permission(error.to_string()))?;
        let new_effects = rule_effects(&policy, &overlay, action, resource.as_str(), &scopes);
        if stricter(&old_effects, &new_effects) {
            self.invalidate_action_grants(session_id, &session.meta.origin, action, runtime)?;
        }
        self.append(
            session_id,
            None,
            origin,
            EventPayload::SessionPermissionOverlaySet { overlay },
        )
        .await?;
        let result = self.get_session_permissions(session_id)?;
        Ok(SessionPermissionMutationResult {
            permissions: result.permissions,
        })
    }

    /// Removes the overlay rule for `action` (qualified by `runtime` when
    /// given) on `resource`.
    pub async fn clear_session_permission(
        &self,
        session_id: SessionId,
        action: &PermissionAction,
        runtime: Option<RuntimeId>,
        resource: &WildcardPattern,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<SessionPermissionMutationResult, crate::EngineError> {
        let _mutation = self
            .inner
            .approvals
            .permission_overlay_mutation
            .lock()
            .await;
        let session = self.inner.store.get(session_id)?;
        let policy = governing_agent(&session);
        let scopes = self.overlay_effect_scopes(&session, runtime.as_ref());
        let mut overlay = session.permission_overlay.clone();
        let old_effects = rule_effects(&policy, &overlay, action, resource.as_str(), &scopes);
        let prior_len = overlay.rules.len();
        overlay.rules.retain(|rule| {
            rule.action != *action || rule.runtime != runtime || &rule.resource != resource
        });
        if overlay.rules.len() != prior_len {
            let new_effects = rule_effects(&policy, &overlay, action, resource.as_str(), &scopes);
            if stricter(&old_effects, &new_effects) {
                self.invalidate_action_grants(session_id, &session.meta.origin, action, runtime)?;
            }
            self.append(
                session_id,
                None,
                origin,
                EventPayload::SessionPermissionOverlaySet { overlay },
            )
            .await?;
        }
        let result = self.get_session_permissions(session_id)?;
        Ok(SessionPermissionMutationResult {
            permissions: result.permissions,
        })
    }

    /// The scopes an overlay rule can affect in `session`: for a rule
    /// qualified by `runtime`, each attachment of that runtime ID (or the
    /// runtime without a workspace when none is attached); for an
    /// unqualified rule, the base layer and every attachment.
    fn overlay_effect_scopes(
        &self,
        session: &crate::session::SessionProjection,
        runtime: Option<&RuntimeId>,
    ) -> Vec<OwnedScope> {
        let attachments = self.attachments_of(session);
        let mut scopes = attachments
            .iter()
            .filter(|attachment| runtime.is_none_or(|runtime| &attachment.runtime_id == runtime))
            .map(|attachment| OwnedScope {
                runtime: Some(attachment.runtime_id.as_str().to_owned()),
                workspace: attachment
                    .workspace
                    .as_deref()
                    .map(std::path::PathBuf::from),
                home: self.attachment_home(attachment),
            })
            .collect::<Vec<_>>();
        match runtime {
            None => scopes.push(OwnedScope::default()),
            Some(runtime) if scopes.is_empty() => {
                scopes.push(OwnedScope {
                    runtime: Some(runtime.as_str().to_owned()),
                    workspace: None,
                    home: self.runtime_home(runtime.as_str()),
                });
            }
            Some(_) => {}
        }
        scopes
    }

    /// The home directory the runtime of `attachment` reported: its running
    /// instance's, else the one its runtime last reported.
    fn attachment_home(
        &self,
        attachment: &cookie_agent_protocol::RuntimeAttachment,
    ) -> Option<String> {
        self.inner
            .runtimes
            .instance(attachment)
            .and_then(|instance| instance.info().home().map(str::to_owned))
            .or_else(|| self.runtime_home(attachment.runtime_id.as_str()))
    }

    /// The home directory the runtime with ID `runtime_id` last reported.
    pub(crate) fn runtime_home(&self, runtime_id: &str) -> Option<String> {
        self.inner
            .runtimes
            .templates()
            .into_iter()
            .find(|template| template.runtime_id == runtime_id)
            .and_then(|template| template.listing.home)
    }

    fn invalidate_action_grants(
        &self,
        session_id: SessionId,
        origin: &SessionOrigin,
        action: &PermissionAction,
        runtime: Option<RuntimeId>,
    ) -> Result<(), crate::EngineError> {
        let root = match origin {
            SessionOrigin::Root => session_id,
            SessionOrigin::Delegated {
                root_session_id, ..
            } => *root_session_id,
        };
        let grants = self
            .inner
            .approvals
            .store
            .for_root(root)
            .into_iter()
            .filter(|grant| {
                runtime
                    .as_ref()
                    .is_none_or(|runtime| grant.runtime_id.as_ref() == Some(runtime))
                    && grant
                        .resources
                        .iter()
                        .any(|resource| resource.capability == *action)
            })
            .collect::<Vec<_>>();
        let ids = grants
            .iter()
            .map(|grant| grant.grant_id)
            .collect::<HashSet<_>>();
        if ids.is_empty() {
            return Ok(());
        }
        let digests = grants
            .iter()
            .flat_map(|grant| grant.resources.iter())
            .filter(|resource| resource.capability == *action)
            .map(|resource| resource.binding_digest.digest().clone())
            .collect();
        self.inner.grant_journals.for_root(root)?.invalidate(
            root,
            ids.iter().copied().collect(),
            digests,
        )?;
        self.inner.approvals.store.invalidate_grants(&ids);
        Ok(())
    }
}

pub(crate) fn governing_agent_for_skills(
    session: &crate::session::SessionProjection,
) -> Arc<AgentSnapshot> {
    governing_agent(session)
}

/// The agent of the latest visible run, else the session's creation agent.
fn governing_agent(session: &crate::session::SessionProjection) -> Arc<AgentSnapshot> {
    // The log index names the latest visible run; the projection already
    // holds its snapshot unless the run started after this projection was
    // taken, in which case the log itself is read.
    let latest_run = session.log.last_run_started().map(|(_, run, _)| {
        session.runs.get(&run).map_or_else(
            || {
                session
                    .log
                    .event_snapshot()
                    .iter()
                    .rev()
                    .find_map(|event| match &event.payload {
                        EventPayload::RunStarted { agent, .. } => {
                            Some(Arc::new(agent.as_ref().clone()))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| session.creation_agent.clone())
            },
            |projected| projected.agent.clone(),
        )
    });
    select_governing_agent(&session.creation_agent, latest_run.as_ref()).clone()
}

fn select_governing_agent<'a, A>(creation_agent: &'a A, latest_run_agent: Option<&'a A>) -> &'a A {
    latest_run_agent.unwrap_or(creation_agent)
}

fn effect_rank(effect: PermissionEffect) -> u8 {
    match effect {
        PermissionEffect::Allow => 0,
        PermissionEffect::Ask => 1,
        PermissionEffect::Deny => 2,
    }
}

/// Whether any scope's effect got stricter between `old` and `new`.
fn stricter(old: &[PermissionEffect], new: &[PermissionEffect]) -> bool {
    old.iter()
        .zip(new)
        .any(|(old, new)| effect_rank(*new) > effect_rank(*old))
}

/// A [`PermissionScope`] that owns its parts.
#[derive(Clone, Debug, Default)]
struct OwnedScope {
    runtime: Option<String>,
    workspace: Option<std::path::PathBuf>,
    home: Option<String>,
}

/// The effect of `action` on `resource` in each of `scopes`. A `~` in
/// `resource`, a rule's pattern, expands against each scope's home as the
/// rule's own pattern does.
fn rule_effects(
    policy: &AgentSnapshot,
    overlay: &SessionPermissionOverlay,
    action: &PermissionAction,
    resource: &str,
    scopes: &[OwnedScope],
) -> Vec<PermissionEffect> {
    scopes
        .iter()
        .map(|owned| {
            let scope = PermissionScope {
                runtime: owned.runtime.as_deref(),
                workspace: owned.workspace.as_deref(),
                home: owned.home.as_deref(),
            };
            if resource == "*" {
                effective_loose_permission_with_overlay(policy, Some(overlay), action, scope).0
            } else {
                let resource = expand_home_pattern(resource, scope.home);
                effective_permission_with_overlay(policy, Some(overlay), action, &resource, scope).0
            }
        })
        .collect()
}

/// The effective permission of every action: the built-in actions, then the
/// other actions the session's tools `declared`, then any other action a rule
/// names, each group in first-seen order.
fn effective_permission_view(
    policy: &AgentSnapshot,
    overlay: &SessionPermissionOverlay,
    declared: &[PermissionAction],
) -> Vec<EffectivePermissionAction> {
    let mut actions = PermissionAction::BUILT_IN.to_vec();
    let named = policy
        .permissions
        .iter()
        .chain(&overlay.rules)
        .map(|rule| &rule.action);
    for action in declared.iter().chain(named) {
        if !actions.contains(action) {
            actions.push(action.clone());
        }
    }
    actions
        .into_iter()
        .map(|action| {
            let unqualified_wildcard = |rule: &&PermissionRule| {
                rule.action == action && rule.runtime.is_none() && rule.resource.as_str() == "*"
            };
            let overlay_wildcard = overlay.rules.iter().rev().find(unqualified_wildcard);
            let agent_wildcard = policy.permissions.iter().rev().find(unqualified_wildcard);
            let (effect, source) = overlay_wildcard.map_or_else(
                || {
                    agent_wildcard.map_or(
                        (PermissionEffect::Deny, PermissionRuleSource::Default),
                        |rule| (rule.effect, PermissionRuleSource::AgentDocument),
                    )
                },
                |rule| (rule.effect, PermissionRuleSource::SessionOverlay),
            );
            let listed = |rule: &&PermissionRule| {
                rule.action == action && (rule.runtime.is_some() || rule.resource.as_str() != "*")
            };
            let entry = |rule: &PermissionRule, source| {
                (
                    (rule.runtime.clone(), rule.resource.as_str().to_owned()),
                    EffectivePermissionRule {
                        runtime: rule.runtime.clone(),
                        resource: rule.resource.clone(),
                        effect: rule.effect,
                        source,
                    },
                )
            };
            let mut patterns = BTreeMap::new();
            if overlay_wildcard.is_none() {
                patterns.extend(
                    policy
                        .permissions
                        .iter()
                        .filter(listed)
                        .map(|rule| entry(rule, PermissionRuleSource::AgentDocument)),
                );
            }
            patterns.extend(
                overlay
                    .rules
                    .iter()
                    .filter(listed)
                    .map(|rule| entry(rule, PermissionRuleSource::SessionOverlay)),
            );
            EffectivePermissionAction {
                action,
                effect,
                source,
                patterns: patterns.into_values().collect(),
            }
        })
        .collect()
}

pub(crate) mod identity;

#[cfg(test)]
mod tests;
