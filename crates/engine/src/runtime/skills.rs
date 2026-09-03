use std::{collections::BTreeMap, path::Path, sync::Arc};

use cookie_agent_config::{SkillContext, SkillDocument};
use cookie_agent_protocol::{
    EventPayload, PermissionAction, PermissionEffect, PermissionRule, RunId, SessionId,
    SessionPermissionOverlay, Sha256Digest, SkillDescriptor, SkillsGetResult, SkillsListResult,
    ToolCallId,
};

use super::{ActiveRun, Engine, EngineError};
use crate::{ToolCall, TurnAgentContext};
use crate::{permissions, policy::FrozenRunPolicy};
pub const RESERVED_STAGED_SKILL_PREFIX: &str = "\0cookie-staged-skill:";

#[derive(Clone, Debug)]
pub struct SkillInvocation {
    pub name: String,
    pub rendered: String,
    pub context: Option<SkillContext>,
}

#[derive(Clone, Debug)]
pub(crate) struct SkillGrantOverlay {
    source: String,
    overlay: SessionPermissionOverlay,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedSkillInvocation {
    name: String,
    args: String,
    rendered: String,
    source_path: String,
    base_dir: String,
    supporting_files: Vec<String>,
    grants: Vec<PermissionRule>,
    model: Option<cookie_agent_protocol::ModelKey>,
    context: Option<SkillContext>,
}

impl PreparedSkillInvocation {
    pub(super) fn grants(&self) -> Option<SessionPermissionOverlay> {
        (!self.grants.is_empty()).then(|| SessionPermissionOverlay {
            rules: self.grants.clone(),
        })
    }

    pub(crate) fn staged_payload(&self) -> cookie_agent_protocol::StagedSkillPayload {
        cookie_agent_protocol::StagedSkillPayload {
            provenance: cookie_agent_protocol::StagedSkillProvenance::SkillFork,
            name: self.name.clone(),
            args: self.args.clone(),
            rendered_body: self.rendered.clone(),
            source_path: self.source_path.clone(),
            base_dir: self.base_dir.clone(),
            supporting_files: self.supporting_files.clone(),
            grants: self.grants.clone(),
            model: self.model.clone(),
        }
    }
}

impl Engine {
    #[must_use]
    pub fn is_reserved_staged_skill_prompt(prompt: &str) -> bool {
        prompt.starts_with(RESERVED_STAGED_SKILL_PREFIX)
    }

    pub fn skill_tool_available(&self, session: SessionId) -> Result<bool, EngineError> {
        let projection = self.inner.store.get(session)?;
        let policy = permissions::governing_agent_for_skills(&projection);
        let grants = self.skill_grants_for_session(session);
        Ok(self.inner.skills.skills().any(|(name, skill)| {
            model_skill_visibility(
                &policy,
                &projection.permission_overlay,
                grants.as_ref(),
                self.inner.store.cwd(),
                name,
                skill,
            )
            .1
        }))
    }

    pub(crate) fn skill_grants_for_session(
        &self,
        session: SessionId,
    ) -> Option<SessionPermissionOverlay> {
        self.inner
            .skill_grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .map(|grants| {
                let rules = grants
                    .values()
                    .flat_map(|grant| {
                        debug_assert!(grant.source.starts_with("skill:"));
                        grant.overlay.rules.iter().cloned()
                    })
                    .collect();
                SessionPermissionOverlay { rules }
            })
    }

    pub fn list_skills(&self, session: SessionId) -> Result<SkillsListResult, EngineError> {
        let projection = self.inner.store.get(session)?;
        let policy = permissions::governing_agent_for_skills(&projection);
        let grants = self.skill_grants_for_session(session);
        let mut skills = self
            .inner
            .skills
            .discoveries()
            .iter()
            .filter_map(|discovery| {
                let skill = self.inner.skills.get(&discovery.name)?;
                let (effect, visible) = model_skill_visibility(
                    &policy,
                    &projection.permission_overlay,
                    grants.as_ref(),
                    self.inner.store.cwd(),
                    &discovery.name,
                    skill,
                );
                let mut descriptor =
                    skill_descriptor(skill, discovery.precedence_winner, effect, visible);
                descriptor.location = discovery.path.to_string_lossy().into_owned();
                descriptor.source = discovery.source;
                Some(descriptor)
            })
            .collect::<Vec<_>>();
        skills.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.location.cmp(&right.location))
        });
        Ok(SkillsListResult { skills })
    }

    pub fn get_skill(
        &self,
        session: SessionId,
        name: &str,
        args: &str,
    ) -> Result<SkillsGetResult, EngineError> {
        let projection = self.inner.store.get(session)?;
        let policy = permissions::governing_agent_for_skills(&projection);
        let skill = self.skill(name)?;
        let effect = permissions::effective_permission_with_overlay(
            &policy,
            Some(&projection.permission_overlay),
            PermissionAction::Skill,
            name,
            self.inner.store.cwd(),
        )
        .0;
        if effect == PermissionEffect::Deny {
            return Err(EngineError::Permission(format!("skill `{name}` is denied")));
        }
        Ok(SkillsGetResult {
            skill: skill_descriptor(skill, true, effect, true),
            rendered: render_skill(skill, args),
        })
    }

    pub fn get_user_skill(
        &self,
        session: SessionId,
        name: &str,
        args: &str,
    ) -> Result<SkillsGetResult, EngineError> {
        let result = self.get_skill(session, name, args)?;
        if !result.skill.user_invocable {
            return Err(EngineError::Permission(format!(
                "skill `{name}` is not user-invocable"
            )));
        }
        Ok(result)
    }

    pub fn get_model_skill(
        &self,
        session: SessionId,
        name: &str,
        args: &str,
    ) -> Result<SkillsGetResult, EngineError> {
        let projection = self.inner.store.get(session)?;
        let policy = permissions::governing_agent_for_skills(&projection);
        let grants = self.skill_grants_for_session(session);
        let visible = |skill_name: &str, skill: &SkillDocument| {
            model_skill_visibility(
                &policy,
                &projection.permission_overlay,
                grants.as_ref(),
                self.inner.store.cwd(),
                skill_name,
                skill,
            )
            .1
        };
        let Some(skill) = self
            .inner
            .skills
            .get(name)
            .filter(|skill| visible(name, skill))
        else {
            let hints = self
                .inner
                .skills
                .skills()
                .filter(|(skill_name, skill)| visible(skill_name, skill))
                .map(|(skill_name, _)| skill_name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(EngineError::MissingTool(format!(
                "unknown or unavailable model skill `{name}`; valid skills: {hints}"
            )));
        };
        let effect = model_skill_visibility(
            &policy,
            &projection.permission_overlay,
            grants.as_ref(),
            self.inner.store.cwd(),
            name,
            skill,
        )
        .0;
        Ok(SkillsGetResult {
            skill: skill_descriptor(skill, true, effect, true),
            rendered: render_skill(skill, args),
        })
    }

    pub async fn invoke_skill(
        &self,
        session: SessionId,
        run: Option<cookie_agent_protocol::RunId>,
        name: &str,
        args: &str,
        permission_preapproved: bool,
    ) -> Result<SkillInvocation, EngineError> {
        let projection = self.inner.store.get(session)?;
        let policy = permissions::governing_agent_for_skills(&projection);
        let skill = self.skill(name)?.clone();
        if !permission_preapproved && !skill.frontmatter.user_invocable {
            return Err(EngineError::Permission(format!(
                "skill `{name}` is not user-invocable"
            )));
        }
        let effect = permissions::effective_permission_with_overlay(
            &policy,
            Some(&projection.permission_overlay),
            PermissionAction::Skill,
            name,
            self.inner.store.cwd(),
        )
        .0;
        if !permission_preapproved && effect != PermissionEffect::Allow {
            return Err(EngineError::Permission(format!(
                "skill `{name}` requires {}",
                if effect == PermissionEffect::Deny {
                    "permission that is denied"
                } else {
                    "approval"
                }
            )));
        }
        let plan = prepared_skill_invocation(&skill, args);
        self.install_prepared_skill(session, run, &plan).await
    }

    pub(super) async fn install_prepared_skill(
        &self,
        session: SessionId,
        run: Option<RunId>,
        plan: &PreparedSkillInvocation,
    ) -> Result<SkillInvocation, EngineError> {
        let projection = self.inner.store.get(session)?;
        let name = &plan.name;
        let duplicate = projection.log.event_snapshot().iter().any(|event| {
            matches!(
                &event.payload,
                EventPayload::SkillLoaded { name: prior_name, rendered_body, .. }
                    if prior_name == name && rendered_body.as_bytes() == plan.rendered.as_bytes()
            )
        });
        let event = if duplicate {
            EventPayload::SkillInvocationNoted { name: name.clone() }
        } else {
            EventPayload::SkillLoaded {
                name: name.clone(),
                rendered_body: plan.rendered.clone(),
                source_path: plan.source_path.clone(),
                args: plan.args.clone(),
                base_dir: plan.base_dir.clone(),
                supporting_files: plan.supporting_files.clone(),
            }
        };
        self.append(session, run, super::event_origin("engine:skills"), event)
            .await?;
        let mut overlays = self
            .inner
            .skill_grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let per_skill = overlays.entry(session).or_default();
        if plan.grants.is_empty() {
            per_skill.remove(name);
        } else {
            per_skill.insert(
                name.clone(),
                SkillGrantOverlay {
                    source: format!("skill:{name}"),
                    overlay: SessionPermissionOverlay {
                        rules: plan.grants.clone(),
                    },
                },
            );
        }
        if per_skill.is_empty() {
            overlays.remove(&session);
        }
        drop(overlays);
        if let Some(model) = &plan.model {
            self.inner
                .skill_models
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(session, model.clone());
        }
        Ok(SkillInvocation {
            name: name.clone(),
            rendered: plan.rendered.clone(),
            context: plan.context,
        })
    }

    pub fn skill_invocation_context(
        &self,
        name: &str,
    ) -> Result<Option<SkillContext>, EngineError> {
        Ok(self.skill(name)?.frontmatter.context)
    }

    pub fn is_direct_skill_call(&self, call_id: ToolCallId) -> bool {
        self.inner
            .direct_skill_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&call_id)
    }

    #[cfg(test)]
    pub(crate) fn stage_skill_fork_for_test(
        &self,
        call_id: ToolCallId,
        payload: &cookie_agent_protocol::StagedSkillPayload,
    ) {
        self.inner
            .pending_skill_forks
            .lock()
            .expect("pending skill forks lock poisoned")
            .insert(call_id, prepared_skill_from_payload(payload));
    }

    pub(super) async fn execute_direct_skill(
        &self,
        active: Arc<ActiveRun>,
        run: RunId,
        name: String,
        args: String,
    ) -> Result<(), EngineError> {
        self.get_user_skill(active.session, &name, &args)?;
        let call_id = ToolCallId::new_v7();
        self.inner
            .direct_skill_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(call_id);
        let result = self
            .execute_prepared_call(
                active,
                run,
                ToolCall {
                    id: call_id,
                    name: "skill".into(),
                    arguments: serde_json::json!({"name": name, "args": args}),
                },
            )
            .await;
        self.inner
            .direct_skill_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&call_id);
        result.map(|_| ()).map_err(|error| {
            if error.to_string().contains("tool_denied") {
                EngineError::Permission(error.to_string())
            } else {
                error
            }
        })
    }

    pub async fn execute_skill_fork(
        &self,
        session: SessionId,
        run: RunId,
        call_id: ToolCallId,
        name: &str,
        args: &str,
    ) -> Result<cookie_agent_protocol::PersistedToolResult, EngineError> {
        let plan = self.prepare_skill_invocation(name, args)?;
        let agent_type = self
            .delegate_targets(session)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                EngineError::MissingTool(
                    "fork skill requires an available delegation target".into(),
                )
            })?;
        self.inner
            .pending_skill_forks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(call_id, plan.clone());
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run)
            .cloned()
            .filter(|active| active.session == session)
            .ok_or(EngineError::MissingRun(run))?;
        let result = self
            .execute_prepared_call(
                active,
                run,
                ToolCall {
                    id: call_id,
                    name: "delegate_subagent".into(),
                    arguments: serde_json::json!({
                        "description": format!("Skill {name}"),
                        "prompt": format!("Apply the staged skill `{name}`."),
                        "agent_type": agent_type,
                        "background": false,
                        "resume_session_id": null,
                        "inherit_context": false,
                    }),
                },
            )
            .await;
        self.inner
            .pending_skill_forks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&call_id);
        result
    }

    async fn execute_prepared_call(
        &self,
        active: Arc<ActiveRun>,
        run: RunId,
        call: ToolCall,
    ) -> Result<cookie_agent_protocol::PersistedToolResult, EngineError> {
        let binding = active
            .policy
            .selected_suffix
            .first()
            .ok_or(EngineError::NoRunnableModel)?;
        let turn_context = Arc::new(TurnAgentContext {
            agent: active.policy.agent.agent.clone(),
            model: binding.selection.model.clone(),
            adapter: crate::policy::wire_adapter(binding.protocol_recipe.as_str()),
            adapter_family: crate::policy::adapter_family(binding.protocol_recipe.as_str()),
            capabilities: active
                .policy
                .model_capabilities(binding)
                .ok_or(EngineError::NoRunnableModel)?,
        });
        let prepared = self
            .prepare_tool_call(
                active.session,
                run,
                call,
                &active.policy,
                turn_context.clone(),
            )
            .await;
        self.execute_tool(active, run, prepared, turn_context)
            .await
            .map_err(|failure| EngineError::MissingTool(failure.message))
    }

    fn prepare_skill_invocation(
        &self,
        name: &str,
        args: &str,
    ) -> Result<PreparedSkillInvocation, EngineError> {
        let skill = self.skill(name)?.clone();
        Ok(prepared_skill_invocation(&skill, args))
    }

    pub(super) fn prepare_user_skill_invocation(
        &self,
        name: &str,
        args: &str,
    ) -> Result<PreparedSkillInvocation, EngineError> {
        let skill = self.skill(name)?.clone();
        if !skill.frontmatter.user_invocable {
            return Err(EngineError::Permission(format!(
                "skill `{name}` is not user-invocable"
            )));
        }
        Ok(prepared_skill_invocation(&skill, args))
    }

    pub(crate) fn stage_child_skill_from_event(
        &self,
        session: SessionId,
        payload: &cookie_agent_protocol::StagedSkillPayload,
    ) {
        self.inner
            .pending_child_skills
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session, prepared_skill_from_payload(payload));
    }

    pub(super) fn pending_child_skill(
        &self,
        session: SessionId,
    ) -> Option<PreparedSkillInvocation> {
        self.inner
            .pending_child_skills
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .cloned()
    }

    pub(super) fn take_pending_child_skill(
        &self,
        session: SessionId,
    ) -> Option<PreparedSkillInvocation> {
        self.inner
            .pending_child_skills
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session)
    }

    pub(crate) fn transfer_pending_skill_fork(
        &self,
        call_id: ToolCallId,
        _child_session: SessionId,
    ) {
        self.inner
            .pending_skill_forks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&call_id);
    }

    pub fn take_skill_model_override(
        &self,
        session: SessionId,
    ) -> Option<cookie_agent_protocol::ModelKey> {
        self.inner
            .skill_models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session)
    }

    pub(crate) fn clear_skill_turn_state(&self, session: SessionId) {
        self.inner
            .skill_grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session);
        self.inner
            .skill_models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session);
    }

    fn skill(&self, name: &str) -> Result<&SkillDocument, EngineError> {
        self.inner.skills.get(name).ok_or_else(|| {
            let names = self
                .inner
                .skills
                .skills()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            EngineError::MissingTool(format!("unknown skill `{name}`; valid skills: {names}"))
        })
    }

    pub(crate) fn compose_skill_listing(
        &self,
        session: SessionId,
        policy: &mut FrozenRunPolicy,
        prospective_grants: Option<&SessionPermissionOverlay>,
    ) -> Result<(), EngineError> {
        let projection = self.inner.store.get(session)?;
        let visible = self
            .inner
            .skills
            .skills()
            .filter_map(|(name, skill)| {
                model_skill_visibility(
                    &policy.agent,
                    &projection.permission_overlay,
                    prospective_grants,
                    self.inner.store.cwd(),
                    name,
                    skill,
                )
                .1
                .then_some(skill)
            })
            .collect::<Vec<_>>();
        if visible.is_empty() {
            return Ok(());
        }
        let context_tokens = policy.selected_suffix[0]
            .descriptor
            .capabilities
            .limits
            .context
            .unwrap_or(8_192);
        let listing = cookie_agent_config::render_available_skills(visible, context_tokens)
            .map_err(|error| EngineError::Config(Box::new(error)))?;
        policy.agent.composed_prompt.push('\n');
        policy.agent.composed_prompt.push_str(&listing);
        policy.agent.composed_prompt.push('\n');
        policy.agent.prompt_fingerprint =
            Sha256Digest::of_bytes(policy.agent.composed_prompt.as_bytes());
        let mut document_fingerprint = policy
            .agent
            .document_fingerprint
            .as_str()
            .as_bytes()
            .to_vec();
        document_fingerprint.extend_from_slice(listing.as_bytes());
        policy.agent.document_fingerprint = Sha256Digest::of_bytes(&document_fingerprint);
        Ok(())
    }
}

fn skill_descriptor(
    skill: &SkillDocument,
    precedence_winner: bool,
    permission_effect: PermissionEffect,
    visible: bool,
) -> SkillDescriptor {
    SkillDescriptor {
        name: skill.frontmatter.name.clone(),
        description: skill.frontmatter.description.clone(),
        when_to_use: skill.frontmatter.when_to_use.clone(),
        location: skill.path.to_string_lossy().into_owned(),
        source: skill.source,
        precedence_winner,
        permission_effect,
        visible,
        user_invocable: skill.frontmatter.user_invocable,
        argument_hint: skill.frontmatter.argument_hint.clone(),
    }
}

fn model_skill_visibility(
    policy: &cookie_agent_protocol::AgentSnapshot,
    overlay: &SessionPermissionOverlay,
    grants: Option<&SessionPermissionOverlay>,
    workspace: &Path,
    name: &str,
    skill: &SkillDocument,
) -> (PermissionEffect, bool) {
    let effect = permissions::effective_permission_with_overlay(
        policy,
        Some(overlay),
        PermissionAction::Skill,
        name,
        workspace,
    )
    .0;
    let action_visible = permissions::PermissionPipeline::tool_visible_with_grants(
        policy,
        Some(overlay),
        grants,
        "skill",
        workspace,
    );
    (
        effect,
        action_visible
            && effect != PermissionEffect::Deny
            && !skill.frontmatter.disable_model_invocation,
    )
}

fn prepared_skill_invocation(skill: &SkillDocument, args: &str) -> PreparedSkillInvocation {
    PreparedSkillInvocation {
        name: skill.frontmatter.name.clone(),
        args: args.to_owned(),
        rendered: render_skill(skill, args),
        source_path: skill.path.to_string_lossy().into_owned(),
        base_dir: skill.base_dir.to_string_lossy().into_owned(),
        supporting_files: skill
            .supporting_files
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        grants: skill
            .frontmatter
            .allowed_tools
            .iter()
            .map(|grant| PermissionRule {
                action: grant.action,
                resource: grant.pattern.clone(),
                effect: PermissionEffect::Allow,
            })
            .collect(),
        model: skill.frontmatter.model.clone(),
        context: skill.frontmatter.context,
    }
}

fn prepared_skill_from_payload(
    payload: &cookie_agent_protocol::StagedSkillPayload,
) -> PreparedSkillInvocation {
    PreparedSkillInvocation {
        name: payload.name.clone(),
        args: payload.args.clone(),
        rendered: payload.rendered_body.clone(),
        source_path: payload.source_path.clone(),
        base_dir: payload.base_dir.clone(),
        supporting_files: payload.supporting_files.clone(),
        grants: payload.grants.clone(),
        model: payload.model.clone(),
        context: Some(SkillContext::Fork),
    }
}

fn render_skill(skill: &SkillDocument, args: &str) -> String {
    let positional = args.split_whitespace().collect::<Vec<_>>();
    let mut variables = BTreeMap::new();
    variables.insert("ARGUMENTS".to_owned(), args.to_owned());
    variables.insert(
        "{COOKIE_SKILL_DIR}".to_owned(),
        skill.base_dir.to_string_lossy().into_owned(),
    );
    for (index, value) in positional.into_iter().enumerate() {
        variables.insert((index + 1).to_string(), value.to_owned());
    }
    let mut output = String::new();
    let bytes = skill.body.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'$' {
            let rest = &skill.body[index + 1..];
            let key = if rest.starts_with("{COOKIE_SKILL_DIR}") {
                Some("{COOKIE_SKILL_DIR}")
            } else if rest.starts_with("ARGUMENTS") {
                Some("ARGUMENTS")
            } else {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                (digits > 0).then_some(&rest[..digits])
            };
            if let Some(key) = key {
                output.push_str(variables.get(key).map_or("", String::as_str));
                index += 1 + key.len();
                continue;
            }
        }
        let character = skill.body[index..]
            .chars()
            .next()
            .expect("character boundary");
        output.push(character);
        index += character.len_utf8();
    }
    output
}
