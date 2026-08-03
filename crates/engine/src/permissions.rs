//! Permission evaluation over immutable protocol-v6 prepared-operation manifests.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

use cookie_agent_config::{PolicySnapshot, simple_wildcard_match};
use cookie_agent_protocol::{
    ActionKind, ApprovalEvaluation, DecisionTrace, Effect, MatchedPermissionRule,
    OperationFingerprint, PreparedOperationIdentity, SessionId, TreeApprovalGrant,
    TreeApprovalGrantId,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("unknown permission action `{0}`")]
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
    pub fn grant(&self, grant: TreeApprovalGrant) {
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                ApprovalKey {
                    root: grant.root_session_id(),
                    fingerprint: grant.operation_fingerprint().clone(),
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
                grant.capabilities() == operation.capabilities()
                    && grant.resources() == operation.resources()
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
            .retain(|_, grant| !ids.contains(&grant.grant_id()));
    }
}

#[derive(Clone, Debug)]
pub struct PermissionDecision {
    pub effect: Effect,
    pub evaluations: Vec<ApprovalEvaluation>,
}

#[derive(Debug, Default)]
pub struct PermissionPipeline {
    _private: (),
}

impl PermissionPipeline {
    pub fn action_for_tool(tool: &str) -> Result<ActionKind, PermissionError> {
        match tool {
            "read" => Ok(ActionKind::Read),
            "write" | "edit" => Ok(ActionKind::Write),
            "bash" => Ok(ActionKind::Bash),
            "grep" => Ok(ActionKind::Grep),
            "glob" => Ok(ActionKind::Glob),
            "delegate" => Ok(ActionKind::Delegate),
            "external_directory" => Ok(ActionKind::ExternalDirectory),
            other => Err(PermissionError::UnknownAction(other.into())),
        }
    }

    #[must_use]
    pub fn decide_operation(
        &self,
        policy: &PolicySnapshot,
        operation: &PreparedOperationIdentity,
        policy_labels: &[String],
    ) -> PermissionDecision {
        assert_eq!(operation.resources().len(), policy_labels.len());
        let evaluations = operation
            .resources()
            .iter()
            .zip(policy_labels)
            .map(|(resource, normalized)| {
                let candidates = matching_rules(policy, resource.capability, normalized);
                let (effect, reason) = candidates.last().map_or(
                    (Effect::Ask, "no matching rule; ask by default".to_owned()),
                    |rule| (rule.effect, "last matching rule".to_owned()),
                );
                ApprovalEvaluation {
                    resource_digest: resource.binding_digest.clone(),
                    effect,
                    trace: DecisionTrace {
                        action: resource.capability,
                        normalized_resource: normalized.clone(),
                        candidates,
                        effect,
                        precedence_reason: reason,
                    },
                }
            })
            .collect::<Vec<_>>();
        let effect = if evaluations.iter().any(|item| item.effect == Effect::Deny) {
            Effect::Deny
        } else if evaluations.iter().any(|item| item.effect == Effect::Ask) {
            Effect::Ask
        } else {
            Effect::Allow
        };
        PermissionDecision {
            effect,
            evaluations,
        }
    }

    #[must_use]
    pub fn tool_visible(policy: &PolicySnapshot, tool: &str) -> bool {
        let Ok(action) = Self::action_for_tool(tool) else {
            return true;
        };
        policy
            .permissions
            .rules
            .iter()
            .rfind(|rule| {
                action_from_config(&rule.action).ok() == Some(action) && rule.resource == "*"
            })
            .is_none_or(|rule| effect(&rule.effect) != Effect::Deny)
    }
}

fn matching_rules(
    policy: &PolicySnapshot,
    action: ActionKind,
    resource: &str,
) -> Vec<MatchedPermissionRule> {
    policy
        .permissions
        .rules
        .iter()
        .filter(|rule| {
            action_from_config(&rule.action).ok() == Some(action)
                && simple_wildcard_match(&rule.resource, resource)
        })
        .map(|rule| MatchedPermissionRule {
            rule_id: Some(rule.id.clone()),
            source_layer: format!("{:?}", rule.source).to_ascii_lowercase(),
            effect: effect(&rule.effect),
        })
        .collect()
}

fn action_from_config(action: &str) -> Result<ActionKind, PermissionError> {
    PermissionPipeline::action_for_tool(action)
}

fn effect(effect: &str) -> Effect {
    match effect {
        "allow" => Effect::Allow,
        "deny" => Effect::Deny,
        _ => Effect::Ask,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs};

    use cookie_agent_config::{
        AgentType, DelegationPolicy, DepthLimit, PermissionRule, PolicySnapshot, ProfileSnapshot,
        ResolvedPermissions, ResultLimits, RuleSource, load_layered,
    };
    use cookie_agent_protocol::{
        ActionKind, ApprovalBoundary, ApprovalCapability, ApprovalResourceSource,
        PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
        PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity, Sha256Digest,
    };

    use super::PermissionPipeline;

    fn policy(rules: Vec<PermissionRule>) -> PolicySnapshot {
        PolicySnapshot {
            profile: ProfileSnapshot {
                name: "test".into(),
                r#type: AgentType::Primary,
            },
            models: Vec::new(),
            tools: BTreeSet::new(),
            permissions: ResolvedPermissions { rules },
            delegation: DelegationPolicy {
                enabled: false,
                allowed_profiles: BTreeSet::new(),
                depth_limit: DepthLimit::Finite(0),
            },
            result_limits: ResultLimits::default(),
        }
    }

    fn rule(id: &str, action: &str, resource: &str, effect: &str) -> PermissionRule {
        PermissionRule {
            id: id.into(),
            action: action.into(),
            resource: resource.into(),
            effect: effect.into(),
            source: RuleSource::User,
        }
    }

    fn resource(action: ActionKind, label: &str, binding: &[u8]) -> PreparedApprovalResource {
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
            source: if action == ActionKind::ExternalDirectory {
                ApprovalResourceSource::ExternalDirectoryGuard
            } else {
                ApprovalResourceSource::PrimaryOperation
            },
        }
    }

    fn operation(resources: Vec<PreparedApprovalResource>) -> PreparedOperationIdentity {
        let mut capabilities = vec![ApprovalCapability {
            action: ActionKind::Read,
            operation: PreparedCapabilityOperation::new("read:read").expect("operation"),
        }];
        if resources
            .iter()
            .any(|resource| resource.capability == ActionKind::ExternalDirectory)
        {
            capabilities.push(ApprovalCapability {
                action: ActionKind::ExternalDirectory,
                operation: PreparedCapabilityOperation::new("read:external").expect("operation"),
            });
        }
        PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"args"),
            capabilities,
            resources,
            Sha256Digest::of_bytes(b"context"),
        )
        .expect("operation")
    }

    fn decide(
        policy: &PolicySnapshot,
        resources: Vec<PreparedApprovalResource>,
    ) -> super::PermissionDecision {
        let labels = resources
            .iter()
            .map(|resource| match &resource.boundary {
                ApprovalBoundary::CommandPrefix { prefix } => prefix.clone(),
                _ => unreachable!("test resources carry explicit labels"),
            })
            .collect::<Vec<_>>();
        PermissionPipeline::default().decide_operation(policy, &operation(resources), &labels)
    }

    #[test]
    fn exact_rule_matches_normalized_label_not_opaque_identity() {
        let decision = decide(
            &policy(vec![rule("exact", "read", "/workspace/a.txt", "allow")]),
            vec![resource(ActionKind::Read, "/workspace/a.txt", b"file")],
        );
        assert_eq!(decision.effect, cookie_agent_protocol::Effect::Allow);
        assert_eq!(
            decision.evaluations[0].trace.normalized_resource,
            "/workspace/a.txt"
        );
    }

    #[test]
    fn wildcard_and_last_matching_deny_are_applied_to_labels() {
        let decision = decide(
            &policy(vec![
                rule("allow", "read", "/workspace/*", "allow"),
                rule("deny", "read", "*/secret.txt", "deny"),
            ]),
            vec![resource(
                ActionKind::Read,
                "/workspace/secret.txt",
                b"secret",
            )],
        );
        assert_eq!(decision.effect, cookie_agent_protocol::Effect::Deny);
        assert_eq!(decision.evaluations[0].trace.candidates.len(), 2);
    }

    #[test]
    fn later_workspace_allow_overrides_user_deny() {
        let temp = tempfile::TempDir::new().expect("temporary config directory");
        let user = temp.path().join("user.toml");
        let workspace = temp.path().join("workspace.toml");
        fs::write(
            &user,
            "[[permissions.rules]]\nid = \"user-deny\"\naction = \"read\"\nresource = \"/workspace/*\"\neffect = \"deny\"\n",
        )
        .expect("write user config");
        fs::write(
            &workspace,
            "[[permissions.rules]]\nid = \"workspace-allow\"\naction = \"read\"\nresource = \"/workspace/*\"\neffect = \"allow\"\n",
        )
        .expect("write workspace config");
        let config = load_layered(Some(&user), Some(&workspace)).expect("load layered config");
        assert_eq!(
            config
                .permissions
                .rules
                .iter()
                .map(|rule| (rule.id.as_str(), rule.source))
                .collect::<Vec<_>>(),
            [
                ("user-deny", RuleSource::User),
                ("workspace-allow", RuleSource::Workspace),
            ]
        );

        let decision = decide(
            &policy(config.permissions.rules),
            vec![resource(
                ActionKind::Read,
                "/workspace/public.txt",
                b"public",
            )],
        );
        assert_eq!(decision.effect, cookie_agent_protocol::Effect::Allow);
        assert_eq!(decision.evaluations[0].trace.candidates.len(), 2);
        assert_eq!(
            decision.evaluations[0].trace.candidates[1].source_layer,
            "workspace"
        );
    }

    #[test]
    fn wildcard_allow_applies_to_non_secret_workspace_path() {
        let decision = decide(
            &policy(vec![rule("allow", "read", "/workspace/*", "allow")]),
            vec![resource(
                ActionKind::Read,
                "/workspace/public.txt",
                b"public",
            )],
        );
        assert_eq!(decision.effect, cookie_agent_protocol::Effect::Allow);
    }

    #[test]
    fn external_guard_requires_separate_approval_despite_read_wildcard() {
        let decision = decide(
            &policy(vec![rule("read-all", "read", "*", "allow")]),
            vec![
                resource(ActionKind::Read, "/etc/passwd", b"passwd"),
                resource(ActionKind::ExternalDirectory, "/etc/passwd", b"external"),
            ],
        );
        assert_eq!(
            decision.evaluations[0].effect,
            cookie_agent_protocol::Effect::Allow
        );
        assert_eq!(
            decision.evaluations[1].effect,
            cookie_agent_protocol::Effect::Ask
        );
        assert_eq!(decision.effect, cookie_agent_protocol::Effect::Ask);
    }

    #[test]
    fn explicit_external_rule_can_allow_external_read() {
        let decision = decide(
            &policy(vec![
                rule("read-all", "read", "*", "allow"),
                rule("external-etc", "external_directory", "/etc/*", "allow"),
            ]),
            vec![
                resource(ActionKind::Read, "/etc/passwd", b"passwd"),
                resource(ActionKind::ExternalDirectory, "/etc/passwd", b"external"),
            ],
        );
        assert_eq!(decision.effect, cookie_agent_protocol::Effect::Allow);
    }
}
