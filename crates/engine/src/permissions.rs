//! Permission evaluation over immutable prepared-operation manifests.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_config::simple_wildcard_match;
use cookie_agent_protocol::{
    AgentSnapshot, ApprovalEvaluation, DecisionTrace, EffectivePermissionAction,
    EffectivePermissionRule, EventPayload, MatchedPermissionRule, OperationFingerprint,
    PermissionAction, PermissionEffect, PermissionRule, PermissionRuleSource,
    PreparedOperationIdentity, SafeCode, SessionId, SessionOrigin, SessionPermissionGetResult,
    SessionPermissionMutationResult, SessionPermissionOverlay, TreeApprovalGrant,
    TreeApprovalGrantId, WildcardPattern,
};
use thiserror::Error;

use crate::tool_api::UNSCOPED_PERMISSION_RESOURCE_DISPLAY;

/// Visibility is action-level opt-in, not a resource authorization decision.
#[must_use]
pub fn tool_visible(
    rules: &[PermissionRule],
    overlay: Option<&SessionPermissionOverlay>,
    action: PermissionAction,
) -> bool {
    rules
        .iter()
        .chain(overlay.into_iter().flat_map(|overlay| &overlay.rules))
        .any(|rule| {
            rule.action == action
                && matches!(rule.effect, PermissionEffect::Allow | PermissionEffect::Ask)
        })
}

#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("unknown permission name `{0}`")]
    UnknownAction(String),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ApprovalKey {
    root: SessionId,
    fingerprint: OperationFingerprint,
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
                },
                grant,
            );
    }

    #[must_use]
    pub fn matching(
        &self,
        root: SessionId,
        operation: &PreparedOperationIdentity,
    ) -> Option<TreeApprovalGrant> {
        let fingerprint = OperationFingerprint::from_prepared_operation(operation);
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&ApprovalKey { root, fingerprint })
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

#[derive(Debug, Default)]
pub struct PermissionPipeline {
    _private: (),
}

impl PermissionPipeline {
    pub fn action_for_permission_name(
        permission_name: &str,
    ) -> Result<PermissionAction, PermissionError> {
        match permission_name {
            "read" => Ok(PermissionAction::Read),
            "write" => Ok(PermissionAction::Write),
            "bash" => Ok(PermissionAction::Bash),
            "delegate" => Ok(PermissionAction::Delegate),
            "message" => Ok(PermissionAction::Message),
            "mcp" => Ok(PermissionAction::Mcp),
            "plugin" => Ok(PermissionAction::Plugin),
            "skill" => Ok(PermissionAction::Skill),
            "webfetch" => Ok(PermissionAction::Webfetch),
            other if other.starts_with("plugin:") && other.len() > "plugin:".len() => {
                Ok(PermissionAction::Plugin)
            }
            other => Err(PermissionError::UnknownAction(other.into())),
        }
    }

    #[must_use]
    pub fn decide_operation(
        &self,
        policy: &AgentSnapshot,
        operation: &PreparedOperationIdentity,
        policy_labels: &[Option<String>],
        workspace: &Path,
    ) -> PermissionDecision {
        self.decide_operation_with_overlay(policy, None, operation, policy_labels, workspace)
    }

    #[must_use]
    pub fn decide_operation_with_overlay(
        &self,
        policy: &AgentSnapshot,
        overlay: Option<&SessionPermissionOverlay>,
        operation: &PreparedOperationIdentity,
        policy_labels: &[Option<String>],
        workspace: &Path,
    ) -> PermissionDecision {
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
                            resource.capability,
                            normalized,
                            workspace,
                        );
                        let (effect, reason) = effective_permission_with_overlay(
                            policy,
                            overlay,
                            resource.capability,
                            normalized,
                            workspace,
                        );
                        (candidates, effect, reason)
                    }
                    None => {
                        let candidates = matching_loose_rules(policy, overlay, resource.capability);
                        let (effect, reason) = effective_loose_permission_with_overlay(
                            policy,
                            overlay,
                            resource.capability,
                        );
                        (candidates, effect, reason)
                    }
                };
                ApprovalEvaluation {
                    resource_digest: resource.binding_digest.clone(),
                    effect,
                    trace: DecisionTrace {
                        action: resource.capability,
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
    pub fn decide_operation_with_grants(
        &self,
        policy: &AgentSnapshot,
        overlay: Option<&SessionPermissionOverlay>,
        grants: Option<&SessionPermissionOverlay>,
        operation: &PreparedOperationIdentity,
        policy_labels: &[Option<String>],
        workspace: &Path,
    ) -> PermissionDecision {
        let mut decision = self.decide_operation_with_overlay(
            policy,
            overlay,
            operation,
            policy_labels,
            workspace,
        );
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
                    && normalized.as_ref().is_some_and(|normalized| {
                        permission_pattern_matches(
                            rule.resource.as_str(),
                            normalized,
                            &absolute_resource(workspace, normalized),
                            workspace,
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

    #[must_use]
    pub fn tool_visible(policy: &AgentSnapshot, permission_name: &str) -> bool {
        Self::tool_visible_with_overlay(policy, None, permission_name)
    }

    #[must_use]
    pub fn tool_visible_with_overlay(
        policy: &AgentSnapshot,
        overlay: Option<&SessionPermissionOverlay>,
        permission_name: &str,
    ) -> bool {
        let Ok(action) = Self::action_for_permission_name(permission_name) else {
            return false;
        };
        tool_visible(&policy.permissions, overlay, action)
    }

    #[must_use]
    pub fn tool_visible_with_grants(
        policy: &AgentSnapshot,
        overlay: Option<&SessionPermissionOverlay>,
        grants: Option<&SessionPermissionOverlay>,
        permission_name: &str,
        workspace: &Path,
    ) -> bool {
        if Self::tool_visible_with_overlay(policy, overlay, permission_name) {
            return true;
        }
        let Ok(action) = Self::action_for_permission_name(permission_name) else {
            return false;
        };
        let plugin_permission = permission_name.strip_prefix("plugin:");
        grants.is_some_and(|grants| {
            grants.rules.iter().any(|grant| {
                grant.action == action
                    && grant.effect == PermissionEffect::Allow
                    && plugin_permission.is_none_or(|permission| {
                        plugin_rule_governs(grant.resource.as_str(), permission)
                    })
                    && effective_permission_with_overlay(
                        policy,
                        overlay,
                        action,
                        grant.resource.as_str(),
                        workspace,
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

pub(crate) fn effective_loose_permission(
    policy: &AgentSnapshot,
    action: PermissionAction,
) -> (PermissionEffect, String) {
    policy
        .permissions
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule.action == action && rule.resource.as_str() == "*")
        .max_by_key(|(index, _)| *index)
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

pub(crate) fn effective_loose_permission_with_overlay(
    policy: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
    action: PermissionAction,
) -> (PermissionEffect, String) {
    overlay
        .and_then(|overlay| {
            overlay
                .rules
                .iter()
                .enumerate()
                .filter(|(_, rule)| rule.action == action && rule.resource.as_str() == "*")
                .max_by_key(|(index, _)| *index)
        })
        .map_or_else(
            || effective_loose_permission(policy, action),
            |(_, rule)| {
                (
                    rule.effect,
                    "session overlay wildcard rule takes precedence over the agent document".into(),
                )
            },
        )
}

pub(crate) fn effective_permission(
    policy: &AgentSnapshot,
    action: PermissionAction,
    resource: &str,
    workspace: &Path,
) -> (PermissionEffect, String) {
    let absolute_resource = absolute_resource(workspace, resource);
    let winner = policy
        .permissions
        .iter()
        .enumerate()
        .filter(|(_, rule)| {
            rule.action == action
                && permission_pattern_matches(
                    rule.resource.as_str(),
                    resource,
                    &absolute_resource,
                    workspace,
                )
        })
        .max_by_key(|(index, rule)| specificity(rule.resource.as_str(), *index));
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
                    "most-specific matching pattern: more literal characters, then fewer wildcards, then later declaration".into(),
                )
            },
        )
}

pub(crate) fn effective_permission_with_overlay(
    policy: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
    action: PermissionAction,
    resource: &str,
    workspace: &Path,
) -> (PermissionEffect, String) {
    let absolute_resource = absolute_resource(workspace, resource);
    let winner = overlay.and_then(|overlay| {
        overlay
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| {
                rule.action == action
                    && permission_pattern_matches(
                        rule.resource.as_str(),
                        resource,
                        &absolute_resource,
                        workspace,
                    )
            })
            .max_by_key(|(index, rule)| specificity(rule.resource.as_str(), *index))
    });
    winner.map_or_else(
        || effective_permission(policy, action, resource, workspace),
        |(_, rule)| {
            (
                rule.effect,
                "session overlay matching rule takes precedence over the agent document".into(),
            )
        },
    )
}

fn permission_pattern_matches(
    pattern: &str,
    relative_resource: &str,
    absolute_resource: &str,
    workspace: &Path,
) -> bool {
    if pattern.contains(cookie_agent_protocol::WildcardPattern::WORKSPACE_DIR_EXPRESSION) {
        let expanded = expand_workspace_pattern(pattern, workspace);
        simple_wildcard_match(&expanded, absolute_resource)
    } else {
        simple_wildcard_match(pattern, relative_resource)
    }
}

fn plugin_rule_governs(pattern: &str, permission_name: &str) -> bool {
    pattern == "*"
        || simple_wildcard_match(pattern, permission_name)
        || pattern
            .strip_prefix(permission_name)
            .is_some_and(|suffix| suffix.starts_with(' '))
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
    if resource.starts_with("artifact://") {
        return resource.to_owned();
    }
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
    workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_owned())
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn specificity(
    pattern: &str,
    declaration_index: usize,
) -> (usize, std::cmp::Reverse<usize>, usize) {
    let wildcards = pattern
        .chars()
        .filter(|character| matches!(character, '*' | '?'))
        .count();
    let literals = pattern.chars().count() - wildcards;
    (literals, std::cmp::Reverse(wildcards), declaration_index)
}

fn matching_rules(
    policy: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
    action: PermissionAction,
    resource: &str,
    workspace: &Path,
) -> Vec<MatchedPermissionRule> {
    let absolute_resource = absolute_resource(workspace, resource);
    policy
        .permissions
        .iter()
        .filter(|rule| {
            rule.action == action
                && permission_pattern_matches(
                    rule.resource.as_str(),
                    resource,
                    &absolute_resource,
                    workspace,
                )
        })
        .map(|rule| MatchedPermissionRule {
            source_layer: SafeCode::new("agent_document").expect("static safe code"),
            action: rule.action,
            resource: rule.resource.clone(),
            effect: rule.effect,
        })
        .chain(
            overlay
                .into_iter()
                .flat_map(|overlay| overlay.rules.iter())
                .filter(|rule| {
                    rule.action == action
                        && permission_pattern_matches(
                            rule.resource.as_str(),
                            resource,
                            &absolute_resource,
                            workspace,
                        )
                })
                .map(|rule| MatchedPermissionRule {
                    source_layer: SafeCode::new("session_overlay").expect("static safe code"),
                    action: rule.action,
                    resource: rule.resource.clone(),
                    effect: rule.effect,
                }),
        )
        .collect()
}

fn matching_loose_rules(
    policy: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
    action: PermissionAction,
) -> Vec<MatchedPermissionRule> {
    policy
        .permissions
        .iter()
        .filter(|rule| rule.action == action && rule.resource.as_str() == "*")
        .map(|rule| MatchedPermissionRule {
            source_layer: SafeCode::new("agent_document").expect("static safe code"),
            action: rule.action,
            resource: rule.resource.clone(),
            effect: rule.effect,
        })
        .chain(
            overlay
                .into_iter()
                .flat_map(|overlay| overlay.rules.iter())
                .filter(|rule| rule.action == action && rule.resource.as_str() == "*")
                .map(|rule| MatchedPermissionRule {
                    source_layer: SafeCode::new("session_overlay").expect("static safe code"),
                    action: rule.action,
                    resource: rule.resource.clone(),
                    effect: rule.effect,
                }),
        )
        .collect()
}

impl crate::Engine {
    pub fn get_session_permissions(
        &self,
        session_id: SessionId,
    ) -> Result<SessionPermissionGetResult, crate::EngineError> {
        let session = self.inner.store.get(session_id)?;
        let policy = governing_agent(&session);
        Ok(SessionPermissionGetResult {
            permissions: effective_permission_view(&policy, &session.permission_overlay),
            current_mode: Some(self.permission_mode(session_id)),
        })
    }

    pub async fn set_session_permission(
        &self,
        session_id: SessionId,
        action: PermissionAction,
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
        let mut overlay = session.permission_overlay.clone();
        let old_effect = rule_effect(
            &policy,
            &overlay,
            action,
            resource.as_str(),
            self.inner.store.cwd(),
        );
        overlay
            .rules
            .retain(|rule| rule.action != action || rule.resource != resource);
        overlay.rules.push(PermissionRule {
            action,
            resource: resource.clone(),
            effect,
        });
        overlay
            .validate()
            .map_err(|error| crate::EngineError::Permission(error.to_string()))?;
        let new_effect = rule_effect(
            &policy,
            &overlay,
            action,
            resource.as_str(),
            self.inner.store.cwd(),
        );
        if effect_rank(new_effect) > effect_rank(old_effect) {
            self.invalidate_action_grants(session_id, &session.meta.origin, action)?;
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

    pub async fn clear_session_permission(
        &self,
        session_id: SessionId,
        action: PermissionAction,
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
        let mut overlay = session.permission_overlay;
        let old_effect = rule_effect(
            &policy,
            &overlay,
            action,
            resource.as_str(),
            self.inner.store.cwd(),
        );
        let prior_len = overlay.rules.len();
        overlay
            .rules
            .retain(|rule| rule.action != action || &rule.resource != resource);
        if overlay.rules.len() != prior_len {
            let new_effect = rule_effect(
                &policy,
                &overlay,
                action,
                resource.as_str(),
                self.inner.store.cwd(),
            );
            if effect_rank(new_effect) > effect_rank(old_effect) {
                self.invalidate_action_grants(session_id, &session.meta.origin, action)?;
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

    fn invalidate_action_grants(
        &self,
        session_id: SessionId,
        origin: &SessionOrigin,
        action: PermissionAction,
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
                grant
                    .resources
                    .iter()
                    .any(|resource| resource.capability == action)
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
            .filter(|resource| resource.capability == action)
            .map(|resource| resource.binding_digest.digest().clone())
            .collect();
        self.inner
            .grant_journal
            .invalidate(root, ids.iter().copied().collect(), digests)?;
        self.inner.approvals.store.invalidate_grants(&ids);
        Ok(())
    }
}

pub(crate) fn governing_agent_for_skills(
    session: &crate::session::SessionProjection,
) -> AgentSnapshot {
    governing_agent(session)
}

fn governing_agent(session: &crate::session::SessionProjection) -> AgentSnapshot {
    let latest_run =
        session
            .log
            .event_snapshot()
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                EventPayload::RunStarted { agent, .. } => Some(agent.as_ref().clone()),
                _ => None,
            });
    select_governing_agent(&session.creation_agent, latest_run.as_ref())
}

fn select_governing_agent(
    creation_agent: &AgentSnapshot,
    latest_run_agent: Option<&AgentSnapshot>,
) -> AgentSnapshot {
    latest_run_agent.unwrap_or(creation_agent).clone()
}

fn effect_rank(effect: PermissionEffect) -> u8 {
    match effect {
        PermissionEffect::Allow => 0,
        PermissionEffect::Ask => 1,
        PermissionEffect::Deny => 2,
    }
}

fn rule_effect(
    policy: &AgentSnapshot,
    overlay: &SessionPermissionOverlay,
    action: PermissionAction,
    resource: &str,
    workspace: &Path,
) -> PermissionEffect {
    if resource == "*" {
        effective_loose_permission_with_overlay(policy, Some(overlay), action).0
    } else {
        effective_permission_with_overlay(policy, Some(overlay), action, resource, workspace).0
    }
}

fn effective_permission_view(
    policy: &AgentSnapshot,
    overlay: &SessionPermissionOverlay,
) -> Vec<EffectivePermissionAction> {
    [
        PermissionAction::Read,
        PermissionAction::Write,
        PermissionAction::Bash,
        PermissionAction::Delegate,
        PermissionAction::Message,
        PermissionAction::Mcp,
        PermissionAction::Plugin,
        PermissionAction::Skill,
        PermissionAction::Webfetch,
    ]
    .into_iter()
    .map(|action| {
        let overlay_wildcard = overlay
            .rules
            .iter()
            .rev()
            .find(|rule| rule.action == action && rule.resource.as_str() == "*");
        let agent_wildcard = policy
            .permissions
            .iter()
            .rev()
            .find(|rule| rule.action == action && rule.resource.as_str() == "*");
        let (effect, source) = overlay_wildcard.map_or_else(
            || {
                agent_wildcard.map_or(
                    (PermissionEffect::Deny, PermissionRuleSource::Default),
                    |rule| (rule.effect, PermissionRuleSource::AgentDocument),
                )
            },
            |rule| (rule.effect, PermissionRuleSource::SessionOverlay),
        );
        let mut patterns = BTreeMap::new();
        if overlay_wildcard.is_none() {
            for rule in policy
                .permissions
                .iter()
                .filter(|rule| rule.action == action && rule.resource.as_str() != "*")
            {
                patterns.insert(
                    rule.resource.as_str().to_owned(),
                    EffectivePermissionRule {
                        resource: rule.resource.clone(),
                        effect: rule.effect,
                        source: PermissionRuleSource::AgentDocument,
                    },
                );
            }
        }
        for rule in overlay
            .rules
            .iter()
            .filter(|rule| rule.action == action && rule.resource.as_str() != "*")
        {
            patterns.insert(
                rule.resource.as_str().to_owned(),
                EffectivePermissionRule {
                    resource: rule.resource.clone(),
                    effect: rule.effect,
                    source: PermissionRuleSource::SessionOverlay,
                },
            );
        }
        EffectivePermissionAction {
            action,
            effect,
            source,
            patterns: patterns.into_values().collect(),
        }
    })
    .collect()
}

#[cfg(test)]
mod tests;
