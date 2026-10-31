//! Permission evaluation over immutable prepared-operation manifests.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
};

use cookie_agent_config::simple_wildcard_match;
use cookie_agent_protocol::paths::{self, PathStyle};
use cookie_agent_protocol::{
    AgentId, AgentMode, AgentSnapshot, ApprovalEvaluation, DecisionTrace,
    EffectivePermissionAction, EffectivePermissionRule, EventPayload, MatchedPermissionRule,
    OperationFingerprint, PermissionAction, PermissionEffect, PermissionRule, PermissionRuleSource,
    PreparedOperationIdentity, RuntimeId, SafeCode, SessionId, SessionPermissionOverlay,
    TreeApprovalGrant, TreeApprovalGrantId, WildcardPattern,
};
use thiserror::Error;

/// Display stand-in for a permission resource the tool did not scope.
pub const UNSCOPED_PERMISSION_RESOURCE_DISPLAY: &str = "<permission-name-only>";

/// Where a permission is evaluated: the runtime that offers the tool, that
/// runtime's workspace, and the home directory it reported.
///
/// A rule with a runtime selector (`bash@labpc`) only applies when `runtime`
/// is that runtime ID; the base layer (`runtime: None`) only sees unqualified
/// rules. `${workspace_dir}` and relative resources resolve against
/// `workspace`; without one they never match. A pattern `~` or `~/…`
/// expands against `home`; without one it stays literal, so it matches no
/// absolute path. A path resource is read as `platform`, the operating
/// system the runtime reported, reads it; without one, a path the platforms
/// read differently is decided under both readings, the strictest winning.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PermissionScope<'a> {
    pub runtime: Option<&'a str>,
    pub workspace: Option<&'a Path>,
    pub home: Option<&'a str>,
    pub platform: Option<&'a str>,
}

impl<'a> PermissionScope<'a> {
    /// The base layer: no runtime, no workspace, no home.
    pub const BASE: Self = Self {
        runtime: None,
        workspace: None,
        home: None,
        platform: None,
    };

    /// The scope of a tool from `origin`.
    #[must_use]
    pub fn of(origin: &'a cookie_agent_runtime_api::ToolOrigin) -> Self {
        let runtime = origin.runtime();
        Self {
            runtime: runtime.map(cookie_agent_runtime_api::RuntimeInfo::id),
            workspace: origin.workspace_root(),
            home: runtime.and_then(cookie_agent_runtime_api::RuntimeInfo::home),
            platform: runtime
                .and_then(cookie_agent_runtime_api::RuntimeInfo::workspace)
                .and_then(cookie_agent_runtime_api::WorkspaceContext::platform),
        }
    }

    /// The scope of the runtime with ID `runtime` working in `workspace`,
    /// which reported no home or platform.
    #[must_use]
    pub const fn runtime(runtime: &'a str, workspace: Option<&'a Path>) -> Self {
        Self {
            runtime: Some(runtime),
            workspace,
            home: None,
            platform: None,
        }
    }

    /// This scope on a runtime that reported `home`.
    #[must_use]
    pub const fn with_home(self, home: Option<&'a str>) -> Self {
        Self { home, ..self }
    }

    /// This scope on a runtime that reported `platform`.
    #[must_use]
    pub const fn with_platform(self, platform: Option<&'a str>) -> Self {
        Self { platform, ..self }
    }

    /// The styles a path resource `resource` of `action` is read in: the
    /// platform's; without one, both when they read it differently.
    fn styles(self, action: &PermissionAction, resource: &str) -> &'static [PathStyle] {
        const POSIX: &[PathStyle] = &[PathStyle::Posix];
        const WINDOWS: &[PathStyle] = &[PathStyle::Windows];
        const BOTH: &[PathStyle] = &[PathStyle::Posix, PathStyle::Windows];
        match self.platform.map(PathStyle::of_platform) {
            Some(PathStyle::Posix) => POSIX,
            Some(PathStyle::Windows) => WINDOWS,
            // Read alike in both, a drive or a POSIX root as one.
            None if !path_resource(action) || !PathStyle::ambiguous(resource) => WINDOWS,
            None => BOTH,
        }
    }
}

/// Only unqualified rules, resolved against `workspace`.
impl<'a> From<Option<&'a Path>> for PermissionScope<'a> {
    fn from(workspace: Option<&'a Path>) -> Self {
        Self {
            runtime: None,
            workspace,
            home: None,
            platform: None,
        }
    }
}

/// Whether the resources of `action` are paths, labelled before matching:
/// `read` and `write`, and `attach`'s workspace.
fn path_resource(action: &PermissionAction) -> bool {
    action.is_path() || *action == PermissionAction::ATTACH
}

/// The stricter of two decisions: deny over ask over allow.
fn strictest(
    left: (PermissionEffect, String),
    right: (PermissionEffect, String),
) -> (PermissionEffect, String) {
    if effect_rank(right.0) > effect_rank(left.0) {
        right
    } else {
        left
    }
}

/// Visibility is action-level opt-in, not a resource authorization decision:
/// whether the merged list of `rules` (a frozen policy) and `overlay` offers
/// `action` to a tool of the runtime with ID `runtime` (`None` for the base
/// layer), as the run's merged rule list decides it.
#[must_use]
pub fn tool_visible(
    rules: &[PermissionRule],
    overlay: Option<&SessionPermissionOverlay>,
    action: &PermissionAction,
    runtime: Option<&str>,
) -> bool {
    MergedRules::new(rules, overlay, None).offers(action, runtime)
}

/// One rule of a run's [merged list](MergedRules) and the level it comes
/// from.
#[derive(Clone, Copy, Debug)]
pub struct MergedRule<'a> {
    pub rule: &'a PermissionRule,
    pub source: PermissionRuleSource,
}

/// A run's permissions as one rule list
/// (`docs/site/specs/permission-consolidation.md` section 3): the frozen
/// policy's rules (the agent document's, and the defaults of each action it
/// names no key for), then the session overlay's. A check no rule matches
/// asks. An overlay rule replaces
/// every policy rule with its identity (action, runtime selector, and
/// pattern, a leading `~` expanded against the scope's home as matching
/// expands it); every other overlay rule is added. Policy rules carry the
/// source `agent_document` until [`label_defaults`](Self::label_defaults)
/// tells the defaults apart; overlay rules carry `session_overlay`.
///
/// Every permission decision reads this list: which tools are offered
/// ([`offers`](Self::offers)), each call ([`decide`](Self::decide),
/// [`decide_name_only`](Self::decide_name_only)), the memory checks, and the
/// `/permissions` view. It is computed from the frozen policy and the
/// current overlay at each decision, so an overlay change applies to the next
/// one without refreezing the run.
#[derive(Clone, Debug, Default)]
pub struct MergedRules<'a> {
    rules: Vec<MergedRule<'a>>,
}

impl<'a> MergedRules<'a> {
    /// The merged list of the frozen `policy` and `overlay`, for a runtime
    /// that reported `home` (`None` compares `~` patterns as written).
    pub fn new(
        policy: &'a [PermissionRule],
        overlay: Option<&'a SessionPermissionOverlay>,
        home: Option<&str>,
    ) -> Self {
        let session = overlay.map_or(&[][..], |overlay| overlay.rules.as_slice());
        // Each overlay identity is expanded once, and a policy rule's
        // pattern only when an overlay rule shares its action and runtime
        // selector.
        let replaced = session
            .iter()
            .map(|rule| identity(rule, home))
            .collect::<Vec<_>>();
        let rules = policy
            .iter()
            .filter(|rule| {
                let mut same_target = replaced
                    .iter()
                    .filter(|(action, runtime, _)| {
                        **action == rule.action && *runtime == rule.runtime.as_ref()
                    })
                    .peekable();
                if same_target.peek().is_none() {
                    return true;
                }
                let pattern = rule_pattern(rule, home);
                !same_target.any(|(_, _, replacing)| *replacing == pattern)
            })
            .map(|rule| MergedRule {
                rule,
                source: PermissionRuleSource::AgentDocument,
            })
            .chain(session.iter().map(|rule| MergedRule {
                rule,
                source: PermissionRuleSource::SessionOverlay,
            }))
            .collect();
        Self { rules }
    }

    /// The merged list of `policy`'s frozen rules and `overlay`.
    pub fn of(
        policy: &'a AgentSnapshot,
        overlay: Option<&'a SessionPermissionOverlay>,
        home: Option<&str>,
    ) -> Self {
        Self::new(&policy.permissions, overlay, home)
    }

    /// The rules in order: policy rules, then session rules.
    pub fn rules(&self) -> &[MergedRule<'a>] {
        &self.rules
    }

    /// This list with the policy rules of each action whose rules are
    /// exactly that action's defaults for a root or `delegated` run of
    /// `policy`'s agent labelled `default`.
    pub fn label_defaults(mut self, policy: &AgentSnapshot, delegated: bool) -> Self {
        let defaults = run_default_rules_with_delegate(policy.mode, delegated, &policy.permissions);
        let mut from_defaults = HashMap::<&PermissionAction, bool>::new();
        for merged in &mut self.rules {
            if merged.source != PermissionRuleSource::AgentDocument {
                continue;
            }
            let action = &merged.rule.action;
            let default = *from_defaults.entry(action).or_insert_with(|| {
                let of_action = |rule: &&PermissionRule| rule.action == *action;
                let default = defaults.iter().filter(of_action).collect::<Vec<_>>();
                let rules = policy
                    .permissions
                    .iter()
                    .filter(of_action)
                    .collect::<Vec<_>>();
                !default.is_empty() && expanded_from_defaults(&default, &rules, &policy.agent)
            });
            if default {
                merged.source = PermissionRuleSource::Default;
            }
        }
        self
    }

    /// Whether a tool checked under `action` on the runtime with ID
    /// `runtime` is offered: some rule for `action` applying to that runtime
    /// allows or asks, or none of them covers every resource (`*`), so what
    /// they leave asks. Only a `*` deny with no allow or ask beside it hides
    /// the tool.
    pub fn offers(&self, action: &PermissionAction, runtime: Option<&str>) -> bool {
        let applying = || {
            self.rules.iter().filter(move |merged| {
                merged.rule.action == *action && merged.rule.applies_to(runtime)
            })
        };
        applying().any(|merged| {
            matches!(
                merged.rule.effect,
                PermissionEffect::Allow | PermissionEffect::Ask
            )
        }) || !applying().any(|merged| merged.rule.resource.as_str() == "*")
    }

    /// What a check that no rule matches decides: ask, for every action,
    /// a plugin's or a runtime's included.
    fn unmatched() -> (PermissionEffect, String) {
        (
            PermissionEffect::Ask,
            "no matching rule; ask by default".into(),
        )
    }

    /// The effect of `action` on `resource` in `scope`: the most specific
    /// matching rule's (`specificity`), a later rule winning a tie, else
    /// ask. The list must have been built with `scope.home`.
    pub fn decide(
        &self,
        action: &PermissionAction,
        resource: &str,
        scope: PermissionScope<'_>,
    ) -> (PermissionEffect, String) {
        let mut styles = scope.styles(action, resource).iter();
        let first = self.decide_in(action, resource, scope, *styles.next().expect("a style"));
        styles.fold(first, |decided, style| {
            strictest(decided, self.decide_in(action, resource, scope, *style))
        })
    }

    /// [`Self::decide`] with path resources read in `style`.
    fn decide_in(
        &self,
        action: &PermissionAction,
        resource: &str,
        scope: PermissionScope<'_>,
        style: PathStyle,
    ) -> (PermissionEffect, String) {
        self.rules
            .iter()
            .enumerate()
            .filter(|(_, merged)| rule_matches_in(merged.rule, action, resource, scope, style))
            .max_by_key(|(index, merged)| specificity(merged.rule, *index, scope.home))
            .map_or_else(
                Self::unmatched,
                |(_, merged)| {
                    (
                        merged.rule.effect,
                        "most-specific matching pattern: more literal characters, then fewer wildcards, then a runtime selector, then later declaration".into(),
                    )
                },
            )
    }

    /// The effect of `action` checked by name only in `scope`: its `*`
    /// rules', a runtime-selected one over a bare one, then the later one;
    /// ask without one.
    pub fn decide_name_only(
        &self,
        action: &PermissionAction,
        scope: PermissionScope<'_>,
    ) -> (PermissionEffect, String) {
        self.rules
            .iter()
            .enumerate()
            .filter(|(_, merged)| name_only_rule(merged.rule, action, scope))
            .max_by_key(|(index, merged)| (merged.rule.runtime.is_some(), *index))
            .map_or_else(Self::unmatched, |(_, merged)| {
                (
                    merged.rule.effect,
                    "bare or `*` permission rule applies to the permission-name-only check".into(),
                )
            })
    }

    /// The rules that `keep` admits, in order, as a decision trace lists
    /// them.
    fn traced(&self, keep: impl Fn(&PermissionRule) -> bool) -> Vec<MatchedPermissionRule> {
        self.rules
            .iter()
            .filter(|merged| keep(merged.rule))
            .map(|MergedRule { rule, source }| MatchedPermissionRule {
                source_layer: SafeCode::new(match source {
                    PermissionRuleSource::SessionOverlay => "session_overlay",
                    PermissionRuleSource::AgentDocument => "agent_document",
                    PermissionRuleSource::Default => "default",
                })
                .expect("static safe code"),
                action: rule.action.clone(),
                runtime: rule.runtime.clone(),
                resource: rule.resource.clone(),
                effect: rule.effect,
            })
            .collect()
    }
}

/// `rule`'s identity in the merged list: its action and runtime selector,
/// and its pattern once a leading `~` is expanded against `home`. Two rules
/// with one identity replace each other.
fn identity<'rule>(
    rule: &'rule PermissionRule,
    home: Option<&str>,
) -> (
    &'rule PermissionAction,
    Option<&'rule RuntimeId>,
    Cow<'rule, str>,
) {
    (
        &rule.action,
        rule.runtime.as_ref(),
        rule_pattern(rule, home),
    )
}

/// Whether `rule` governs a name-only check of `action` in `scope`.
fn name_only_rule(
    rule: &PermissionRule,
    action: &PermissionAction,
    scope: PermissionScope<'_>,
) -> bool {
    rule.action == *action && rule.applies_to(scope.runtime) && rule.resource.as_str() == "*"
}

/// Whether some resource matches both `left` and `right`, wildcard
/// patterns of `*` (any characters) and `?` (one character). Decided
/// exactly, without any resource having to exist; a trailing ` *`, which
/// also matches the bare prefix, counts both ways.
pub fn patterns_intersect(left: &str, right: &str) -> bool {
    let spellings = |pattern: &str| {
        let mut spellings = vec![pattern.chars().collect::<Vec<_>>()];
        if let Some(prefix) = pattern.strip_suffix(" *") {
            spellings.push(prefix.chars().collect());
        }
        spellings
    };
    spellings(left).iter().any(|left| {
        spellings(right)
            .iter()
            .any(|right| globs_intersect(left, right))
    })
}

/// Whether one string matches both glob patterns: a search over pairs of
/// positions, where a `*` may match nothing or swallow what the other
/// pattern produces next.
fn globs_intersect(left: &[char], right: &[char]) -> bool {
    let mut seen = HashSet::new();
    let mut pending = vec![(0usize, 0usize)];
    while let Some((i, j)) = pending.pop() {
        if !seen.insert((i, j)) {
            continue;
        }
        let (a, b) = (left.get(i), right.get(j));
        match (a, b) {
            (None, None) => return true,
            (Some('*'), _) | (_, Some('*')) => {
                if a == Some(&'*') {
                    pending.push((i + 1, j));
                    if b.is_some() {
                        pending.push((i, j + 1));
                    }
                }
                if b == Some(&'*') {
                    pending.push((i, j + 1));
                    if a.is_some() {
                        pending.push((i + 1, j));
                    }
                }
            }
            (Some(a), Some(b)) if *a == '?' || *b == '?' || a == b => pending.push((i + 1, j + 1)),
            _ => {}
        }
    }
    false
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
        let merged = MergedRules::of(policy, overlay, scope.home);
        let evaluations = operation
            .resources()
            .iter()
            .zip(policy_labels)
            .map(|(resource, normalized)| {
                let action = &resource.capability;
                let (candidates, (effect, reason)) = match normalized {
                    Some(normalized) => (
                        merged.traced(|rule| rule_matches(rule, action, normalized, scope)),
                        merged.decide(action, normalized, scope),
                    ),
                    None => (
                        merged.traced(|rule| name_only_rule(rule, action, scope)),
                        merged.decide_name_only(action, scope),
                    ),
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
                        // A grant allows only what it covers in every reading.
                        scope
                            .styles(&resource.capability, normalized)
                            .iter()
                            .all(|style| {
                                permission_pattern_matches(
                                    &expand_home_pattern(rule.resource.as_str(), scope.home),
                                    &resource.capability,
                                    normalized,
                                    scope.workspace,
                                    *style,
                                )
                            })
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
    /// (`None` for the base layer) is visible: whether the merged list of
    /// `policy` and `overlay` offers it. A tool that
    /// needs no permission has no name to ask about and is always visible.
    #[must_use]
    pub fn tool_visible_with_overlay(
        policy: &AgentSnapshot,
        overlay: Option<&SessionPermissionOverlay>,
        permission_name: &str,
        runtime: Option<&str>,
    ) -> bool {
        Self::tool_visible_with_grants(
            policy,
            overlay,
            None,
            permission_name,
            PermissionScope {
                runtime,
                workspace: None,
                home: None,
                platform: None,
            },
        )
    }

    /// Whether the tool its provider knows as `tool_name`, checked under
    /// `permission_name` in `scope`, is offered. A tool whose action's
    /// resource is the tool's name
    /// ([`PermissionAction::is_tool_name`]) is offered when the merged list
    /// of `policy` and `overlay` does not deny that name: a matching `allow`
    /// or `ask` offers it, and so does no matching rule, which asks; a
    /// matching `deny` hides it. A grant
    /// cannot offer it otherwise, since it never overrides a deny. Any other
    /// tool is offered as [`Self::tool_visible_with_grants`] decides.
    #[must_use]
    pub fn tool_offered<'a>(
        policy: &AgentSnapshot,
        overlay: Option<&SessionPermissionOverlay>,
        grants: Option<&SessionPermissionOverlay>,
        permission_name: &str,
        tool_name: &str,
        scope: impl Into<PermissionScope<'a>>,
    ) -> bool {
        let scope = scope.into();
        Self::tool_offered_by(
            &MergedRules::of(policy, overlay, scope.home),
            grants,
            permission_name,
            tool_name,
            scope,
        )
    }

    /// [`Self::tool_offered`] against a merged list already built for
    /// `scope.home`, for callers that ask about many tools.
    pub fn tool_offered_by(
        merged: &MergedRules<'_>,
        grants: Option<&SessionPermissionOverlay>,
        permission_name: &str,
        tool_name: &str,
        scope: PermissionScope<'_>,
    ) -> bool {
        match Self::action_for_permission_name(permission_name) {
            Ok(action) if action.is_tool_name() => {
                merged.decide(&action, tool_name, scope).0 != PermissionEffect::Deny
            }
            _ => Self::tool_visible_by(merged, grants, permission_name, scope),
        }
    }

    /// Whether a tool with `permission_name` in `scope` is visible: the
    /// merged list of `policy` and `overlay` offers it, or one of `grants`
    /// allows a resource of it that the merged list does not deny.
    #[must_use]
    pub fn tool_visible_with_grants<'a>(
        policy: &AgentSnapshot,
        overlay: Option<&SessionPermissionOverlay>,
        grants: Option<&SessionPermissionOverlay>,
        permission_name: &str,
        scope: impl Into<PermissionScope<'a>>,
    ) -> bool {
        let scope = scope.into();
        Self::tool_visible_by(
            &MergedRules::of(policy, overlay, scope.home),
            grants,
            permission_name,
            scope,
        )
    }

    /// [`Self::tool_visible_with_grants`] against a merged list already
    /// built for `scope.home`.
    pub fn tool_visible_by(
        merged: &MergedRules<'_>,
        grants: Option<&SessionPermissionOverlay>,
        permission_name: &str,
        scope: PermissionScope<'_>,
    ) -> bool {
        let Ok(action) = Self::action_for_permission_name(permission_name) else {
            return false;
        };
        if merged.offers(&action, scope.runtime) {
            return true;
        }
        grants.is_some_and(|grants| {
            grants.rules.iter().any(|grant| {
                grant.action == action
                    && grant.effect == PermissionEffect::Allow
                    && grant.applies_to(scope.runtime)
                    && merged
                        .decide(
                            &action,
                            &expand_home_pattern(grant.resource.as_str(), scope.home),
                            scope,
                        )
                        .0
                        != PermissionEffect::Deny
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

/// The effect of `action` on `resource` in `scope` under `policy`'s frozen
/// rules alone.
#[cfg(any(test, feature = "test-support"))]
pub fn effective_permission<'a>(
    policy: &AgentSnapshot,
    action: &PermissionAction,
    resource: &str,
    scope: impl Into<PermissionScope<'a>>,
) -> (PermissionEffect, String) {
    effective_permission_with_overlay(policy, None, action, resource, scope)
}

/// The effect of `action` on `resource` in `scope` under the merged list of
/// `policy` and `overlay` ([`MergedRules::decide`]).
pub fn effective_permission_with_overlay<'a>(
    policy: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
    action: &PermissionAction,
    resource: &str,
    scope: impl Into<PermissionScope<'a>>,
) -> (PermissionEffect, String) {
    let scope = scope.into();
    MergedRules::of(policy, overlay, scope.home).decide(action, resource, scope)
}

/// Whether `rule` governs `resource` of `action` in `scope`, in any of the
/// readings the scope gives it.
fn rule_matches(
    rule: &PermissionRule,
    action: &PermissionAction,
    resource: &str,
    scope: PermissionScope<'_>,
) -> bool {
    scope
        .styles(action, resource)
        .iter()
        .any(|style| rule_matches_in(rule, action, resource, scope, *style))
}

/// Whether `rule` governs `resource` of `action` in `scope`, a path read in
/// `style`.
fn rule_matches_in(
    rule: &PermissionRule,
    action: &PermissionAction,
    resource: &str,
    scope: PermissionScope<'_>,
    style: PathStyle,
) -> bool {
    rule.action == *action
        && rule.applies_to(scope.runtime)
        && permission_pattern_matches(
            &rule_pattern(rule, scope.home),
            action,
            resource,
            scope.workspace,
            style,
        )
}

/// The pattern `rule` matches with on a runtime with `home`: its resource,
/// with a leading `~` expanded against `home` ([`expand_home_pattern`]).
fn rule_pattern<'rule>(rule: &'rule PermissionRule, home: Option<&str>) -> Cow<'rule, str> {
    expand_home_pattern(rule.resource.as_str(), home)
}

/// `pattern` with a leading `~` (the whole pattern, or `~/` before the
/// rest) replaced by `home`, the home directory the tool's runtime
/// reported, spelled with `/` as path labels are. Without a home, and for
/// any other pattern (`~user/…` included), `pattern` as it is: a literal `~`
/// then matches no absolute path. Pure and allocation-free when nothing
/// expands.
pub fn expand_home_pattern<'p>(pattern: &'p str, home: Option<&str>) -> Cow<'p, str> {
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

/// The rules the daemon supplies for an agent of `mode`
/// (`docs/site/specs/runtime-delegation.md` section 4 and
/// `docs/site/specs/runtimes.md` section 7.1), each applying only to an
/// action its document names no key for. Every agent but an internal one
/// reads every file, edits files in its workspace and asks for any other
/// path, fetches the web, uses every skill, calls MCP tools after asking, and
/// delegates to nobody; `primary` and `all` agents also list runtimes.
/// Internal agents, which call no tools, are denied every action. Memory's defaults
/// depend on whether the run is delegated, so they are
/// separate (`memory_default_rules`), and `message` follows `delegate`
/// ([`message_default_rules`]).
pub fn mode_default_rules(mode: AgentMode) -> Vec<PermissionRule> {
    let working: &[(PermissionAction, &str, PermissionEffect)] = &[
        (PermissionAction::READ, "*", PermissionEffect::Allow),
        (
            PermissionAction::WRITE,
            "${workspace_dir}/*",
            PermissionEffect::Allow,
        ),
        (PermissionAction::WRITE, "*", PermissionEffect::Ask),
        (PermissionAction::WEBFETCH, "*", PermissionEffect::Allow),
        (PermissionAction::SKILL, "*", PermissionEffect::Allow),
        (PermissionAction::MCP, "*", PermissionEffect::Ask),
        (PermissionAction::DELEGATE, "*", PermissionEffect::Deny),
    ];
    let listing: &[(PermissionAction, &str, PermissionEffect)] = match mode {
        AgentMode::Primary | AgentMode::All => &[(
            PermissionAction::LIST_RUNTIMES,
            "*",
            PermissionEffect::Allow,
        )],
        AgentMode::Subagent | AgentMode::Internal => &[],
    };
    if mode == AgentMode::Internal {
        return PermissionAction::BUILT_IN
            .into_iter()
            .map(|action| {
                PermissionRule::new(
                    action,
                    WildcardPattern::new("*").expect("static default pattern"),
                    PermissionEffect::Deny,
                )
            })
            .collect();
    }
    working
        .iter()
        .chain(listing)
        .map(|(action, pattern, effect)| {
            PermissionRule::new(
                action.clone(),
                WildcardPattern::new(*pattern).expect("static default pattern"),
                *effect,
            )
        })
        .collect()
}

/// The `message` rule a run of an agent of `mode` gets when its document
/// names no `message` key: a delegated run messages anyone in its tree; a
/// root run follows its `delegate` rules (`delegate`, the run's own or the
/// default), allowed when one of them allows, asking when one asks, and
/// denied when it may delegate to nobody, as it then has no subagent to
/// message. Internal agents get nothing.
pub fn message_default_rules<'r>(
    mode: AgentMode,
    delegated: bool,
    delegate: impl IntoIterator<Item = &'r PermissionRule>,
) -> Vec<PermissionRule> {
    if mode == AgentMode::Internal {
        return Vec::new();
    }
    let effect = if delegated {
        PermissionEffect::Allow
    } else {
        delegate
            .into_iter()
            .filter(|rule| rule.action == PermissionAction::DELEGATE)
            .map(|rule| rule.effect)
            .min_by_key(|effect| effect_rank(*effect))
            .unwrap_or(PermissionEffect::Deny)
    };
    vec![PermissionRule::new(
        PermissionAction::MESSAGE,
        WildcardPattern::new("*").expect("static default pattern"),
        effect,
    )]
}

/// The rules a run of an agent of `mode` gets for each memory action its
/// agent document names no key for (`docs/site/specs/memory.md` section
/// 16.9.11): a root run reads every memory and writes the memory related to
/// it, and a `delegated` run reads and maintains only its own agent file.
/// Internal agents get nothing.
fn memory_default_rules(mode: AgentMode, delegated: bool) -> Vec<PermissionRule> {
    let defaults: &[(PermissionAction, &str)] = match (mode, delegated) {
        (AgentMode::Internal, _) => &[],
        (_, true) => &[
            (PermissionAction::MEMORY, "agent://${agent_id}"),
            (PermissionAction::MEMORY_WRITE, "agent://${agent_id}"),
        ],
        (_, false) => &[
            (PermissionAction::MEMORY, "*"),
            (PermissionAction::MEMORY_WRITE, "user://*"),
            (PermissionAction::MEMORY_WRITE, "agent://${agent_id}"),
            (
                PermissionAction::MEMORY_WRITE,
                "machine://${session_machine}/*",
            ),
            (
                PermissionAction::MEMORY_WRITE,
                "project://${session_project}/*",
            ),
        ],
    };
    defaults
        .iter()
        .map(|(action, pattern)| {
            PermissionRule::new(
                action.clone(),
                WildcardPattern::new(*pattern).expect("static default pattern"),
                PermissionEffect::Allow,
            )
        })
        .collect()
}

/// The rules a run of an agent of `mode` gets for `cron` when its agent
/// document names no `cron` key (`docs/site/specs/cron.md` section 7): a
/// root run of a non-internal agent manages every job; a `delegated` run
/// and an internal agent get nothing, so the cron tools are hidden unless
/// granted.
pub fn cron_default_rules(mode: AgentMode, delegated: bool) -> Vec<PermissionRule> {
    if delegated || mode == AgentMode::Internal {
        return Vec::new();
    }
    vec![PermissionRule::new(
        PermissionAction::CRON,
        WildcardPattern::new("*").expect("static default pattern"),
        PermissionEffect::Allow,
    )]
}

/// The `attach` default (`docs/site/specs/machines-on-demand.md` section
/// 3.3): a root run may attach and detach machines, and a delegated one,
/// or an internal agent, has no rule, so the tools are hidden.
pub fn attach_default_rules(mode: AgentMode, delegated: bool) -> Vec<PermissionRule> {
    if delegated || mode == AgentMode::Internal {
        return Vec::new();
    }
    vec![PermissionRule::new(
        PermissionAction::ATTACH,
        WildcardPattern::new("*").expect("static default pattern"),
        PermissionEffect::Allow,
    )]
}

/// Every rule the daemon supplies to a run of an agent of `mode`, root or
/// `delegated`, each for an action its document names no key for. Whatever
/// no rule matches asks ([`MergedRules`]).
pub fn run_default_rules(mode: AgentMode, delegated: bool) -> Vec<PermissionRule> {
    let mut rules = run_defaults_but_message(mode, delegated);
    let message = message_default_rules(mode, delegated, &rules);
    rules.extend(message);
    rules
}

/// [`run_default_rules`] for a run whose `delegate` rules are `delegate`:
/// the `message` default follows them.
pub fn run_default_rules_with_delegate<'r>(
    mode: AgentMode,
    delegated: bool,
    delegate: impl IntoIterator<Item = &'r PermissionRule>,
) -> Vec<PermissionRule> {
    let mut rules = run_defaults_but_message(mode, delegated);
    rules.extend(message_default_rules(mode, delegated, delegate));
    rules
}

fn run_defaults_but_message(mode: AgentMode, delegated: bool) -> Vec<PermissionRule> {
    mode_default_rules(mode)
        .into_iter()
        .chain(memory_default_rules(mode, delegated))
        .chain(cron_default_rules(mode, delegated))
        .chain(attach_default_rules(mode, delegated))
        .collect()
}

/// What the memory rule variables stand for in one run
/// (`docs/site/specs/memory.md` section 16.9.9), besides `${agent_id}`,
/// which is the run's agent: the session's machines in order, and the name
/// of its project.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MemoryVariables {
    pub machines: Vec<String>,
    pub project: Option<String>,
}

/// `rule` with the memory variables of its pattern expanded for a run of
/// `agent`: one rule per session machine for `${session_machine}`, and none
/// when a variable stands for nothing (no project, or no machine), since
/// such a pattern matches no memory. Rules of other actions, and patterns
/// without a variable, are kept as they are.
fn expand_memory_variables(
    rule: PermissionRule,
    agent: &AgentId,
    variables: &MemoryVariables,
) -> Vec<PermissionRule> {
    let [agent_id, session_machine, session_project] = WildcardPattern::MEMORY_VARIABLES;
    let pattern = rule.resource.as_str();
    let memory_action =
        rule.action == PermissionAction::MEMORY || rule.action == PermissionAction::MEMORY_WRITE;
    if !memory_action || !pattern.contains("${") {
        return vec![rule];
    }
    let mut pattern = pattern.replace(agent_id, agent.as_str());
    if pattern.contains(session_project) {
        let Some(project) = &variables.project else {
            return Vec::new();
        };
        pattern = pattern.replace(session_project, project);
    }
    let patterns = if pattern.contains(session_machine) {
        variables
            .machines
            .iter()
            .map(|machine| pattern.replace(session_machine, machine))
            .collect()
    } else {
        vec![pattern]
    };
    patterns
        .into_iter()
        .filter_map(|pattern| WildcardPattern::new(pattern).ok())
        .map(|resource| PermissionRule {
            resource,
            ..rule.clone()
        })
        .collect()
}

/// The rules an agent document gives its runs: the document's own, then
/// the [mode defaults](mode_default_rules) of each action the document
/// names no key for, with or without a runtime selector, and the
/// memory defaults (`memory_default_rules`) and the [cron default](cron_default_rules)
/// for a root or `delegated` run
/// likewise. A declared key replaces its action's default entirely. The
/// memory rule variables are expanded with `variables` as the run's policy
/// is frozen, so frozen rules, which the wire carries, never hold one.
pub fn agent_permission_rules(
    document: &cookie_agent_config::AgentDocument,
    delegated: bool,
    variables: &MemoryVariables,
) -> Vec<PermissionRule> {
    agent_rules_with_sources(document, delegated)
        .into_iter()
        .map(|(rule, _)| rule)
        .flat_map(|rule| expand_memory_variables(rule, &document.id, variables))
        .collect()
}

/// The rules a root or `delegated` run of `document`'s agent starts with,
/// as [`agent_permission_rules`] gives them before the memory variables are
/// expanded, each with whether the daemon supplied it as a default.
pub fn agent_rules_with_sources(
    document: &cookie_agent_config::AgentDocument,
    delegated: bool,
) -> Vec<(PermissionRule, bool)> {
    let permissions = &document.frontmatter.permissions;
    let declared = |action: &PermissionAction| permissions.keys().any(|key| key.action == *action);
    let mut rules = permissions
        .iter()
        .flat_map(|(key, value)| value.rules(key))
        .map(|rule| (rule, false))
        .chain(
            run_defaults_but_message(document.frontmatter.mode, delegated)
                .into_iter()
                .filter(|rule| !declared(&rule.action))
                .map(|rule| (rule, true)),
        )
        .collect::<Vec<_>>();
    if !declared(&PermissionAction::MESSAGE) {
        let message = message_default_rules(
            document.frontmatter.mode,
            delegated,
            rules.iter().map(|(rule, _)| rule),
        );
        rules.extend(message.into_iter().map(|rule| (rule, true)));
    }
    rules
}

/// Whether `pattern` matches `resource` for a tool whose runtime has
/// `workspace`. A path resource ([`path_resource`]) is matched as its label
/// in `style` ([`paths::path_label`]), `.` and `..` collapsed, and made
/// absolute against the workspace when it leaves it. `${workspace_dir}`
/// patterns match the resource made absolute against the workspace. Without
/// a workspace (the base layer and the global runtime) nothing can be
/// resolved: `${workspace_dir}` patterns never match, and neither does any
/// pattern for a relative path resource of a path action (`read`, `write`).
fn permission_pattern_matches(
    pattern: &str,
    action: &PermissionAction,
    resource: &str,
    workspace: Option<&Path>,
    style: PathStyle,
) -> bool {
    let workspace_pattern =
        pattern.contains(cookie_agent_protocol::WildcardPattern::WORKSPACE_DIR_EXPRESSION);
    // A path is matched as the runtime labels it: `.` and `..` collapsed,
    // relative inside the workspace and absolute outside it.
    let resource = if path_resource(action) {
        let label = paths::path_label(resource, style);
        match workspace {
            Some(workspace) if label == ".." || label.starts_with("../") => {
                Cow::Owned(absolute_resource(workspace, &label, style))
            }
            _ => Cow::Owned(label),
        }
    } else {
        Cow::Borrowed(resource)
    };
    match workspace {
        Some(workspace) if workspace_pattern => simple_wildcard_match(
            &expand_workspace_pattern(pattern, workspace, style),
            &absolute_resource(workspace, &resource, style),
        ),
        Some(_) => simple_wildcard_match(pattern, &resource),
        None if workspace_pattern => false,
        None if action.is_path() && !paths::label_is_absolute(&resource, style) => false,
        None => simple_wildcard_match(pattern, &resource),
    }
}

fn expand_workspace_pattern(pattern: &str, workspace: &Path, style: PathStyle) -> String {
    let workspace = workspace_label(workspace, style);
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

/// `resource` as a label from a root in `style` ([`paths::path_label`]): as
/// it is when it names one, else read against `workspace`.
fn absolute_resource(workspace: &Path, resource: &str, style: PathStyle) -> String {
    let label = paths::path_label(resource, style);
    if paths::label_is_absolute(&label, style) {
        return label;
    }
    let workspace = workspace_label(workspace, style);
    let separator = if workspace.ends_with('/') { "" } else { "/" };
    paths::path_label(&format!("{workspace}{separator}{label}"), style)
}

/// The label of `workspace` in `style`, canonicalized when it exists here.
fn workspace_label(workspace: &Path, style: PathStyle) -> String {
    let workspace = paths::canonicalize(workspace).unwrap_or_else(|_| workspace.to_owned());
    paths::path_label(&workspace.to_string_lossy(), style)
}

/// Rule precedence among matching rules: more literal characters, then fewer
/// wildcards, then a runtime selector over none, then later declaration.
fn specificity(
    rule: &PermissionRule,
    declaration_index: usize,
    home: Option<&str>,
) -> (usize, std::cmp::Reverse<usize>, bool, usize) {
    let pattern = rule_pattern(rule, home);
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

/// Refuses overlay rules a client sends with a runtime selector their
/// action takes none of (`message@labpc`), as agent documents do. Only
/// commands are checked: an overlay already in a session log reads as it is.
pub fn check_rule_selectors(rules: &[PermissionRule]) -> Result<(), String> {
    rules.iter().try_for_each(|rule| {
        if let Some(variable) = WildcardPattern::MEMORY_VARIABLES
            .iter()
            .find(|variable| rule.resource.as_str().contains(**variable))
        {
            return Err(format!(
                "`{variable}` is only for agent documents' memory rules, not session rules"
            ));
        }
        rule.action.check_runtime_selector(rule.runtime.as_ref())
    })
}

pub fn governing_agent_for_skills(
    session: &cookie_agent_engine_store::session::SessionProjection,
) -> Arc<AgentSnapshot> {
    governing_agent(session)
}

/// The agent of the latest visible run, else the session's creation agent.
pub fn governing_agent(
    session: &cookie_agent_engine_store::session::SessionProjection,
) -> Arc<AgentSnapshot> {
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
pub fn stricter(old: &[PermissionEffect], new: &[PermissionEffect]) -> bool {
    old.iter()
        .zip(new)
        .any(|(old, new)| effect_rank(*new) > effect_rank(*old))
}

/// A [`PermissionScope`] that owns its parts.
#[derive(Clone, Debug, Default)]
pub struct OwnedScope {
    pub runtime: Option<String>,
    pub workspace: Option<std::path::PathBuf>,
    pub home: Option<String>,
    pub platform: Option<String>,
}

/// The effect of `action` on `resource` in each of `scopes`. A `~` in
/// `resource`, a rule's pattern, expands against each scope's home as the
/// rule's own pattern does.
pub fn rule_effects(
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
                platform: owned.platform.as_deref(),
            };
            let merged = MergedRules::of(policy, Some(overlay), scope.home);
            if resource == "*" {
                merged.decide_name_only(action, scope).0
            } else {
                merged
                    .decide(action, &expand_home_pattern(resource, scope.home), scope)
                    .0
            }
        })
        .collect()
}

/// Whether `rules`, one action's frozen rules, are what that action's
/// `defaults` expand to in a run of `agent` (see [`expand_memory_variables`]):
/// `${agent_id}` is the agent, and a default naming `${session_machine}` or
/// `${session_project}` stands for any number of rules, one per machine or
/// project name, none when the session has none.
fn expanded_from_defaults(
    defaults: &[&PermissionRule],
    rules: &[&PermissionRule],
    agent: &AgentId,
) -> bool {
    let [agent_id, session_machine, session_project] = WildcardPattern::MEMORY_VARIABLES;
    let mut rules = rules.iter().peekable();
    for default in defaults {
        let pattern = default.resource.as_str().replace(agent_id, agent.as_str());
        let session_variable = [session_machine, session_project]
            .into_iter()
            .find_map(|variable| pattern.split_once(variable));
        let same_rule = |rule: &PermissionRule| {
            rule.action == default.action
                && rule.runtime == default.runtime
                && rule.effect == default.effect
        };
        match session_variable {
            None => {
                let Some(rule) = rules.next() else {
                    return false;
                };
                if !same_rule(rule) || rule.resource.as_str() != pattern {
                    return false;
                }
            }
            Some((prefix, suffix)) => {
                let names_one = |rule: &PermissionRule| {
                    rule.resource
                        .as_str()
                        .strip_prefix(prefix)
                        .and_then(|rest| rest.strip_suffix(suffix))
                        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
                };
                while rules
                    .next_if(|rule| same_rule(rule) && names_one(rule))
                    .is_some()
                {}
            }
        }
    }
    rules.next().is_none()
}

/// The effective permission of every action, as the merged list of `policy`
/// and `overlay` ([`MergedRules`]) gives it: the built-in actions, then the
/// other actions the session's tools `declared`, then any other action a rule
/// names, each group in first-seen order. Each action reports its last
/// unqualified `*` rule as its effect (ask, what no rule matching gives,
/// with the source `default` without one) and lists every other rule as a pattern, each with its
/// source; an action whose policy rules are exactly its [mode
/// default](mode_default_rules), or its memory default (`memory_default_rules`)
/// for a root or `delegated` run, reports them with the source `default`. A
/// session `*` rule replaces only the `*` row: the document's other patterns
/// stay listed, because they still apply.
pub fn effective_permission_view(
    policy: &AgentSnapshot,
    delegated: bool,
    overlay: &SessionPermissionOverlay,
    declared: &[PermissionAction],
) -> Vec<EffectivePermissionAction> {
    let merged = MergedRules::of(policy, Some(overlay), None).label_defaults(policy, delegated);
    let mut actions = PermissionAction::BUILT_IN.to_vec();
    let named = merged.rules().iter().map(|merged| &merged.rule.action);
    for action in declared.iter().chain(named) {
        if !actions.contains(action) {
            actions.push(action.clone());
        }
    }
    actions
        .into_iter()
        .map(|action| {
            let of_action = merged
                .rules()
                .iter()
                .filter(|merged| merged.rule.action == action);
            let wildcard = of_action.clone().rfind(|merged| {
                merged.rule.runtime.is_none() && merged.rule.resource.as_str() == "*"
            });
            let (effect, source) = wildcard.map_or(
                (PermissionEffect::Ask, PermissionRuleSource::Default),
                |merged| (merged.rule.effect, merged.source),
            );
            let patterns = of_action
                .filter(|merged| {
                    merged.rule.runtime.is_some() || merged.rule.resource.as_str() != "*"
                })
                .map(|MergedRule { rule, source }| {
                    (
                        (rule.runtime.clone(), rule.resource.as_str().to_owned()),
                        EffectivePermissionRule {
                            runtime: rule.runtime.clone(),
                            resource: rule.resource.clone(),
                            effect: rule.effect,
                            source: *source,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>();
            EffectivePermissionAction {
                action,
                effect,
                source,
                patterns: patterns.into_values().collect(),
            }
        })
        .collect()
}

pub mod identity;

#[cfg(test)]
mod tests;
