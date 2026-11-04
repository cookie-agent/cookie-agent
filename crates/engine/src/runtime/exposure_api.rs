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
            // A server turned back on connects; one turned off stops once
            // no session wants it.
            self.refresh_mcp_wanted();
        }
        self.get_session_tools(session_id)
    }
}

impl Engine {
    /// The tool groups and project skills a running instance offers.
    fn instance_groups(
        &self,
        instance: &crate::runtimes::RuntimeInstance,
    ) -> (
        Vec<cookie_agent_protocol::RuntimeToolGroup>,
        Vec<cookie_agent_protocol::RuntimeSkill>,
    ) {
        let info = instance.info();
        let attachment = cookie_agent_protocol::AttachmentName::new(info.attachment_name()).ok();
        let origin = instance.origin();
        let (generation, providers) = instance.sources();
        let mut sources = providers
            .into_iter()
            .map(|provider| crate::runtimes::SourcedProvider {
                attachment: attachment.clone(),
                generation,
                origin: origin.clone(),
                provider,
            })
            .collect::<Vec<_>>();
        if let Some((generation, shared)) = instance.shared_source() {
            sources.push(crate::runtimes::SourcedProvider {
                attachment: attachment.clone(),
                generation,
                origin: crate::ToolOrigin::Runtime(shared.info),
                provider: shared.provider,
            });
        }
        let config = self.inner.config.load();
        let context = cookie_agent_runtime_api::SessionToolContext::new(SessionId::new_v7());
        let mut groups = Vec::<(GroupKey, cookie_agent_protocol::RuntimeToolGroup)>::new();
        for source in &sources {
            let Ok(specs) = source.provider.tools_for_session(&context) else {
                continue;
            };
            for spec in specs {
                let key = GroupKey::of(source, &spec.name);
                if key.is_fixed() {
                    continue;
                }
                let Some(selector) = key.selector() else {
                    continue;
                };
                let bytes =
                    spec.name.len() + spec.description.len() + spec.parameters.to_string().len();
                match groups.iter_mut().find(|(known, _)| *known == key) {
                    Some((_, group)) => {
                        group.tools.push(spec.name.clone());
                        group.token_estimate += estimate_tokens(bytes);
                    }
                    None => {
                        let deferred = crate::deferred_tools::policy_defers(
                            source,
                            &spec.name,
                            &config.runtime.deferred_tools,
                            None,
                        );
                        let group = crate::deferred_tools::group_of(source, &spec.name);
                        groups.push((
                            key.clone(),
                            cookie_agent_protocol::RuntimeToolGroup {
                                selector,
                                kind: key.kind,
                                name: key.name.clone(),
                                summary: group.summary,
                                tools: vec![spec.name.clone()],
                                token_estimate: estimate_tokens(bytes),
                                default: if deferred {
                                    ToolGroupState::Deferred
                                } else {
                                    ToolGroupState::On
                                },
                            },
                        ));
                    }
                }
            }
        }
        let registry = instance.skills();
        let skills = registry
            .skills()
            .map(|(name, document)| cookie_agent_protocol::RuntimeSkill {
                name: name.clone(),
                description: document.frontmatter.description.clone(),
                source: document.source,
            })
            .collect();
        (groups.into_iter().map(|(_, group)| group).collect(), skills)
    }

    /// Remembers what a running instance offers, at most once a minute
    /// (session tool groups spec, section 4.4).
    pub(crate) fn remember_instance(&self, instance: &crate::runtimes::RuntimeInstance) {
        let info = instance.info();
        if info.id() == cookie_agent_protocol::RuntimeId::GLOBAL {
            return;
        }
        let workspace = info
            .workspace()
            .map(|workspace| workspace.root().to_string_lossy().into_owned());
        if !self.inner.runtime_groups.due(
            info.id(),
            info.capability().as_str(),
            workspace.as_deref(),
            instance.generation(),
        ) {
            return;
        }
        let (groups, skills) = self.instance_groups(instance);
        self.inner
            .runtime_groups
            .record(crate::runtime_groups::Entry {
                runtime: info.id().to_owned(),
                capability: info.capability().as_str().to_owned(),
                workspace,
                seen: jiff::Timestamp::now(),
                groups,
                skills,
            });
    }

    /// `runtimes.groups`: what `runtime_id`'s capability brings at
    /// `workspace`: live from a running instance, else as of the last time
    /// it ran there, else at its latest workspace.
    pub fn runtime_groups(
        &self,
        params: cookie_agent_protocol::RuntimeGroupsParams,
    ) -> Result<cookie_agent_protocol::RuntimeGroupsResult, EngineError> {
        let runtime = params.runtime_id.as_str();
        let registry = &self.inner.runtimes;
        let template = registry
            .templates()
            .into_iter()
            .find(|template| template.runtime_id == runtime);
        let capability = params
            .capability
            .as_ref()
            .map(|capability| capability.as_str().to_owned())
            .or_else(|| {
                template.as_ref().and_then(|template| {
                    (template.listing.capabilities.len() == 1)
                        .then(|| template.listing.capabilities[0].name.as_str().to_owned())
                })
            })
            .unwrap_or_else(|| {
                if runtime == cookie_agent_protocol::RuntimeId::GLOBAL {
                    "global".to_owned()
                } else {
                    "coding".to_owned()
                }
            });
        // The workspace as the runtime spells it: `~` against its home, and
        // the daemon's own machine's path canonical for `local`.
        let home = template
            .as_ref()
            .and_then(|template| template.listing.home.clone())
            .or_else(|| {
                self.inner
                    .machines
                    .record(&params.runtime_id)
                    .and_then(|record| record.home)
            });
        let workspace = params.workspace.as_deref().map(|typed| {
            let expanded = match (typed.strip_prefix("~"), &home) {
                (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
                    format!("{}{rest}", home.trim_end_matches('/'))
                }
                _ => typed.to_owned(),
            };
            if runtime == "local" {
                std::fs::canonicalize(&expanded)
                    .map_or(expanded, |path| path.to_string_lossy().into_owned())
            } else {
                expanded
            }
        });
        let live = if runtime == cookie_agent_protocol::RuntimeId::GLOBAL {
            Some(std::sync::Arc::clone(registry.global()))
        } else {
            registry
                .instance_at(runtime, workspace.as_deref())
                .filter(|instance| instance.info().capability().as_str() == capability)
        };
        if let Some(instance) = live {
            self.remember_instance(&instance);
            let (groups, skills) = self.instance_groups(&instance);
            return Ok(cookie_agent_protocol::RuntimeGroupsResult {
                known: true,
                live: true,
                as_of: None,
                seen_at: None,
                groups,
                skills,
            });
        }
        let known_runtime =
            template.is_some() || self.inner.machines.record(&params.runtime_id).is_some();
        if !known_runtime {
            self.inner
                .runtime_groups
                .retain(|runtime| registry.knows(runtime) || runtime == "local");
        }
        Ok(
            match self
                .inner
                .runtime_groups
                .lookup(runtime, &capability, workspace.as_deref())
                .filter(|_| known_runtime)
            {
                Some((entry, exact)) => cookie_agent_protocol::RuntimeGroupsResult {
                    known: true,
                    live: false,
                    as_of: Some(entry.seen),
                    seen_at: (!exact).then(|| entry.workspace.clone()).flatten(),
                    groups: entry.groups,
                    // Project skills belong to the workspace they came from.
                    skills: if exact { entry.skills } else { Vec::new() },
                },
                None => cookie_agent_protocol::RuntimeGroupsResult {
                    known: false,
                    live: false,
                    as_of: None,
                    seen_at: None,
                    groups: Vec::new(),
                    skills: Vec::new(),
                },
            },
        )
    }
}
