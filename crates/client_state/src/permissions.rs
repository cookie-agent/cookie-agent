//! The `/permissions` table: a session's effective rules as rows grouped by
//! action, and the rules a new-session draft holds until `session.create`.

use cookie_agent_protocol::{
    EffectivePermissionAction, PermissionAction, PermissionEffect, PermissionRule,
    PermissionRuleSource, WildcardPattern,
};

/// The effects a rule can have, in the order a segmented control shows them.
pub const EFFECTS: [PermissionEffect; 3] = [
    PermissionEffect::Allow,
    PermissionEffect::Ask,
    PermissionEffect::Deny,
];

/// One row of the table: an action's default (`*`) or one of its patterns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRow {
    pub action: PermissionAction,
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
                resource: "*".into(),
                effect: action.effect,
                source: action.source,
            })
            .chain(action.patterns.iter().map(|rule| PermissionRow {
                action: action.action.clone(),
                resource: rule.resource.as_str().to_owned(),
                effect: rule.effect,
                source: rule.source,
            }))
        })
        .collect()
}

/// The actions a rule can name, in table order: the ones the daemon reports
/// for the session, or the built-in actions until it has.
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

/// Sets a draft rule, replacing any rule for the same action and pattern;
/// rules stay in edit order.
pub fn set_draft_rule(rules: &mut Vec<PermissionRule>, rule: PermissionRule) {
    remove_draft_rule(rules, &rule.action, &rule.resource);
    rules.push(rule);
}

/// Removes the draft rule for `action` and `resource`.
pub fn remove_draft_rule(
    rules: &mut Vec<PermissionRule>,
    action: &PermissionAction,
    resource: &WildcardPattern,
) {
    rules.retain(|rule| rule.action != *action || &rule.resource != resource);
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        EffectivePermissionAction, PermissionAction, PermissionEffect, PermissionRule,
        PermissionRuleSource, WildcardPattern,
    };

    use super::{PermissionRow, permission_rows, remove_draft_rule, set_draft_rule};

    #[test]
    fn each_action_gives_its_default_row_then_its_patterns() {
        let permissions: Vec<EffectivePermissionAction> = serde_json::from_value(serde_json::json!([
            {
                "action": "bash",
                "effect": "ask",
                "source": "agent_document",
                "patterns": [
                    {"runtime": null, "resource": "git *", "effect": "allow", "source": "session_overlay"}
                ]
            },
            {"action": "read", "effect": "allow", "source": "default", "patterns": []}
        ]))
        .expect("permissions");
        let row = |action: &str, resource: &str, effect, source| PermissionRow {
            action: PermissionAction::new(action).expect("action"),
            resource: resource.into(),
            effect,
            source,
        };
        assert_eq!(
            permission_rows(&permissions),
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
        remove_draft_rule(
            &mut rules,
            &PermissionAction::new("bash").expect("action"),
            &WildcardPattern::new("rm *").expect("pattern"),
        );
        assert_eq!(rules, vec![rule("git *", PermissionEffect::Ask)]);
    }
}
