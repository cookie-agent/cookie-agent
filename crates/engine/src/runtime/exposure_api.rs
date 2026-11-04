//! A session's tool groups and skills as clients reach them
//! (`docs/site/specs/session-tool-groups.md` sections 12 and 13):
//! `session.tools.get` lists them with their states, and
//! `session.tools.set` replaces the session's choices.

use std::collections::HashMap;

use cookie_agent_protocol::{
    EventOrigin, EventPayload, SessionId, SessionSkill, SessionToolGroup, SessionToolsGetResult,
    SkillState, ToolExposure, ToolGroupState,
};

use super::{Engine, EngineError};
use crate::exposure::{GroupKey, Layers};
use crate::permissions::{MergedRules, PermissionPipeline, PermissionScope};

/// A rough count of the tokens `text` costs: four bytes a token.
fn estimate_tokens(bytes: usize) -> u64 {
    u64::try_from(bytes.div_ceil(4)).unwrap_or(u64::MAX)
}

impl Engine {
    /// `session.tools.get`: the session's tool groups, one per group per
    /// runtime, and its skills, each with its state and the layer that
    /// decided it. Fixed groups and the loader are left out.
    pub fn get_session_tools(
        &self,
        session_id: SessionId,
    ) -> Result<SessionToolsGetResult, EngineError> {
        let projection = self.inner.store.get(session_id)?;
        let agent = crate::permissions::governing_agent(&projection);
        let acp = self
            .current_runtime()
            .agents
            .get(&agent.agent)
            .is_some_and(|resolved| resolved.acp.is_some());
        let layers = Layers::new(&agent.exposure, &projection.tool_exposure, acp);
        let config = self.inner.config.load();
        let composition = self.compose_session_tools(session_id, |_, _| true)?;
        let mut merged_by_home = Vec::<(Option<String>, MergedRules<'_>)>::new();
        let mut groups = Vec::<(GroupKey, SessionToolGroup, bool)>::new();
        let mut index = HashMap::<GroupKey, usize>::new();
        for tool in &composition.tools {
            if tool.source.provider.provider_id() == crate::base_tools::LOADER_PROVIDER_ID {
                continue;
            }
            let key = GroupKey::of(&tool.source, &tool.spec.name);
            if key.is_fixed() {
                continue;
            }
            let Some(selector) = key.selector() else {
                continue;
            };
            let offered = tool.spec.permission_name.as_deref().is_none_or(|name| {
                let scope = PermissionScope::of(&tool.source.origin);
                let at = merged_by_home
                    .iter()
                    .position(|(home, _)| home.as_deref() == scope.home)
                    .unwrap_or_else(|| {
                        merged_by_home.push((
                            scope.home.map(str::to_owned),
                            MergedRules::of(
                                &agent,
                                Some(&projection.permission_overlay),
                                scope.home,
                            ),
                        ));
                        merged_by_home.len() - 1
                    });
                PermissionPipeline::tool_offered_by(
                    &merged_by_home[at].1,
                    None,
                    name,
                    &tool.spec.name,
                    scope,
                )
            });
            let bytes = tool.name.len()
                + tool.spec.description.len()
                + tool.spec.parameters.to_string().len();
            let at = *index.entry(key.clone()).or_insert_with(|| {
                let decided = layers.decide(&key);
                // The defaults' `on` is sent up front or deferred as the
                // deferral policy says.
                let configured = if crate::deferred_tools::policy_defers(
                    &tool.source,
                    &tool.spec.name,
                    &config.runtime.deferred_tools,
                    None,
                ) {
                    ToolGroupState::Deferred
                } else {
                    ToolGroupState::On
                };
                let group = crate::deferred_tools::group_of(&tool.source, &tool.spec.name);
                groups.push((
                    key.clone(),
                    SessionToolGroup {
                        selector,
                        kind: key.kind,
                        name: key.name.clone(),
                        runtime: key.runtime.clone(),
                        attachment: tool.source.attachment.as_ref().map(ToString::to_string),
                        summary: group.summary,
                        state: decided.state.unwrap_or(configured),
                        layer: decided.layer,
                        below: decided.below.unwrap_or(configured),
                        below_layer: decided.below_layer,
                        hidden_by_permissions: false,
                        tools: Vec::new(),
                        token_estimate: 0,
                    },
                    false,
                ));
                groups.len() - 1
            });
            let (_, group, any_offered) = &mut groups[at];
            group.tools.push(tool.name.clone());
            group.token_estimate += estimate_tokens(bytes);
            *any_offered |= offered;
        }
        let groups = groups
            .into_iter()
            .map(|(_, mut group, any_offered)| {
                group.hidden_by_permissions = !any_offered;
                group
            })
            .collect();
        let view = self.session_skills(session_id)?;
        let skills = view
            .discoveries()
            .map(|(name, source, winner, skill)| {
                let decided = layers.decide_skill(name);
                SessionSkill {
                    name: name.to_owned(),
                    description: skill.description().to_owned(),
                    source,
                    attachment: skill.attachment_name().map(ToString::to_string),
                    state: decided.state.unwrap_or(SkillState::On),
                    layer: decided.layer,
                    below: decided.below.unwrap_or(SkillState::On),
                    below_layer: decided.below_layer,
                    shadowed: !winner,
                }
            })
            .collect();
        Ok(SessionToolsGetResult {
            groups,
            skills,
            exposure: projection.tool_exposure.clone(),
        })
    }

    /// `session.tools.set`: replaces the session's tool group and skill
    /// choices with `exposure`, which names groups and skills exactly.
    /// Takes effect from the session's next request.
    pub async fn set_session_tools(
        &self,
        session_id: SessionId,
        exposure: ToolExposure,
        origin: EventOrigin,
    ) -> Result<SessionToolsGetResult, EngineError> {
        exposure
            .validate_exact()
            .map_err(|error| EngineError::ToolExposure(error.to_string()))?;
        let projection = self.inner.store.get(session_id)?;
        if projection.tool_exposure != exposure {
            self.append(
                session_id,
                None,
                origin,
                EventPayload::SessionToolExposureSet { exposure },
            )
            .await?;
        }
        self.get_session_tools(session_id)
    }
}
