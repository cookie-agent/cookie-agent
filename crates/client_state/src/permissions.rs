//! The `/permissions` table: a session's effective rules as rows grouped by
//! action, and the rules a new-session draft holds until `session.create`.

use cookie_agent_protocol::{
    EffectivePermissionAction, PermissionAction, PermissionEffect, PermissionRule,
    PermissionRuleSource, RuntimeId, RuntimesListResult, WildcardPattern,
    parse_permission_key_parts,
};

/// The effects a rule can have, in the order a segmented control shows them.
pub const EFFECTS: [PermissionEffect; 3] = [
    PermissionEffect::Allow,
    PermissionEffect::Ask,
    PermissionEffect::Deny,
];

/// One row of the table: an action's default (`*`) or one of its patterns,
/// which may be limited to one runtime (`bash@labpc`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRow {
    pub action: PermissionAction,
    pub runtime: Option<RuntimeId>,
    pub resource: String,
    pub effect: PermissionEffect,
    pub source: PermissionRuleSource,
}

/// The rows of `permissions`: each action's `*` row, then its patterns.
pub fn permission_rows(permissions: &[EffectivePermissionAction]) -> Vec<PermissionRow> {
    permissions
        .iter()
        .flat_map(|action| {
            std::iter::once(PermissionRow {
                action: action.action.clone(),
                runtime: None,
                resource: "*".into(),
                effect: action.effect,
                source: action.source,
            })
            .chain(action.patterns.iter().map(|rule| PermissionRow {
                action: action.action.clone(),
                runtime: rule.runtime.clone(),
                resource: rule.resource.as_str().to_owned(),
                effect: rule.effect,
                source: rule.source,
            }))
        })
        .collect()
}

impl PermissionRow {
    /// The rule's key as agent documents write it: `bash`, `bash@labpc`.
    #[must_use]
    pub fn key(&self) -> String {
        rule_key(&self.action, self.runtime.as_ref())
    }
}

/// A rule key as agent documents write it: `action` or `action@runtime_id`.
#[must_use]
pub fn rule_key(action: &PermissionAction, runtime: Option<&RuntimeId>) -> String {
    match runtime {
        Some(runtime) => format!("{action}@{runtime}"),
        None => action.to_string(),
    }
}

/// Reads a rule key from the form's action and runtime fields, checked as the
/// daemon checks agent documents: an empty runtime field means every runtime,
/// and `message` and `skill` take no runtime.
pub fn parse_rule_key(
    action: &str,
    runtime: &str,
) -> Result<(PermissionAction, Option<RuntimeId>), String> {
    let action = action.trim();
    if action.is_empty() {
        return Err("a rule needs an action".into());
    }
    let runtime = runtime.trim();
    parse_permission_key_parts(action, (!runtime.is_empty()).then_some(runtime))
}

/// Whether a rule for `action` may name a runtime; an action the field does
/// not hold a valid name of may, until it does.
#[must_use]
pub fn takes_runtime(action: &str) -> bool {
    PermissionAction::new(action.trim())
        .ok()
        .is_none_or(|action| action.takes_runtime_selector())
}

/// The actions a rule can name, in table order: the ones the daemon reports
/// for the session (the built-in actions, those its tools declare, and those
/// its rules name), or the built-in actions until it has. The form suggests
/// them; any other valid action may be typed.
pub fn permission_actions(
    permissions: Option<&[EffectivePermissionAction]>,
) -> Vec<PermissionAction> {
    permissions.map_or_else(
        || PermissionAction::BUILT_IN.to_vec(),
        |permissions| {
            permissions
                .iter()
                .map(|action| action.action.clone())
                .collect()
        },
    )
}

/// The runtimes a rule can name, from `runtimes.list`, sorted.
pub fn rule_runtimes(listing: &RuntimesListResult) -> Vec<RuntimeId> {
    let mut runtimes: Vec<RuntimeId> = listing
        .runtimes
        .iter()
        .map(|runtime| runtime.runtime_id.clone())
        .collect();
    runtimes.sort();
    runtimes.dedup();
    runtimes
}

pub fn effect_label(effect: PermissionEffect) -> &'static str {
    match effect {
        PermissionEffect::Allow => "allow",
        PermissionEffect::Ask => "ask",
        PermissionEffect::Deny => "deny",
    }
}

pub fn source_label(source: PermissionRuleSource) -> &'static str {
    match source {
        PermissionRuleSource::SessionOverlay => "session",
        PermissionRuleSource::AgentDocument => "agent",
        PermissionRuleSource::Default => "default",
    }
}

/// Sets a draft rule, replacing any rule for the same key and pattern;
/// rules stay in edit order.
pub fn set_draft_rule(rules: &mut Vec<PermissionRule>, rule: PermissionRule) {
    remove_draft_rule(rules, &rule.action, rule.runtime.as_ref(), &rule.resource);
    rules.push(rule);
}

/// Removes the draft rule for `action` (on `runtime`) and `resource`.
pub fn remove_draft_rule(
    rules: &mut Vec<PermissionRule>,
    action: &PermissionAction,
    runtime: Option<&RuntimeId>,
    resource: &WildcardPattern,
) {
    rules.retain(|rule| {
        rule.action != *action || rule.runtime.as_ref() != runtime || &rule.resource != resource
    });
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        EffectivePermissionAction, PermissionAction, PermissionEffect, PermissionRule,
        PermissionRuleSource, RuntimeId, WildcardPattern,
    };

    use super::{
        PermissionRow, parse_rule_key, permission_rows, remove_draft_rule, set_draft_rule,
        takes_runtime,
    };

    fn runtime(id: &str) -> RuntimeId {
        RuntimeId::new(id).expect("runtime")
    }

    #[test]
    fn each_action_gives_its_default_row_then_its_patterns() {
        let permissions: Vec<EffectivePermissionAction> = serde_json::from_value(serde_json::json!([
            {
                "action": "bash",
                "effect": "ask",
                "source": "agent_document",
                "patterns": [
                    {"runtime": null, "resource": "git *", "effect": "allow", "source": "session_overlay"},
                    {"runtime": "labpc", "resource": "*", "effect": "deny", "source": "agent_document"}
                ]
            },
            {"action": "read", "effect": "allow", "source": "default", "patterns": []}
        ]))
        .expect("permissions");
        let row = |key: &str, resource: &str, effect, source| {
            let (action, runtime) = key.split_once('@').unwrap_or((key, ""));
            let (action, runtime) = parse_rule_key(action, runtime).expect("key");
            PermissionRow {
                action,
                runtime,
                resource: resource.into(),
                effect,
                source,
            }
        };
        let rows = permission_rows(&permissions);
        assert_eq!(rows[2].key(), "bash@labpc");
        assert_eq!(
            rows,
            vec![
                row(
                    "bash",
                    "*",
                    PermissionEffect::Ask,
                    PermissionRuleSource::AgentDocument
                ),
                row(
                    "bash",
                    "git *",
                    PermissionEffect::Allow,
                    PermissionRuleSource::SessionOverlay
                ),
                row(
                    "bash@labpc",
                    "*",
                    PermissionEffect::Deny,
                    PermissionRuleSource::AgentDocument
                ),
                row(
                    "read",
                    "*",
                    PermissionEffect::Allow,
                    PermissionRuleSource::Default
                ),
            ]
        );
    }

    #[test]
    fn a_draft_holds_one_rule_per_action_and_pattern() {
        let rule = |resource: &str, effect| PermissionRule {
            runtime: None,
            action: PermissionAction::new("bash").expect("action"),
            resource: WildcardPattern::new(resource).expect("pattern"),
            effect,
        };
        let mut rules = Vec::new();
        set_draft_rule(&mut rules, rule("git *", PermissionEffect::Allow));
        set_draft_rule(&mut rules, rule("rm *", PermissionEffect::Deny));
        set_draft_rule(&mut rules, rule("git *", PermissionEffect::Ask));
        assert_eq!(
            rules,
            vec![
                rule("rm *", PermissionEffect::Deny),
                rule("git *", PermissionEffect::Ask)
            ]
        );
        // The same pattern on one runtime is a rule of its own.
        let on_labpc = PermissionRule {
            runtime: Some(runtime("labpc")),
            ..rule("git *", PermissionEffect::Deny)
        };
        set_draft_rule(&mut rules, on_labpc.clone());
        remove_draft_rule(
            &mut rules,
            &PermissionAction::new("bash").expect("action"),
            None,
            &WildcardPattern::new("rm *").expect("pattern"),
        );
        assert_eq!(rules, vec![rule("git *", PermissionEffect::Ask), on_labpc]);
        remove_draft_rule(
            &mut rules,
            &PermissionAction::new("bash").expect("action"),
            Some(&runtime("labpc")),
            &WildcardPattern::new("git *").expect("pattern"),
        );
        assert_eq!(rules, vec![rule("git *", PermissionEffect::Ask)]);
    }

    #[test]
    fn a_rule_key_reads_an_action_and_an_optional_runtime() {
        let action = |name: &str| PermissionAction::new(name).expect("action");
        assert_eq!(parse_rule_key(" bash ", ""), Ok((action("bash"), None)));
        assert_eq!(
            parse_rule_key("bash", " labpc "),
            Ok((action("bash"), Some(runtime("labpc"))))
        );
        // Any action a tool declares, not only the built-in ones.
        assert_eq!(
            parse_rule_key("deploy_prod", "box"),
            Ok((action("deploy_prod"), Some(runtime("box"))))
        );
        // `delegate@<runtime>` limits where a root delegates.
        assert_eq!(
            parse_rule_key("delegate", "labpc"),
            Ok((action("delegate"), Some(runtime("labpc"))))
        );
        assert_eq!(
            parse_rule_key("  ", "labpc"),
            Err("a rule needs an action".into())
        );
        for invalid in [
            "Bash",
            "1bash",
            "ba sh",
            "bash@labpc",
            "a23456789012345678901234567890123",
        ] {
            assert!(
                parse_rule_key(invalid, "")
                    .is_err_and(|error| error.contains("invalid permission action")),
                "{invalid}"
            );
        }
        for invalid in ["Labpc", "lab-pc", "lab@pc"] {
            assert!(
                parse_rule_key("bash", invalid)
                    .is_err_and(|error| error.contains("invalid runtime selector")),
                "{invalid}"
            );
        }
    }

    #[test]
    fn message_and_skill_refuse_a_runtime() {
        for name in ["message", "skill"] {
            assert_eq!(
                parse_rule_key(name, "labpc"),
                Err(format!(
                    "`{name}@labpc`: `message` and `skill` belong to the base layer and take no runtime selector"
                ))
            );
            assert!(parse_rule_key(name, "").is_ok());
            assert!(!takes_runtime(name));
        }
        for name in ["bash", "delegate", "deploy", "", "Not valid"] {
            assert!(takes_runtime(name), "{name}");
        }
    }
}
