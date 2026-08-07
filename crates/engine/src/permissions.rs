//! Permission evaluation over immutable protocol-v7 prepared-operation manifests.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_config::simple_wildcard_match;
use cookie_agent_protocol::{
    AgentSnapshot, ApprovalEvaluation, DecisionTrace, MatchedPermissionRule, OperationFingerprint,
    PermissionAction, PermissionEffect, PreparedOperationIdentity, SafeCode, SessionId,
    TreeApprovalGrant, TreeApprovalGrantId,
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
    pub fn action_for_tool(tool: &str) -> Result<PermissionAction, PermissionError> {
        match tool {
            "read" => Ok(PermissionAction::Read),
            "write" | "edit" => Ok(PermissionAction::Write),
            "bash" => Ok(PermissionAction::Bash),
            "grep" => Ok(PermissionAction::Grep),
            "glob" => Ok(PermissionAction::Glob),
            "delegate" => Ok(PermissionAction::Delegate),
            "external_directory" => Ok(PermissionAction::ExternalDirectory),
            other => Err(PermissionError::UnknownAction(other.into())),
        }
    }

    #[must_use]
    pub fn decide_operation(
        &self,
        policy: &AgentSnapshot,
        operation: &PreparedOperationIdentity,
        policy_labels: &[String],
        workspace: &Path,
    ) -> PermissionDecision {
        assert_eq!(operation.resources().len(), policy_labels.len());
        let evaluations = operation
            .resources()
            .iter()
            .zip(policy_labels)
            .map(|(resource, normalized)| {
                let candidates = matching_rules(policy, resource.capability, normalized, workspace);
                let (effect, reason) =
                    effective_permission(policy, resource.capability, normalized, workspace);
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
        let effect = if evaluations
            .iter()
            .any(|item| item.effect == PermissionEffect::Deny)
        {
            PermissionEffect::Deny
        } else if evaluations
            .iter()
            .any(|item| item.effect == PermissionEffect::Ask)
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
    pub fn tool_visible(policy: &AgentSnapshot, tool: &str) -> bool {
        let Ok(action) = Self::action_for_tool(tool) else {
            return true;
        };
        let Some(deny) = policy
            .permissions
            .iter()
            .find(|rule| rule.action == action && rule.resource.as_str() == "*")
        else {
            return true;
        };
        deny.effect != PermissionEffect::Deny
            || policy.permissions.iter().any(|rule| {
                rule.action == action
                    && rule.resource.as_str() != "*"
                    && rule.effect != PermissionEffect::Deny
            })
    }
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
        .collect()
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AgentDocumentSource, AgentId, AgentMode, AgentSchemaVersion, AgentSnapshot,
        ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, PermissionAction,
        PermissionEffect, PermissionRule, PreparedApprovalResource, PreparedBindingLifetime,
        PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
        PreparedResourceIdentity, SafeCode, Sha256Digest, WildcardPattern,
    };

    use super::PermissionPipeline;

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
            tools: Vec::new(),
            permissions: rules,
            delegation: None,
            fallback_chain: Vec::new(),
            selected_suffix_start: 0,
        }
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
            source: if action == PermissionAction::ExternalDirectory {
                ApprovalResourceSource::ExternalDirectoryGuard
            } else {
                ApprovalResourceSource::PrimaryOperation
            },
        }
    }

    fn operation(resources: Vec<PreparedApprovalResource>) -> PreparedOperationIdentity {
        let mut capabilities = vec![ApprovalCapability {
            action: PermissionAction::Read,
            operation: PreparedCapabilityOperation::new("read:read").expect("operation"),
        }];
        if resources
            .iter()
            .any(|resource| resource.capability == PermissionAction::ExternalDirectory)
        {
            capabilities.push(ApprovalCapability {
                action: PermissionAction::ExternalDirectory,
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
        policy: &AgentSnapshot,
        resources: Vec<PreparedApprovalResource>,
    ) -> super::PermissionDecision {
        let labels = resources
            .iter()
            .map(|resource| match &resource.boundary {
                ApprovalBoundary::CommandPrefix { prefix } => prefix.clone(),
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

    #[test]
    fn exact_rule_matches_normalized_label_not_opaque_identity() {
        let decision = decide(
            &policy(vec![rule(
                "exact",
                PermissionAction::Read,
                "/workspace/a.txt",
                PermissionEffect::Allow,
            )]),
            vec![resource(
                PermissionAction::Read,
                "/workspace/a.txt",
                b"file",
            )],
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
            vec![resource(
                PermissionAction::Read,
                "/workspace/secret.txt",
                b"secret",
            )],
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
            vec![resource(
                PermissionAction::Read,
                "nested/.env.local",
                b"secret",
            )],
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
            vec![resource(
                PermissionAction::Read,
                "/workspace/public.txt",
                b"public",
            )],
        );
        assert_eq!(decision.effect, PermissionEffect::Allow);
        assert_eq!(decision.evaluations[0].trace.candidates.len(), 2);
        assert_eq!(
            decision.evaluations[0].trace.candidates[1].source_layer,
            SafeCode::new("agent_document").expect("safe code")
        );
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
            vec![resource(
                PermissionAction::Read,
                "/workspace/public.txt",
                b"public",
            )],
        );
        assert_eq!(decision.effect, PermissionEffect::Allow);
    }

    #[test]
    fn external_guard_requires_separate_approval_despite_read_wildcard() {
        let decision = decide(
            &policy(vec![rule(
                "read-all",
                PermissionAction::Read,
                "*",
                PermissionEffect::Allow,
            )]),
            vec![
                resource(PermissionAction::Read, "/etc/passwd", b"passwd"),
                resource(
                    PermissionAction::ExternalDirectory,
                    "/etc/passwd",
                    b"external",
                ),
            ],
        );
        assert_eq!(decision.evaluations[0].effect, PermissionEffect::Allow);
        assert_eq!(decision.evaluations[1].effect, PermissionEffect::Ask);
        assert_eq!(decision.effect, PermissionEffect::Ask);
    }

    #[test]
    fn explicit_external_rule_can_allow_external_read() {
        let decision = decide(
            &policy(vec![
                rule(
                    "read-all",
                    PermissionAction::Read,
                    "*",
                    PermissionEffect::Allow,
                ),
                rule(
                    "external-etc",
                    PermissionAction::ExternalDirectory,
                    "/etc/*",
                    PermissionEffect::Allow,
                ),
            ]),
            vec![
                resource(PermissionAction::Read, "/etc/passwd", b"passwd"),
                resource(
                    PermissionAction::ExternalDirectory,
                    "/etc/passwd",
                    b"external",
                ),
            ],
        );
        assert_eq!(decision.effect, PermissionEffect::Allow);
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
                vec![resource(PermissionAction::Read, path, path.as_bytes())],
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
                vec![resource(PermissionAction::Read, ".env", b"env")],
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
            vec![resource(
                PermissionAction::Read,
                "nested/.env.local",
                b"env",
            )],
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
            vec![resource(
                PermissionAction::Read,
                "nested/.env.production.example",
                b"example",
            )],
        );
        assert_eq!(decision.effect, PermissionEffect::Allow);
    }

    #[test]
    fn tool_visibility_requires_effective_unconditional_deny() {
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

    #[test]
    fn workspace_dir_external_rule_does_not_match_outside_boundary() {
        let policy = policy(vec![rule(
            "workspace-external",
            PermissionAction::ExternalDirectory,
            "${workspace_dir}/*",
            PermissionEffect::Allow,
        )]);
        assert_eq!(
            super::effective_permission(
                &policy,
                PermissionAction::ExternalDirectory,
                "/outside/*",
                std::path::Path::new("/workspace"),
            )
            .0,
            PermissionEffect::Ask
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
