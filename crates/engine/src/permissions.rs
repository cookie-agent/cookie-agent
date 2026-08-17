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
            "mcp" => Ok(PermissionAction::Mcp),
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
        let effect = if evaluations
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
        };
        PermissionDecision {
            effect,
            evaluations,
        }
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
        let effective_overlay = overlay
            .into_iter()
            .flat_map(|overlay| overlay.rules.iter())
            .filter(|rule| rule.action == action)
            .map(|rule| (rule.resource.as_str(), rule.effect))
            .collect::<HashMap<_, _>>();
        if effective_overlay
            .values()
            .any(|effect| *effect != PermissionEffect::Deny)
        {
            return true;
        }
        let effective_policy = policy
            .permissions
            .iter()
            .filter(|rule| rule.action == action)
            .map(|rule| (rule.resource.as_str(), rule.effect))
            .collect::<HashMap<_, _>>();

        // Visibility only estimates whether some resource may be usable. Treat
        // each policy pattern as a candidate string for overlay deny matching;
        // operation evaluation remains authoritative for actual resources.
        effective_policy.iter().any(|(resource, effect)| {
            *effect != PermissionEffect::Deny
                && !effective_overlay.iter().any(|(pattern, effect)| {
                    *effect == PermissionEffect::Deny
                        && (*pattern == "*" || simple_wildcard_match(pattern, resource))
                })
        })
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
                    PermissionEffect::Ask,
                    "no bare or `*` rule; ask by default for a permission-name-only check".into(),
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
    let protected_env = action == PermissionAction::Read && protected_env_resource(resource);
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
                && !(protected_env
                    && rule.effect == PermissionEffect::Allow
                    && !permission_pattern_is_exact(
                        rule.resource.as_str(),
                        resource,
                        &absolute_resource,
                        workspace,
                    ))
        })
        .max_by_key(|(index, rule)| specificity(rule.resource.as_str(), *index));
    winner
        .map_or_else(
            || {
                if protected_env {
                    (
                        PermissionEffect::Ask,
                        "built-in .env read guard asks by default; no exact allow overrides it"
                            .into(),
                    )
                } else {
                    (
                        PermissionEffect::Ask,
                        "no matching rule; ask by default".into(),
                    )
                }
            },
            |(_, rule)| {
                let reason = if protected_env
                    && rule.effect == PermissionEffect::Allow
                    && permission_pattern_is_exact(
                        rule.resource.as_str(),
                        resource,
                        &absolute_resource,
                        workspace,
                    )
                {
                    "exact agent rule overrides the built-in .env default".into()
                } else {
                    "most-specific matching pattern: more literal characters, then fewer wildcards, then later declaration".into()
                };
                (rule.effect, reason)
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
    let protected_env = action == PermissionAction::Read && protected_env_resource(resource);
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
                    && !(protected_env
                        && rule.effect == PermissionEffect::Allow
                        && !permission_pattern_is_exact(
                            rule.resource.as_str(),
                            resource,
                            &absolute_resource,
                            workspace,
                        ))
            })
            .max_by_key(|(index, rule)| specificity(rule.resource.as_str(), *index))
    });
    winner.map_or_else(
        || effective_permission(policy, action, resource, workspace),
        |(_, rule)| {
            let reason = if protected_env && rule.effect == PermissionEffect::Allow {
                "exact session overlay rule overrides the built-in .env default"
            } else {
                "session overlay matching rule takes precedence over the agent document"
            };
            (rule.effect, reason.into())
        },
    )
}

fn permission_pattern_is_exact(
    pattern: &str,
    relative_resource: &str,
    absolute_resource: &str,
    workspace: &Path,
) -> bool {
    if pattern
        .chars()
        .any(|character| matches!(character, '*' | '?'))
    {
        return false;
    }
    if pattern.contains(cookie_agent_protocol::WildcardPattern::WORKSPACE_DIR_EXPRESSION) {
        expand_workspace_pattern(pattern, workspace) == absolute_resource
    } else {
        pattern == relative_resource
    }
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

fn protected_env_resource(resource: &str) -> bool {
    let name = resource.rsplit('/').next().unwrap_or(resource);
    (name == ".env" || name.starts_with(".env.")) && !name.ends_with(".example")
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
        })
    }

    pub async fn set_session_permission(
        &self,
        session_id: SessionId,
        action: PermissionAction,
        resource: WildcardPattern,
        effect: PermissionEffect,
    ) -> Result<SessionPermissionMutationResult, crate::EngineError> {
        let _mutation = self.inner.permission_overlay_mutation.lock().await;
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
    ) -> Result<SessionPermissionMutationResult, crate::EngineError> {
        let _mutation = self.inner.permission_overlay_mutation.lock().await;
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
        self.inner.approvals.invalidate_grants(&ids);
        Ok(())
    }
}

fn governing_agent(session: &crate::session::SessionProjection) -> AgentSnapshot {
    let latest_run = session
        .log
        .events()
        .into_iter()
        .rev()
        .find_map(|event| match event.payload {
            EventPayload::RunStarted { agent, .. } => Some(*agent),
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
        PermissionAction::Mcp,
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
                    (PermissionEffect::Ask, PermissionRuleSource::Default),
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
mod tests {
    use cookie_agent_protocol::{
        AgentDocumentSource, AgentId, AgentMode, AgentSchemaVersion, AgentSnapshot,
        ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, PermissionAction,
        PermissionEffect, PermissionRule, PreparedApprovalResource, PreparedBindingLifetime,
        PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
        PreparedResourceIdentity, SafeCode, SessionPermissionOverlay, Sha256Digest,
        WildcardPattern,
    };

    use super::{PermissionPipeline, select_governing_agent};

    fn policy(rules: Vec<PermissionRule>) -> AgentSnapshot {
        AgentSnapshot {
            agent: AgentId::new("test").expect("agent id"),
            schema: AgentSchemaVersion::current(),
            mode: AgentMode::Primary,
            description: "Test agent".into(),
            document_source: AgentDocumentSource::Workspace,
            document_fingerprint: Sha256Digest::of_bytes(b"test document"),
            composed_prompt: "Test permission evaluation.\n".into(),
            prompt_fingerprint: Sha256Digest::of_bytes(b"Test permission evaluation.\n"),
            max_output_tokens: 0,
            permissions: rules,
            delegation: None,
            fallback_chain: Vec::new(),
            selected_suffix_start: 0,
        }
    }

    #[test]
    fn later_run_agent_governs_permission_mutation_classification() {
        let creation = policy(vec![rule(
            "creation",
            PermissionAction::Read,
            "*",
            PermissionEffect::Allow,
        )]);
        let later = policy(vec![rule(
            "later",
            PermissionAction::Read,
            "*",
            PermissionEffect::Deny,
        )]);

        assert_eq!(
            select_governing_agent(&creation, Some(&later)).permissions,
            later.permissions
        );
        assert_eq!(
            select_governing_agent(&creation, None).permissions,
            creation.permissions
        );
    }

    fn rule(
        _id: &str,
        action: PermissionAction,
        resource: &str,
        effect: PermissionEffect,
    ) -> PermissionRule {
        PermissionRule {
            action,
            resource: WildcardPattern::new(resource).expect("wildcard"),
            effect,
        }
    }

    fn resource(action: PermissionAction, label: &str, binding: &[u8]) -> PreparedApprovalResource {
        PreparedApprovalResource {
            capability: action,
            canonical: PreparedResourceIdentity::new(format!(
                "label:{}",
                Sha256Digest::of_bytes(label.as_bytes()).as_str()
            ))
            .expect("identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(binding),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::CommandPrefix {
                prefix: label.into(),
            },
            source: ApprovalResourceSource::PrimaryOperation,
        }
    }

    fn operation(resources: Vec<PreparedApprovalResource>) -> PreparedOperationIdentity {
        let action = resources.first().expect("test resources").capability;
        let capabilities = vec![ApprovalCapability {
            action,
            operation: PreparedCapabilityOperation::new("permission:test").expect("operation"),
        }];
        PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"args"),
            capabilities,
            resources,
            Sha256Digest::of_bytes(b"context"),
        )
        .expect("operation")
    }

    fn decide(
        policy: &AgentSnapshot,
        resource: PreparedApprovalResource,
    ) -> super::PermissionDecision {
        decide_many(policy, vec![resource])
    }

    fn decide_many(
        policy: &AgentSnapshot,
        resources: Vec<PreparedApprovalResource>,
    ) -> super::PermissionDecision {
        let labels = resources
            .iter()
            .map(|resource| match &resource.boundary {
                ApprovalBoundary::CommandPrefix { prefix } => Some(prefix.clone()),
                _ => unreachable!("test resources carry explicit labels"),
            })
            .collect::<Vec<_>>();
        PermissionPipeline::default().decide_operation(
            policy,
            &operation(resources),
            &labels,
            std::path::Path::new("/workspace"),
        )
    }

    fn decide_loose(policy: &AgentSnapshot, action: PermissionAction) -> super::PermissionDecision {
        let prepared_resource = resource(action, "unscoped-test-identity", b"permission-name-only");
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"loose args"),
            vec![ApprovalCapability {
                action,
                operation: PreparedCapabilityOperation::new("permission:loose")
                    .expect("loose operation"),
            }],
            vec![prepared_resource],
            Sha256Digest::of_bytes(b"loose context"),
        )
        .expect("loose prepared operation");
        PermissionPipeline::default().decide_operation(
            policy,
            &operation,
            &[None],
            std::path::Path::new("/workspace"),
        )
    }

    #[test]
    fn loose_permission_uses_only_bare_or_wildcard_effect() {
        let bare_allow = cookie_agent_config::PermissionValue::Effect(PermissionEffect::Allow)
            .rules(PermissionAction::Delegate);
        assert_eq!(
            decide_loose(&policy(bare_allow), PermissionAction::Delegate).effect,
            PermissionEffect::Allow
        );
        for effect in [
            PermissionEffect::Allow,
            PermissionEffect::Ask,
            PermissionEffect::Deny,
        ] {
            let decision = decide_loose(
                &policy(vec![rule(
                    "wildcard",
                    PermissionAction::Delegate,
                    "*",
                    effect,
                )]),
                PermissionAction::Delegate,
            );
            assert_eq!(decision.effect, effect);
            assert_eq!(decision.evaluations[0].trace.candidates.len(), 1);
        }
        let specific_only = decide_loose(
            &policy(vec![rule(
                "specific",
                PermissionAction::Delegate,
                "reviewer",
                PermissionEffect::Allow,
            )]),
            PermissionAction::Delegate,
        );
        assert_eq!(specific_only.effect, PermissionEffect::Ask);
        assert!(specific_only.evaluations[0].trace.candidates.is_empty());
    }

    #[test]
    fn former_loose_marker_literal_remains_a_scoped_resource() {
        let literal = "<permission-name-only>";
        let decision = decide(
            &policy(vec![
                rule(
                    "literal-deny",
                    PermissionAction::Bash,
                    literal,
                    PermissionEffect::Deny,
                ),
                rule(
                    "fallback-allow",
                    PermissionAction::Bash,
                    "*",
                    PermissionEffect::Allow,
                ),
            ]),
            resource(PermissionAction::Bash, literal, literal.as_bytes()),
        );
        assert_eq!(decision.effect, PermissionEffect::Deny);
        assert_eq!(decision.evaluations[0].trace.normalized_resource, literal);
    }

    #[test]
    fn delegate_spawn_matches_agent_pattern_while_session_tools_ignore_it() {
        let delegate_policy = policy(vec![
            rule(
                "reviewer",
                PermissionAction::Delegate,
                "reviewer",
                PermissionEffect::Allow,
            ),
            rule(
                "fallback",
                PermissionAction::Delegate,
                "*",
                PermissionEffect::Deny,
            ),
        ]);
        let spawn = decide(
            &delegate_policy,
            resource(PermissionAction::Delegate, "reviewer", b"reviewer"),
        );
        assert_eq!(spawn.effect, PermissionEffect::Allow);
        let session_tool = decide_loose(&delegate_policy, PermissionAction::Delegate);
        assert_eq!(session_tool.effect, PermissionEffect::Deny);

        let specific_deny_with_wildcard_allow = decide_loose(
            &policy(vec![
                rule(
                    "reviewer",
                    PermissionAction::Delegate,
                    "reviewer",
                    PermissionEffect::Deny,
                ),
                rule(
                    "fallback",
                    PermissionAction::Delegate,
                    "*",
                    PermissionEffect::Allow,
                ),
            ]),
            PermissionAction::Delegate,
        );
        assert_eq!(
            specific_deny_with_wildcard_allow.effect,
            PermissionEffect::Allow
        );
    }

    #[test]
    fn mcp_permissions_are_scoped_by_generated_tool_name() {
        let mcp_policy = policy(vec![
            rule(
                "server-allow",
                PermissionAction::Mcp,
                "github_*",
                PermissionEffect::Allow,
            ),
            rule(
                "tool-deny",
                PermissionAction::Mcp,
                "github_delete_repo",
                PermissionEffect::Deny,
            ),
        ]);
        assert_eq!(
            decide(
                &mcp_policy,
                resource(PermissionAction::Mcp, "github_search", b"search")
            )
            .effect,
            PermissionEffect::Allow
        );
        assert_eq!(
            decide(
                &mcp_policy,
                resource(PermissionAction::Mcp, "github_delete_repo", b"delete")
            )
            .effect,
            PermissionEffect::Deny
        );
        assert_eq!(
            decide(
                &mcp_policy,
                resource(PermissionAction::Mcp, "slack_search", b"unmatched")
            )
            .effect,
            PermissionEffect::Ask
        );
        assert_eq!(
            PermissionPipeline::action_for_permission_name("mcp").expect("MCP action"),
            PermissionAction::Mcp
        );
        assert!(PermissionPipeline::tool_visible(&mcp_policy, "mcp"));
        assert!(!PermissionPipeline::tool_visible(
            &policy(Vec::new()),
            "mcp"
        ));
        assert!(!PermissionPipeline::tool_visible(
            &policy(vec![rule(
                "deny-all",
                PermissionAction::Mcp,
                "*",
                PermissionEffect::Deny,
            )]),
            "mcp"
        ));
        assert!(PermissionPipeline::tool_visible(
            &policy(vec![
                rule(
                    "deny-all",
                    PermissionAction::Mcp,
                    "*",
                    PermissionEffect::Deny,
                ),
                rule(
                    "allow-github",
                    PermissionAction::Mcp,
                    "github_*",
                    PermissionEffect::Allow,
                ),
            ]),
            "mcp"
        ));
    }

    #[test]
    fn session_overlay_precedes_more_specific_agent_rules() {
        let policy = policy(vec![rule(
            "agent-specific",
            PermissionAction::Bash,
            "git status",
            PermissionEffect::Allow,
        )]);
        let overlay = SessionPermissionOverlay {
            rules: vec![rule(
                "overlay-wildcard",
                PermissionAction::Bash,
                "*",
                PermissionEffect::Deny,
            )],
        };
        let decision = PermissionPipeline::default().decide_operation_with_overlay(
            &policy,
            Some(&overlay),
            &operation(vec![resource(
                PermissionAction::Bash,
                "git status",
                b"git status",
            )]),
            &[Some("git status".into())],
            std::path::Path::new("/workspace"),
        );
        assert_eq!(decision.effect, PermissionEffect::Deny);
        assert_eq!(
            decision.evaluations[0]
                .trace
                .candidates
                .last()
                .expect("overlay candidate")
                .source_layer
                .as_str(),
            "session_overlay"
        );
    }

    #[test]
    fn exact_rule_matches_normalized_label_not_opaque_identity() {
        let decision = decide(
            &policy(vec![rule(
                "exact",
                PermissionAction::Read,
                "/workspace/a.txt",
                PermissionEffect::Allow,
            )]),
            resource(PermissionAction::Read, "/workspace/a.txt", b"file"),
        );
        assert_eq!(decision.effect, PermissionEffect::Allow);
        assert_eq!(
            decision.evaluations[0].trace.normalized_resource,
            "/workspace/a.txt"
        );
    }

    #[test]
    fn more_literals_win_before_wildcard_count() {
        let decision = decide(
            &policy(vec![
                rule(
                    "allow",
                    PermissionAction::Read,
                    "/workspace/*",
                    PermissionEffect::Allow,
                ),
                rule(
                    "deny",
                    PermissionAction::Read,
                    "*/secret.txt",
                    PermissionEffect::Deny,
                ),
            ]),
            resource(PermissionAction::Read, "/workspace/secret.txt", b"secret"),
        );
        assert_eq!(decision.effect, PermissionEffect::Deny);
        assert_eq!(decision.evaluations[0].trace.candidates.len(), 2);
    }

    #[test]
    fn universal_catch_all_is_least_specific() {
        let decision = decide(
            &policy(vec![
                rule(
                    "allow",
                    PermissionAction::Read,
                    "*",
                    PermissionEffect::Allow,
                ),
                rule(
                    "deny",
                    PermissionAction::Read,
                    "*/.env.*",
                    PermissionEffect::Deny,
                ),
            ]),
            resource(PermissionAction::Read, "nested/.env.local", b"secret"),
        );
        assert_eq!(decision.effect, PermissionEffect::Deny);
        assert_eq!(decision.evaluations[0].trace.candidates.len(), 2);
    }

    #[test]
    fn later_declaration_wins_final_specificity_tie() {
        let decision = decide(
            &policy(vec![
                rule(
                    "earlier-deny",
                    PermissionAction::Read,
                    "/workspace/*",
                    PermissionEffect::Deny,
                ),
                rule(
                    "later-allow",
                    PermissionAction::Read,
                    "/workspace/*",
                    PermissionEffect::Allow,
                ),
            ]),
            resource(PermissionAction::Read, "/workspace/public.txt", b"public"),
        );
        assert_eq!(decision.effect, PermissionEffect::Allow);
        assert_eq!(decision.evaluations[0].trace.candidates.len(), 2);
        assert_eq!(
            decision.evaluations[0].trace.candidates[1].source_layer,
            SafeCode::new("agent_document").expect("safe code")
        );
    }

    #[test]
    fn steer_subagent_uses_loose_delegate_permission_and_can_be_denied() {
        let session = cookie_agent_protocol::SessionId::new_v7().to_string();
        let resource = resource(PermissionAction::Delegate, &session, session.as_bytes());
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"steer args"),
            vec![ApprovalCapability {
                action: PermissionAction::Delegate,
                operation: PreparedCapabilityOperation::new("steer_subagent:steer")
                    .expect("steer operation"),
            }],
            vec![resource],
            Sha256Digest::of_bytes(b"context"),
        )
        .expect("prepared steer operation");
        let decision = PermissionPipeline::default().decide_operation(
            &policy(vec![rule(
                "deny-session-tools",
                PermissionAction::Delegate,
                "*",
                PermissionEffect::Deny,
            )]),
            &operation,
            &[None],
            std::path::Path::new("/workspace"),
        );
        assert_eq!(
            PermissionPipeline::action_for_permission_name("delegate").expect("delegate action"),
            PermissionAction::Delegate
        );
        assert_eq!(decision.effect, PermissionEffect::Deny);
    }

    #[test]
    fn wildcard_allow_applies_to_non_secret_workspace_path() {
        let decision = decide(
            &policy(vec![rule(
                "allow",
                PermissionAction::Read,
                "/workspace/*",
                PermissionEffect::Allow,
            )]),
            resource(PermissionAction::Read, "/workspace/public.txt", b"public"),
        );
        assert_eq!(decision.effect, PermissionEffect::Allow);
    }

    #[test]
    fn multi_resource_deny_wins_over_allow() {
        let decision = decide_many(
            &policy(vec![
                rule(
                    "allow-public",
                    PermissionAction::Read,
                    "/workspace/public.txt",
                    PermissionEffect::Allow,
                ),
                rule(
                    "deny-secret",
                    PermissionAction::Read,
                    "/workspace/secret.txt",
                    PermissionEffect::Deny,
                ),
            ]),
            vec![
                resource(PermissionAction::Read, "/workspace/public.txt", b"public"),
                resource(PermissionAction::Read, "/workspace/secret.txt", b"secret"),
            ],
        );
        assert_eq!(decision.effect, PermissionEffect::Deny);
        assert_eq!(decision.evaluations.len(), 2);
    }

    #[test]
    fn multi_resource_ask_beats_allow() {
        let decision = decide_many(
            &policy(vec![rule(
                "allow-public",
                PermissionAction::Read,
                "/workspace/public.txt",
                PermissionEffect::Allow,
            )]),
            vec![
                resource(PermissionAction::Read, "/workspace/public.txt", b"public"),
                resource(PermissionAction::Read, "/workspace/review.txt", b"review"),
            ],
        );
        assert_eq!(decision.effect, PermissionEffect::Ask);
        assert_eq!(decision.evaluations.len(), 2);
    }

    #[test]
    fn absolute_rule_controls_outside_read() {
        let decision = decide(
            &policy(vec![
                rule(
                    "read-all",
                    PermissionAction::Read,
                    "*",
                    PermissionEffect::Allow,
                ),
                rule(
                    "outside-etc",
                    PermissionAction::Read,
                    "/etc/*",
                    PermissionEffect::Ask,
                ),
            ]),
            resource(PermissionAction::Read, "/etc/passwd", b"passwd"),
        );
        assert_eq!(decision.effect, PermissionEffect::Ask);
    }

    #[test]
    fn absolute_deny_pattern_catches_outside_ssh_read() {
        let decision = decide(
            &policy(vec![
                rule(
                    "read-all",
                    PermissionAction::Read,
                    "*",
                    PermissionEffect::Allow,
                ),
                rule(
                    "deny-ssh",
                    PermissionAction::Read,
                    "*/.ssh/*",
                    PermissionEffect::Deny,
                ),
            ]),
            resource(
                PermissionAction::Read,
                "/home/other/.ssh/id_ed25519",
                b"ssh-key",
            ),
        );
        assert_eq!(decision.effect, PermissionEffect::Deny);
    }

    #[test]
    fn env_read_guard_overrides_allow_all_at_root_and_nested_paths() {
        let allow_all = policy(vec![rule(
            "allow-all",
            PermissionAction::Read,
            "*",
            PermissionEffect::Allow,
        )]);
        for path in [".env", "nested/.env.local"] {
            let decision = decide(
                &allow_all,
                resource(PermissionAction::Read, path, path.as_bytes()),
            );
            assert_eq!(decision.effect, PermissionEffect::Ask, "{path}");
            assert_eq!(
                decision.evaluations[0].trace.precedence_reason,
                "built-in .env read guard asks by default; no exact allow overrides it"
            );
        }
    }

    #[test]
    fn exact_env_rules_override_default_ask_but_generic_allow_does_not() {
        for (effect, expected) in [
            (PermissionEffect::Allow, PermissionEffect::Allow),
            (PermissionEffect::Deny, PermissionEffect::Deny),
        ] {
            let decision = decide(
                &policy(vec![
                    rule(
                        "allow-all",
                        PermissionAction::Read,
                        "*",
                        PermissionEffect::Allow,
                    ),
                    rule("exact-env", PermissionAction::Read, ".env", effect),
                ]),
                resource(PermissionAction::Read, ".env", b"env"),
            );
            assert_eq!(decision.effect, expected);
        }
    }

    #[test]
    fn later_generic_allow_cannot_override_exact_env_deny() {
        let decision = decide(
            &policy(vec![
                rule(
                    "exact-deny",
                    PermissionAction::Read,
                    "nested/.env.local",
                    PermissionEffect::Deny,
                ),
                rule(
                    "allow-all",
                    PermissionAction::Read,
                    "*",
                    PermissionEffect::Allow,
                ),
            ]),
            resource(PermissionAction::Read, "nested/.env.local", b"env"),
        );
        assert_eq!(decision.effect, PermissionEffect::Deny);
    }

    #[test]
    fn env_example_read_is_not_guarded() {
        let decision = decide(
            &policy(vec![rule(
                "allow-all",
                PermissionAction::Read,
                "*",
                PermissionEffect::Allow,
            )]),
            resource(
                PermissionAction::Read,
                "nested/.env.production.example",
                b"example",
            ),
        );
        assert_eq!(decision.effect, PermissionEffect::Allow);
    }

    #[test]
    fn tool_visibility_requires_an_effective_non_deny_rule() {
        let hidden = policy(vec![rule(
            "deny-all",
            PermissionAction::Read,
            "*",
            PermissionEffect::Deny,
        )]);
        assert!(!PermissionPipeline::tool_visible(&hidden, "read"));

        let specific_exception = policy(vec![
            rule(
                "deny-all",
                PermissionAction::Read,
                "*",
                PermissionEffect::Deny,
            ),
            rule(
                "allow-readme",
                PermissionAction::Read,
                "README.md",
                PermissionEffect::Allow,
            ),
        ]);
        assert!(PermissionPipeline::tool_visible(
            &specific_exception,
            "read"
        ));

        let denied_again = policy(vec![
            rule(
                "deny-all",
                PermissionAction::Read,
                "*",
                PermissionEffect::Deny,
            ),
            rule(
                "allow-readme",
                PermissionAction::Read,
                "README.md",
                PermissionEffect::Allow,
            ),
            rule(
                "deny-all-later",
                PermissionAction::Read,
                "*",
                PermissionEffect::Deny,
            ),
        ]);
        assert!(PermissionPipeline::tool_visible(&denied_again, "read"));

        let delegate_hidden = policy(vec![rule(
            "deny-delegate",
            PermissionAction::Delegate,
            "*",
            PermissionEffect::Deny,
        )]);
        assert!(!PermissionPipeline::tool_visible(
            &delegate_hidden,
            "delegate"
        ));
        let delegate_exception = policy(vec![
            rule(
                "deny-delegate",
                PermissionAction::Delegate,
                "*",
                PermissionEffect::Deny,
            ),
            rule(
                "allow-reviewer",
                PermissionAction::Delegate,
                "reviewer",
                PermissionEffect::Allow,
            ),
        ]);
        assert!(PermissionPipeline::tool_visible(
            &delegate_exception,
            "delegate"
        ));

        for permission_name in ["write", "bash"] {
            assert!(!PermissionPipeline::tool_visible(
                &policy(vec![rule(
                    "deny-action",
                    PermissionPipeline::action_for_permission_name(permission_name).unwrap(),
                    "*",
                    PermissionEffect::Deny,
                )]),
                permission_name,
            ));
        }

        assert!(!PermissionPipeline::tool_visible(
            &policy(Vec::new()),
            "unknown"
        ));
    }

    #[test]
    fn empty_permissions_hide_all_known_actions() {
        let policy = policy(Vec::new());
        for permission_name in ["read", "write", "bash", "delegate", "mcp"] {
            assert!(!PermissionPipeline::tool_visible(&policy, permission_name));
        }
    }

    #[test]
    fn session_overlay_can_expose_or_hide_tools() {
        let empty_policy = policy(Vec::new());
        let named_allow = SessionPermissionOverlay {
            rules: vec![rule(
                "overlay-named-allow",
                PermissionAction::Read,
                "README.md",
                PermissionEffect::Allow,
            )],
        };
        assert!(PermissionPipeline::tool_visible_with_overlay(
            &empty_policy,
            Some(&named_allow),
            "read"
        ));

        let deny_with_exception = SessionPermissionOverlay {
            rules: vec![
                rule(
                    "overlay-wildcard-deny",
                    PermissionAction::Read,
                    "*",
                    PermissionEffect::Deny,
                ),
                rule(
                    "overlay-named-ask",
                    PermissionAction::Read,
                    "README.md",
                    PermissionEffect::Ask,
                ),
            ],
        };
        assert!(PermissionPipeline::tool_visible_with_overlay(
            &empty_policy,
            Some(&deny_with_exception),
            "read"
        ));

        let wildcard_deny = SessionPermissionOverlay {
            rules: vec![rule(
                "overlay-wildcard-deny",
                PermissionAction::Read,
                "*",
                PermissionEffect::Deny,
            )],
        };
        let policy_allow = policy(vec![rule(
            "policy-allow",
            PermissionAction::Read,
            "*",
            PermissionEffect::Allow,
        )]);
        assert!(!PermissionPipeline::tool_visible_with_overlay(
            &policy_allow,
            Some(&wildcard_deny),
            "read"
        ));

        let named_policy_allow = policy(vec![rule(
            "policy-named-allow",
            PermissionAction::Read,
            "README.md",
            PermissionEffect::Allow,
        )]);
        let identical_named_deny = SessionPermissionOverlay {
            rules: vec![rule(
                "overlay-named-deny",
                PermissionAction::Read,
                "README.md",
                PermissionEffect::Deny,
            )],
        };
        assert!(!PermissionPipeline::tool_visible_with_overlay(
            &named_policy_allow,
            Some(&identical_named_deny),
            "read"
        ));

        let markdown_deny = SessionPermissionOverlay {
            rules: vec![rule(
                "overlay-markdown-deny",
                PermissionAction::Read,
                "*.md",
                PermissionEffect::Deny,
            )],
        };
        assert!(!PermissionPipeline::tool_visible_with_overlay(
            &named_policy_allow,
            Some(&markdown_deny),
            "read"
        ));

        let text_deny = SessionPermissionOverlay {
            rules: vec![rule(
                "overlay-text-deny",
                PermissionAction::Read,
                "*.txt",
                PermissionEffect::Deny,
            )],
        };
        assert!(PermissionPipeline::tool_visible_with_overlay(
            &named_policy_allow,
            Some(&text_deny),
            "read"
        ));

        let policy_wildcard_allow = policy(vec![rule(
            "policy-wildcard-allow",
            PermissionAction::Read,
            "*",
            PermissionEffect::Allow,
        )]);
        assert!(PermissionPipeline::tool_visible_with_overlay(
            &policy_wildcard_allow,
            Some(&identical_named_deny),
            "read"
        ));
    }

    #[test]
    fn workspace_dir_pattern_matches_absolute_workspace_path_only() {
        let policy = policy(vec![rule(
            "workspace-write",
            PermissionAction::Write,
            "${workspace_dir}/src/*",
            PermissionEffect::Allow,
        )]);
        assert_eq!(
            super::effective_permission(
                &policy,
                PermissionAction::Write,
                "src/main.rs",
                std::path::Path::new("/workspace"),
            )
            .0,
            PermissionEffect::Allow
        );
        assert_eq!(
            super::effective_permission(
                &policy,
                PermissionAction::Write,
                "/outside/src/main.rs",
                std::path::Path::new("/workspace"),
            )
            .0,
            PermissionEffect::Ask
        );
    }

    #[test]
    fn relative_and_workspace_dir_patterns_share_specificity_ordering() {
        let policy = policy(vec![
            rule(
                "relative",
                PermissionAction::Write,
                "src/*",
                PermissionEffect::Deny,
            ),
            rule(
                "absolute",
                PermissionAction::Write,
                "${workspace_dir}/src/*",
                PermissionEffect::Allow,
            ),
        ]);
        assert_eq!(
            super::effective_permission(
                &policy,
                PermissionAction::Write,
                "src/main.rs",
                std::path::Path::new("/workspace"),
            )
            .0,
            PermissionEffect::Allow
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_dir_expansion_canonicalizes_the_workspace_anchor() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("real-workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let alias = temp.path().join("workspace-alias");
        symlink(&workspace, &alias).expect("workspace symlink");
        let policy = policy(vec![rule(
            "workspace-write",
            PermissionAction::Write,
            "${workspace_dir}/src/*",
            PermissionEffect::Allow,
        )]);
        assert_eq!(
            super::effective_permission(&policy, PermissionAction::Write, "src/main.rs", &alias,).0,
            PermissionEffect::Allow
        );
    }
}
