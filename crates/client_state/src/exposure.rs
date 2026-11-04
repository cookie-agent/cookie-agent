//! Tool group and skill choices as clients make them
//! (`docs/site/specs/session-tool-groups.md` section 12): one group or one
//! skill at a time, by exact selector or name.

use cookie_agent_protocol::{
    SkillPattern, SkillRule, SkillState, ToolExposure, ToolGroupRule, ToolGroupSelector,
    ToolGroupState,
};

/// `exposure` as copied into a session of its project: each rule marked
/// `from_project`.
#[must_use]
pub fn from_project(exposure: &ToolExposure) -> ToolExposure {
    let mut copy = exposure.clone();
    for rule in &mut copy.tool_groups {
        rule.from_project = true;
    }
    for rule in &mut copy.skills {
        rule.from_project = true;
    }
    copy
}

/// `exposure` with `state` chosen for the group `selector`, or with `None`
/// without a choice for it. Choosing what the layers below give (`below`)
/// keeps no choice rather than a copy.
#[must_use]
pub fn with_group(
    mut exposure: ToolExposure,
    selector: ToolGroupSelector,
    state: Option<ToolGroupState>,
    below: ToolGroupState,
) -> ToolExposure {
    exposure
        .tool_groups
        .retain(|rule| rule.selector != selector);
    if let Some(state) = state.filter(|state| *state != below) {
        exposure.tool_groups.push(ToolGroupRule {
            selector,
            state,
            from_project: false,
        });
    }
    exposure
}

/// `exposure` with `state` chosen for the skill `name`, or with `None`
/// without a choice for it; as [`with_group`].
#[must_use]
pub fn with_skill(
    mut exposure: ToolExposure,
    name: &str,
    state: Option<SkillState>,
    below: SkillState,
) -> ToolExposure {
    exposure.skills.retain(|rule| rule.skill.as_str() != name);
    if let Some(state) = state.filter(|state| *state != below)
        && let Ok(skill) = SkillPattern::new(name)
    {
        exposure.skills.push(SkillRule {
            skill,
            state,
            from_project: false,
        });
    }
    exposure
}

fn group_state_word(state: ToolGroupState) -> &'static str {
    match state {
        ToolGroupState::On => "on",
        ToolGroupState::Deferred => "deferred",
        ToolGroupState::Off => "off",
    }
}

/// The quiet transcript row of a change from `before` to `after`, made by
/// `origin`: each group and skill whose choice changed, a dropped choice
/// as reset.
#[must_use]
pub fn change_text(
    before: &ToolExposure,
    after: &ToolExposure,
    origin: Option<&cookie_agent_protocol::EventOrigin>,
) -> Option<String> {
    let mut parts = Vec::new();
    for rule in &after.tool_groups {
        let earlier = before
            .tool_groups
            .iter()
            .find(|known| known.selector == rule.selector);
        if earlier.is_none_or(|earlier| earlier.state != rule.state) {
            parts.push(format!(
                "{} {}",
                rule.selector,
                group_state_word(rule.state)
            ));
        }
    }
    for rule in &before.tool_groups {
        if !after
            .tool_groups
            .iter()
            .any(|known| known.selector == rule.selector)
        {
            parts.push(format!("{} reset", rule.selector));
        }
    }
    for rule in &after.skills {
        let earlier = before.skills.iter().find(|known| known.skill == rule.skill);
        if earlier.is_none_or(|earlier| earlier.state != rule.state) {
            let state = if rule.state == SkillState::On {
                "on"
            } else {
                "off"
            };
            parts.push(format!("skill {} {state}", rule.skill.as_str()));
        }
    }
    for rule in &before.skills {
        if !after.skills.iter().any(|known| known.skill == rule.skill) {
            parts.push(format!("skill {} reset", rule.skill.as_str()));
        }
    }
    if parts.is_empty() {
        return None;
    }
    let all_from_project = before.is_empty()
        && after.tool_groups.iter().all(|rule| rule.from_project)
        && after.skills.iter().all(|rule| rule.from_project);
    let by = if all_from_project {
        " from the project".to_owned()
    } else {
        origin.map_or_else(String::new, |origin| format!(" by {origin}"))
    };
    Some(format!("tool groups changed{by}: {}", parts.join(", ")))
}

/// How many groups the session's own choices turn off.
#[must_use]
pub fn groups_off(exposure: &ToolExposure) -> usize {
    exposure
        .tool_groups
        .iter()
        .filter(|rule| rule.state == ToolGroupState::Off)
        .count()
}

/// The state after `state` as left/right steps through them.
#[must_use]
pub fn step_group_state(state: ToolGroupState, forward: bool) -> ToolGroupState {
    use ToolGroupState::{Deferred, Off, On};
    match (state, forward) {
        (On, true) | (Off, false) => Deferred,
        (Deferred, true) | (On, false) => Off,
        (Off, true) | (Deferred, false) => On,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector(value: &str) -> ToolGroupSelector {
        value.parse().unwrap()
    }

    #[test]
    fn a_choice_replaces_the_one_before_and_one_like_the_layers_below_is_dropped() {
        let exposure = with_group(
            ToolExposure::default(),
            selector("mcp:exa"),
            Some(ToolGroupState::Off),
            ToolGroupState::On,
        );
        let mut copy = from_project(&exposure);
        assert!(copy.tool_groups[0].from_project);
        copy = with_group(
            copy,
            selector("mcp:exa"),
            Some(ToolGroupState::Deferred),
            ToolGroupState::On,
        );
        assert_eq!(copy.tool_groups.len(), 1);
        assert_eq!(copy.tool_groups[0].state, ToolGroupState::Deferred);
        assert!(!copy.tool_groups[0].from_project);
        let back = with_group(
            copy,
            selector("mcp:exa"),
            Some(ToolGroupState::On),
            ToolGroupState::On,
        );
        assert!(back.is_empty());
        let skills = with_skill(
            ToolExposure::default(),
            "deploy",
            Some(SkillState::Off),
            SkillState::On,
        );
        assert_eq!(skills.skills[0].skill.as_str(), "deploy");
        assert!(with_skill(skills, "deploy", None, SkillState::On).is_empty());
    }

    #[test]
    fn a_change_names_each_choice_that_changed() {
        let before = with_group(
            ToolExposure::default(),
            selector("mcp:exa"),
            Some(ToolGroupState::Off),
            ToolGroupState::On,
        );
        let after = with_group(
            with_skill(
                ToolExposure::default(),
                "deploy",
                Some(SkillState::Off),
                SkillState::On,
            ),
            selector("coding@labpc"),
            Some(ToolGroupState::Deferred),
            ToolGroupState::On,
        );
        let origin = cookie_agent_protocol::EventOrigin::new("client:web").unwrap();
        assert_eq!(
            change_text(&before, &after, Some(&origin)).as_deref(),
            Some(
                "tool groups changed by client:web: coding@labpc deferred, mcp:exa reset, skill deploy off"
            )
        );
        assert_eq!(change_text(&after, &after, Some(&origin)), None);
        assert_eq!(
            change_text(
                &ToolExposure::default(),
                &from_project(&before),
                Some(&origin)
            )
            .as_deref(),
            Some("tool groups changed from the project: mcp:exa off")
        );
        assert_eq!(groups_off(&before), 1);
    }

    #[test]
    fn states_step_in_a_ring() {
        let mut state = ToolGroupState::On;
        for expected in [
            ToolGroupState::Deferred,
            ToolGroupState::Off,
            ToolGroupState::On,
        ] {
            state = step_group_state(state, true);
            assert_eq!(state, expected);
        }
        assert_eq!(
            step_group_state(ToolGroupState::On, false),
            ToolGroupState::Off
        );
    }
}
