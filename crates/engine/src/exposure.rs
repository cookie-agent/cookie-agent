//! Tool exposure (`docs/site/specs/session-tool-groups.md`): which tool
//! groups a run is offered, on, deferred, or off, and which skills it
//! loads. Three layers hold rules: the defaults, the agent document, and
//! the session's choices (a project's defaults are copied into them). The
//! highest layer with a rule matching a group decides it (D12); within a
//! layer the most specific rule wins.

use cookie_agent_protocol::{
    ExposureLayer, FIXED_TOOL_GROUPS, RuntimeId, SkillState, ToolExposure, ToolGroupKind,
    ToolGroupSelector, ToolGroupSource, ToolGroupState,
};

use crate::runtimes::SourcedProvider;

/// The group ACP agents do not get by default: a fetch reads the same page
/// from nearly any machine, and the harness has its own.
const ACP_DEFAULT_OFF: &str = "web";

/// A project's defaults as a new session's choices: each marked as copied
/// from the project.
pub(crate) fn from_project(mut exposure: ToolExposure) -> ToolExposure {
    for rule in &mut exposure.tool_groups {
        rule.from_project = true;
    }
    for rule in &mut exposure.skills {
        rule.from_project = true;
    }
    exposure
}

/// A tool group on one runtime.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct GroupKey {
    pub(crate) kind: ToolGroupKind,
    pub(crate) name: String,
    /// The runtime that offers it; `None` for the base layer, `global`, and
    /// plugins.
    pub(crate) runtime: Option<RuntimeId>,
}

impl GroupKey {
    /// The group the tool `tool` of `source` belongs to.
    pub(crate) fn of(source: &SourcedProvider, tool: &str) -> Self {
        let group = crate::deferred_tools::group_of(source, tool);
        let kind = match group.source {
            ToolGroupSource::Mcp => ToolGroupKind::Mcp,
            ToolGroupSource::Plugin => ToolGroupKind::Plugin,
            _ => ToolGroupKind::Builtin,
        };
        let runtime = (kind != ToolGroupKind::Plugin)
            .then(|| source.origin.runtime())
            .flatten()
            .map(cookie_agent_runtime_api::RuntimeInfo::id)
            .filter(|id| *id != RuntimeId::GLOBAL)
            .and_then(|id| RuntimeId::new(id).ok());
        Self {
            kind,
            name: group.name,
            runtime,
        }
    }

    /// The exact selector a choice for this group names.
    pub(crate) fn selector(&self) -> Option<ToolGroupSelector> {
        ToolGroupSelector::new(self.kind, self.name.clone(), self.runtime.clone()).ok()
    }

    /// Whether the group is never switched (spec section 4.3).
    pub(crate) fn is_fixed(&self) -> bool {
        self.kind == ToolGroupKind::Builtin && FIXED_TOOL_GROUPS.contains(&self.name.as_str())
    }
}

/// What the layers decide for one group or skill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Decision<S> {
    /// The state; `None` is the defaults' `on`, deferred as configured.
    pub(crate) state: Option<S>,
    pub(crate) layer: ExposureLayer,
    /// What the layers below the session's choices give.
    pub(crate) below: Option<S>,
    pub(crate) below_layer: ExposureLayer,
}

/// The rules of one run's layers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layers<'a> {
    pub(crate) agent: &'a ToolExposure,
    pub(crate) session: &'a ToolExposure,
    /// The run is an ACP agent's: its defaults turn `web` off.
    pub(crate) acp: bool,
}

impl<'a> Layers<'a> {
    pub(crate) fn new(agent: &'a ToolExposure, session: &'a ToolExposure, acp: bool) -> Self {
        Self {
            agent,
            session,
            acp,
        }
    }

    /// The layers of a run of `policy` in the session `projection`.
    pub(crate) fn of(
        projection: &'a cookie_agent_engine_store::session::SessionProjection,
        policy: &'a crate::policy::FrozenRunPolicy,
    ) -> Self {
        Self::new(
            &policy.agent.exposure,
            &projection.tool_exposure,
            policy.acp_agent().is_some(),
        )
    }

    /// The state a choice forces on the deferral of the tool `tool` of
    /// `source`: `on` sends it up front, `deferred` defers it; `None` leaves
    /// it to the deferral policy.
    pub(crate) fn forced(&self, source: &SourcedProvider, tool: &str) -> Option<ToolGroupState> {
        self.decide(&GroupKey::of(source, tool)).state
    }

    /// The defaults' state for `key`.
    fn default_state(&self, key: &GroupKey) -> Option<ToolGroupState> {
        (self.acp && key.kind == ToolGroupKind::Builtin && key.name == ACP_DEFAULT_OFF)
            .then_some(ToolGroupState::Off)
    }

    /// What decides `key`. A fixed group is always the defaults' `on`.
    pub(crate) fn decide(&self, key: &GroupKey) -> Decision<ToolGroupState> {
        let below = if key.is_fixed() {
            None
        } else {
            self.agent
                .group_rule(key.kind, &key.name, key.runtime.as_ref())
                .map(|rule| (Some(rule.state), ExposureLayer::Agent))
        }
        .unwrap_or((self.default_state(key), ExposureLayer::Default));
        let session = (!key.is_fixed())
            .then(|| {
                self.session
                    .group_rule(key.kind, &key.name, key.runtime.as_ref())
            })
            .flatten()
            .map(|rule| {
                (
                    Some(rule.state),
                    if rule.from_project {
                        ExposureLayer::Project
                    } else {
                        ExposureLayer::Session
                    },
                )
            });
        let (state, layer) = session.unwrap_or(below);
        Decision {
            state,
            layer,
            below: below.0,
            below_layer: below.1,
        }
    }

    /// Whether the group `key` is offered.
    pub(crate) fn offers(&self, key: &GroupKey) -> bool {
        self.decide(key).state != Some(ToolGroupState::Off)
    }

    /// What decides the skill `name`.
    pub(crate) fn decide_skill(&self, name: &str) -> Decision<SkillState> {
        let below = self
            .agent
            .skill_rule(name)
            .map_or((None, ExposureLayer::Default), |rule| {
                (Some(rule.state), ExposureLayer::Agent)
            });
        let (state, layer) = self.session.skill_rule(name).map_or(below, |rule| {
            (
                Some(rule.state),
                if rule.from_project {
                    ExposureLayer::Project
                } else {
                    ExposureLayer::Session
                },
            )
        });
        Decision {
            state,
            layer,
            below: below.0,
            below_layer: below.1,
        }
    }

    /// Whether the skill `name` is on for the agent.
    pub(crate) fn skill_on(&self, name: &str) -> bool {
        self.decide_skill(name).state != Some(SkillState::Off)
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{SkillPattern, SkillRule, ToolGroupRule};

    use super::*;

    fn rules(groups: &[(&str, ToolGroupState, bool)]) -> ToolExposure {
        ToolExposure {
            tool_groups: groups
                .iter()
                .map(|(selector, state, from_project)| ToolGroupRule {
                    selector: selector.parse().unwrap(),
                    state: *state,
                    from_project: *from_project,
                })
                .collect(),
            skills: Vec::new(),
        }
    }

    fn key(kind: ToolGroupKind, name: &str, runtime: Option<&str>) -> GroupKey {
        GroupKey {
            kind,
            name: name.to_owned(),
            runtime: runtime.map(|id| RuntimeId::new(id).unwrap()),
        }
    }

    #[test]
    fn any_session_rule_beats_every_agent_rule() {
        let agent = rules(&[("coding@labpc", ToolGroupState::On, false)]);
        let session = rules(&[("coding", ToolGroupState::Off, false)]);
        let layers = Layers::new(&agent, &session, false);
        let decided = layers.decide(&key(ToolGroupKind::Builtin, "coding", Some("labpc")));
        assert_eq!(decided.state, Some(ToolGroupState::Off));
        assert_eq!(decided.layer, ExposureLayer::Session);
        assert_eq!(decided.below, Some(ToolGroupState::On));
        assert_eq!(decided.below_layer, ExposureLayer::Agent);
    }

    #[test]
    fn a_layer_without_a_matching_rule_passes_the_group_down() {
        let agent = rules(&[("mcp:*", ToolGroupState::Deferred, false)]);
        let session = rules(&[("mcp:exa", ToolGroupState::Off, true)]);
        let layers = Layers::new(&agent, &session, false);
        let github = layers.decide(&key(ToolGroupKind::Mcp, "github", None));
        assert_eq!(
            (github.state, github.layer),
            (Some(ToolGroupState::Deferred), ExposureLayer::Agent)
        );
        let exa = layers.decide(&key(ToolGroupKind::Mcp, "exa", None));
        assert_eq!(
            (exa.state, exa.layer),
            (Some(ToolGroupState::Off), ExposureLayer::Project)
        );
        let coding = layers.decide(&key(ToolGroupKind::Builtin, "coding", None));
        assert_eq!((coding.state, coding.layer), (None, ExposureLayer::Default));
    }

    #[test]
    fn acp_agents_default_web_off_and_a_document_turns_it_back_on() {
        let empty = ToolExposure::default();
        let web = key(ToolGroupKind::Builtin, "web", Some("local"));
        assert!(!Layers::new(&empty, &empty, true).offers(&web));
        assert!(Layers::new(&empty, &empty, false).offers(&web));
        let agent = rules(&[("web", ToolGroupState::On, false)]);
        assert!(Layers::new(&agent, &empty, true).offers(&web));
    }

    #[test]
    fn fixed_groups_ignore_patterns() {
        let agent = rules(&[("*", ToolGroupState::Off, false)]);
        let empty = ToolExposure::default();
        let layers = Layers::new(&agent, &empty, false);
        assert!(layers.offers(&key(ToolGroupKind::Builtin, "artifacts", None)));
        assert!(!layers.offers(&key(ToolGroupKind::Builtin, "coding", None)));
    }

    #[test]
    fn skills_follow_the_same_layers() {
        let agent = ToolExposure {
            tool_groups: Vec::new(),
            skills: vec![SkillRule {
                skill: SkillPattern::new("*").unwrap(),
                state: SkillState::Off,
                from_project: false,
            }],
        };
        let session = ToolExposure {
            tool_groups: Vec::new(),
            skills: vec![SkillRule {
                skill: SkillPattern::new("deploy").unwrap(),
                state: SkillState::On,
                from_project: false,
            }],
        };
        let layers = Layers::new(&agent, &session, false);
        assert!(layers.skill_on("deploy"));
        assert!(!layers.skill_on("release-check"));
    }
}
