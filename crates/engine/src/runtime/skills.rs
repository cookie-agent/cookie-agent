use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use cookie_agent_config::{SkillContext, SkillDocument, SkillRegistry};
use cookie_agent_protocol::{
    AttachmentName, EventPayload, PermissionAction, PermissionEffect, PermissionRule, RunId,
    SessionId, SessionPermissionOverlay, SkillDescriptor, SkillsGetResult, SkillsListResult,
    ToolCallId,
};

use super::{ActiveRun, Engine, EngineError};
use crate::{ToolCall, TurnAgentContext};
use crate::{permissions, policy::FrozenRunPolicy};
pub const RESERVED_STAGED_SKILL_PREFIX: &str = "\0cookie-staged-skill:";

/// Skill runtime state owned by [`super::Inner`].
#[derive(Default)]
pub(crate) struct SkillRuntimeState {
    pub(crate) grants: Mutex<HashMap<SessionId, BTreeMap<String, SkillGrantOverlay>>>,
    pub(crate) models: Mutex<HashMap<SessionId, cookie_agent_protocol::ModelKey>>,
    pub(crate) pending_forks: Mutex<HashMap<ToolCallId, PreparedSkillInvocation>>,
    pub(crate) pending_child: Mutex<HashMap<SessionId, PreparedSkillInvocation>>,
    pub(crate) direct_calls: Mutex<HashSet<ToolCallId>>,
}

#[derive(Clone, Debug)]
pub struct SkillInvocation {
    pub name: String,
    pub rendered: String,
    pub context: Option<SkillContext>,
}

#[derive(Clone, Debug)]
pub(crate) struct SkillGrantOverlay {
    source: String,
    /// The attachment that contributed the skill; its grants apply only to
    /// that attachment's tools. `None` for user skills, whose grants apply
    /// everywhere.
    attachment: Option<AttachmentName>,
    overlay: SessionPermissionOverlay,
}

/// One skill as a session sees it.
#[derive(Clone)]
pub(crate) struct SessionSkill {
    registry: Arc<SkillRegistry>,
    /// The skill's own name, its key in `registry`.
    key: String,
    /// The attachment that contributed a project skill; `None` for a user
    /// skill.
    attachment: Option<AttachmentName>,
    /// The workspace of the attachment that contributed a project skill.
    workspace: Option<PathBuf>,
}

impl SessionSkill {
    fn document(&self) -> &SkillDocument {
        self.registry
            .get(&self.key)
            .expect("session skills name registry winners")
    }
}

/// One discovered skill file, as `skills.list` reports it.
struct SessionSkillDiscovery {
    name: String,
    source: cookie_agent_protocol::SkillSource,
    path: PathBuf,
    precedence_winner: bool,
}

/// The skills of a session: user skills from the global runtime and project
/// skills from each coding runtime it is attached to. Project skills shadow
/// user skills of the same name; same-named project skills of several
/// attachments are each named `<attachment>_<name>`.
pub(crate) struct SessionSkills {
    skills: BTreeMap<String, SessionSkill>,
    discoveries: Vec<SessionSkillDiscovery>,
}

impl SessionSkills {
    fn get(&self, name: &str) -> Option<&SessionSkill> {
        self.skills.get(name)
    }

    fn iter(&self) -> impl Iterator<Item = (&String, &SessionSkill)> {
        self.skills.iter()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedSkillInvocation {
    name: String,
    /// The attachment that contributed a project skill; `None` for a user
    /// skill.
    attachment: Option<AttachmentName>,
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
            attachment: self.attachment.clone(),
        }
    }
}

impl Engine {
    #[must_use]
    pub fn is_reserved_staged_skill_prompt(prompt: &str) -> bool {
        prompt.starts_with(RESERVED_STAGED_SKILL_PREFIX)
    }

    /// The skills `session` sees through its attachments.
    pub(crate) fn session_skills(&self, session: SessionId) -> Result<SessionSkills, EngineError> {
        let mut user = Vec::new();
        let mut project = Vec::new();
        for attachment in self.session_runtimes(session)? {
            let Some(instance) = self.inner.runtimes.instance(&attachment) else {
                continue;
            };
            let registry = instance.skills();
            // The daemon's own global runtime carries the user skills; every
            // other attachment's skills are its project skills.
            if attachment.runtime_id.as_str() == cookie_agent_protocol::RuntimeId::GLOBAL {
                user.push(registry);
            } else {
                let workspace = instance
                    .info()
                    .workspace()
                    .map(|workspace| workspace.root().to_owned());
                project.push((attachment.name, registry, workspace));
            }
        }
        let mut offered = HashMap::<String, usize>::new();
        for (_, registry, _) in &project {
            for (name, _) in registry.skills() {
                *offered.entry(name.clone()).or_default() += 1;
            }
        }
        let published = |attachment: &AttachmentName, name: &str| {
            if offered.get(name).is_some_and(|count| *count > 1) {
                format!("{attachment}_{name}")
            } else {
                name.to_owned()
            }
        };
        let mut skills = BTreeMap::new();
        let mut discoveries = Vec::new();
        for (attachment, registry, workspace) in &project {
            for (name, _) in registry.skills() {
                skills.insert(
                    published(attachment, name),
                    SessionSkill {
                        registry: Arc::clone(registry),
                        key: name.clone(),
                        attachment: Some(attachment.clone()),
                        workspace: workspace.clone(),
                    },
                );
            }
            discoveries.extend(registry.discoveries().iter().map(|discovery| {
                SessionSkillDiscovery {
                    name: published(attachment, &discovery.name),
                    source: discovery.source,
                    path: discovery.path.clone(),
                    precedence_winner: discovery.precedence_winner,
                }
            }));
        }
        for registry in &user {
            for (name, _) in registry.skills() {
                if offered.contains_key(name) {
                    continue;
                }
                skills.insert(
                    name.clone(),
                    SessionSkill {
                        registry: Arc::clone(registry),
                        key: name.clone(),
                        attachment: None,
                        workspace: None,
                    },
                );
            }
            discoveries.extend(registry.discoveries().iter().map(|discovery| {
                SessionSkillDiscovery {
                    name: discovery.name.clone(),
                    source: discovery.source,
                    path: discovery.path.clone(),
                    precedence_winner: discovery.precedence_winner
                        && !offered.contains_key(&discovery.name),
                }
            }));
        }
        Ok(SessionSkills {
            skills,
            discoveries,
        })
    }

    pub fn skill_tool_available(&self, session: SessionId) -> Result<bool, EngineError> {
        let projection = self.inner.store.get(session)?;
        let policy = permissions::governing_agent_for_skills(&projection);
        let grants = self.skill_grants_for_session(session, None);
        let view = self.session_skills(session)?;
        if view.skills.is_empty() {
            return Ok(false);
        }
        let merged = skill_rules(&policy, &projection.permission_overlay);
        // Without the `skill` action offered or granted, no skill is
        // visible, whatever each skill's rules say.
        let grants_skill = grants.as_ref().is_some_and(|grants| {
            grants.rules.iter().any(|grant| {
                grant.action == PermissionAction::SKILL
                    && grant.effect == PermissionEffect::Allow
                    && grant.applies_to(None)
            })
        });
        if !grants_skill && !merged.offers(&PermissionAction::SKILL, None) {
            return Ok(false);
        }
        Ok(view
            .iter()
            .any(|(name, skill)| model_skill_visibility(&merged, grants.as_ref(), name, skill).1))
    }

    /// The turn-scoped skill grants that apply to tools of `attachment`
    /// (`None` for the base layer): every user skill's grants, and the
    /// grants of project skills that attachment contributed. A project
    /// skill's grants for the base layer's own actions (`skill`, `delegate`,
    /// `message`) apply to the base layer, which has no attachment.
    pub(crate) fn skill_grants_for_session(
        &self,
        session: SessionId,
        attachment: Option<&AttachmentName>,
    ) -> Option<SessionPermissionOverlay> {
        self.inner
            .skills_runtime
            .grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .map(|grants| {
                let rules = grants
                    .values()
                    .flat_map(|grant| {
                        debug_assert!(grant.source.starts_with("skill:"));
                        grant.overlay.rules.iter().filter(move |rule| {
                            match (&grant.attachment, attachment) {
                                (None, _) => true,
                                (Some(owner), Some(attachment)) => owner == attachment,
                                (Some(_), None) => rule.action.is_base_layer(),
                            }
                        })
                    })
                    .cloned()
                    .collect();
                SessionPermissionOverlay { rules }
            })
    }

    /// The daemon's own skills, without a session: the global runtime's
    /// user and official skills. `permission_effect` is `ask` and `visible`
    /// says whether the skill wins its name, since no agent decides them.
    #[must_use]
    pub fn list_daemon_skills(&self) -> SkillsListResult {
        let registry = self.inner.runtimes.global().skills();
        let mut skills = registry
            .discoveries()
            .iter()
            .filter_map(|discovery| {
                let document = registry.get(&discovery.name)?;
                let mut descriptor = skill_descriptor(
                    &discovery.name,
                    document,
                    discovery.precedence_winner,
                    PermissionEffect::Ask,
                    discovery.precedence_winner,
                );
                descriptor.location = discovery.path.to_string_lossy().into_owned();
                descriptor.source = discovery.source;
                Some(descriptor)
            })
            .collect::<Vec<_>>();
        self.mark_official_versions(&mut skills);
        skills.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| right.precedence_winner.cmp(&left.precedence_winner))
                .then_with(|| left.location.cmp(&right.location))
        });
        SkillsListResult { skills }
    }

    /// Gives each official skill of `skills` its version.
    fn mark_official_versions(&self, skills: &mut [SkillDescriptor]) {
        if !skills
            .iter()
            .any(|skill| skill.source == cookie_agent_protocol::SkillSource::Official)
        {
            return;
        }
        let versions = self.official_skills();
        for skill in skills {
            if skill.source == cookie_agent_protocol::SkillSource::Official {
                skill.version = versions.get(&skill.name).cloned();
            }
        }
    }

    pub fn list_skills(&self, session: SessionId) -> Result<SkillsListResult, EngineError> {
        let projection = self.inner.store.get(session)?;
        let policy = permissions::governing_agent_for_skills(&projection);
        let grants = self.skill_grants_for_session(session, None);
        let merged = skill_rules(&policy, &projection.permission_overlay);
        let view = self.session_skills(session)?;
        let mut skills = view
            .discoveries
            .iter()
            .filter_map(|discovery| {
                let skill = view.get(&discovery.name)?;
                let (effect, visible) =
                    model_skill_visibility(&merged, grants.as_ref(), &discovery.name, skill);
                let mut descriptor = skill_descriptor(
                    &discovery.name,
                    skill.document(),
                    discovery.precedence_winner,
                    effect,
                    visible,
                );
                descriptor.location = discovery.path.to_string_lossy().into_owned();
                descriptor.source = discovery.source;
                Some(descriptor)
            })
            .collect::<Vec<_>>();
        self.mark_official_versions(&mut skills);
        // By name, the skill that wins its name first, then by location:
        // the order does not depend on how the platform spells paths.
        skills.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| right.precedence_winner.cmp(&left.precedence_winner))
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
        let view = self.session_skills(session)?;
        let skill = session_skill(&view, name)?;
        let effect = permissions::effective_permission_with_overlay(
            &policy,
            Some(&projection.permission_overlay),
            &PermissionAction::SKILL,
            name,
            skill.workspace.as_deref(),
        )
        .0;
        if effect == PermissionEffect::Deny {
            return Err(EngineError::Permission(format!("skill `{name}` is denied")));
        }
        Ok(SkillsGetResult {
            skill: skill_descriptor(name, skill.document(), true, effect, true),
            rendered: render_skill(skill.document(), args),
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
        let grants = self.skill_grants_for_session(session, None);
        let merged = skill_rules(&policy, &projection.permission_overlay);
        let view = self.session_skills(session)?;
        let visible = |skill_name: &str, skill: &SessionSkill| {
            model_skill_visibility(&merged, grants.as_ref(), skill_name, skill).1
        };
        let Some(skill) = view.get(name).filter(|skill| visible(name, skill)) else {
            let hints = view
                .iter()
                .filter(|(skill_name, skill)| visible(skill_name, skill))
                .map(|(skill_name, _)| skill_name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(EngineError::ToolFailed(format!(
                "unknown or unavailable model skill `{name}`; valid skills: {hints}"
            )));
        };
        let effect = model_skill_visibility(&merged, grants.as_ref(), name, skill).0;
        Ok(SkillsGetResult {
            skill: skill_descriptor(name, skill.document(), true, effect, true),
            rendered: render_skill(skill.document(), args),
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
        let view = self.session_skills(session)?;
        let skill = session_skill(&view, name)?;
        if !permission_preapproved && !skill.document().frontmatter.user_invocable {
            return Err(EngineError::Permission(format!(
                "skill `{name}` is not user-invocable"
            )));
        }
        let effect = permissions::effective_permission_with_overlay(
            &policy,
            Some(&projection.permission_overlay),
            &PermissionAction::SKILL,
            name,
            skill.workspace.as_deref(),
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
        let plan = prepared_skill_invocation(name, skill, args);
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
            .skills_runtime
            .grants
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
                    attachment: plan.attachment.clone(),
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
                .skills_runtime
                .models
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
        session: SessionId,
        name: &str,
    ) -> Result<Option<SkillContext>, EngineError> {
        let view = self.session_skills(session)?;
        Ok(session_skill(&view, name)?.document().frontmatter.context)
    }

    #[cfg(test)]
    pub(crate) fn install_skill_grants_for_test(
        &self,
        session: SessionId,
        name: &str,
        attachment: Option<AttachmentName>,
        rules: Vec<PermissionRule>,
    ) {
        self.inner
            .skills_runtime
            .grants
            .lock()
            .expect("skill grants lock poisoned")
            .entry(session)
            .or_default()
            .insert(
                name.to_owned(),
                SkillGrantOverlay {
                    source: format!("skill:{name}"),
                    attachment,
                    overlay: SessionPermissionOverlay { rules },
                },
            );
    }

    pub fn is_direct_skill_call(&self, call_id: ToolCallId) -> bool {
        self.inner
            .skills_runtime
            .direct_calls
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
            .skills_runtime
            .pending_forks
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
            .skills_runtime
            .direct_calls
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
            .skills_runtime
            .direct_calls
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
        let plan = self.prepare_skill_invocation(session, name, args)?;
        let agent_type = self
            .delegate_targets(session)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                EngineError::ToolFailed("fork skill requires an available delegation target".into())
            })?;
        self.inner
            .skills_runtime
            .pending_forks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(call_id, plan.clone());
        let active = self
            .inner
            .sessions
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
            .skills_runtime
            .pending_forks
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
            adapter: crate::policy::wire_adapter(binding.descriptor.adapter_id.as_str()),
            input_media: crate::policy::tool_input_media(
                binding.descriptor.adapter_id.as_str(),
                &active
                    .policy
                    .model_capabilities(binding)
                    .ok_or(EngineError::NoRunnableModel)?,
            ),
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
        self.execute_tool(active, run, prepared, turn_context, false)
            .await
            .map_err(|failure| EngineError::ToolFailed(failure.message))
    }

    fn prepare_skill_invocation(
        &self,
        session: SessionId,
        name: &str,
        args: &str,
    ) -> Result<PreparedSkillInvocation, EngineError> {
        let view = self.session_skills(session)?;
        Ok(prepared_skill_invocation(
            name,
            session_skill(&view, name)?,
            args,
        ))
    }

    pub(super) fn prepare_user_skill_invocation(
        &self,
        session: SessionId,
        name: &str,
        args: &str,
    ) -> Result<PreparedSkillInvocation, EngineError> {
        let view = self.session_skills(session)?;
        let skill = session_skill(&view, name)?;
        if !skill.document().frontmatter.user_invocable {
            return Err(EngineError::Permission(format!(
                "skill `{name}` is not user-invocable"
            )));
        }
        Ok(prepared_skill_invocation(name, skill, args))
    }

    pub(crate) fn stage_child_skill_from_event(
        &self,
        session: SessionId,
        payload: &cookie_agent_protocol::StagedSkillPayload,
    ) {
        self.inner
            .skills_runtime
            .pending_child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session, prepared_skill_from_payload(payload));
    }

    pub(super) fn pending_child_skill(
        &self,
        session: SessionId,
    ) -> Option<PreparedSkillInvocation> {
        self.inner
            .skills_runtime
            .pending_child
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
            .skills_runtime
            .pending_child
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
            .skills_runtime
            .pending_forks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&call_id);
    }

    pub fn take_skill_model_override(
        &self,
        session: SessionId,
    ) -> Option<cookie_agent_protocol::ModelKey> {
        self.inner
            .skills_runtime
            .models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session)
    }

    pub(crate) fn clear_skill_turn_state(&self, session: SessionId) {
        self.inner
            .skills_runtime
            .grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session);
        self.inner
            .skills_runtime
            .models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session);
    }

    /// The `<available_skills>` listing of the skills a run of `policy` in
    /// `session` may see, sized for a context of `context_tokens`; `None`
    /// when it sees none. The model loop puts it in the system prompt, and
    /// an ACP agent's first prompt carries it.
    pub(crate) fn skill_listing(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
        prospective_grants: Option<&SessionPermissionOverlay>,
        context_tokens: u64,
    ) -> Result<Option<String>, EngineError> {
        let projection = self.inner.store.get(session)?;
        let view = self.session_skills(session)?;
        let merged = skill_rules(&policy.agent, &projection.permission_overlay);
        let visible = view
            .iter()
            .filter_map(|(name, skill)| {
                model_skill_visibility(&merged, prospective_grants, name, skill)
                    .1
                    .then_some((name.as_str(), skill.document()))
            })
            .collect::<Vec<_>>();
        if visible.is_empty() {
            return Ok(None);
        }
        cookie_agent_config::render_available_skills(visible, context_tokens)
            .map(Some)
            .map_err(|error| EngineError::Config(Box::new(error)))
    }

    /// The `skills` prompt block a run of `policy` composes now, sized for
    /// its model's context.
    pub(crate) fn live_skills_block(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
        prospective_grants: Option<&SessionPermissionOverlay>,
    ) -> Result<Option<String>, EngineError> {
        let context_tokens = policy.selected_suffix[0]
            .descriptor
            .capabilities
            .limits
            .context
            .unwrap_or(8_192);
        self.skill_listing(session, policy, prospective_grants, context_tokens)
    }

    pub(crate) fn compose_skill_listing(
        &self,
        session: SessionId,
        policy: &mut FrozenRunPolicy,
        prospective_grants: Option<&SessionPermissionOverlay>,
    ) -> Result<(), EngineError> {
        let Some(listing) = self.live_skills_block(session, policy, prospective_grants)? else {
            return Ok(());
        };
        crate::runtime::prompt_blocks::append_prompt_block(
            policy,
            crate::runtime::prompt_blocks::SKILLS_BLOCK,
            &listing,
        );
        Ok(())
    }
}

/// The skill named `name` in `view`, or an error listing the valid names.
fn session_skill<'a>(view: &'a SessionSkills, name: &str) -> Result<&'a SessionSkill, EngineError> {
    view.get(name).ok_or_else(|| {
        let names = view
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        EngineError::ToolFailed(format!("unknown skill `{name}`; valid skills: {names}"))
    })
}

fn skill_descriptor(
    name: &str,
    skill: &SkillDocument,
    precedence_winner: bool,
    permission_effect: PermissionEffect,
    visible: bool,
) -> SkillDescriptor {
    SkillDescriptor {
        name: name.to_owned(),
        description: skill.frontmatter.description.clone(),
        when_to_use: skill.frontmatter.when_to_use.clone(),
        location: skill.path.to_string_lossy().into_owned(),
        source: skill.source,
        precedence_winner,
        permission_effect,
        visible,
        user_invocable: skill.frontmatter.user_invocable,
        argument_hint: skill.frontmatter.argument_hint.clone(),
        version: None,
    }
}

/// The merged rules skills are decided by: `policy`'s and `overlay`'s, on
/// the base layer, which reports no home.
fn skill_rules<'a>(
    policy: &'a cookie_agent_protocol::AgentSnapshot,
    overlay: &'a SessionPermissionOverlay,
) -> permissions::MergedRules<'a> {
    permissions::MergedRules::of(policy, Some(overlay), None)
}

/// The effect of `skill`, known as `name`, under `merged`
/// ([`skill_rules`]), and whether the model sees it: the `skill` action is
/// offered or granted, the skill is not denied, and it does not opt out of
/// model invocation.
fn model_skill_visibility(
    merged: &permissions::MergedRules<'_>,
    grants: Option<&SessionPermissionOverlay>,
    name: &str,
    skill: &SessionSkill,
) -> (PermissionEffect, bool) {
    let scope = permissions::PermissionScope::from(skill.workspace.as_deref());
    let effect = merged.decide(&PermissionAction::SKILL, name, scope).0;
    let visible = effect != PermissionEffect::Deny
        && !skill.document().frontmatter.disable_model_invocation
        && permissions::PermissionPipeline::tool_visible_by(merged, grants, "skill", scope);
    (effect, visible)
}

/// The plan for invoking `skill`, known to the session as `name`. A project
/// skill's `allowed-tools` grants carry the attachment that contributed it.
fn prepared_skill_invocation(
    name: &str,
    skill: &SessionSkill,
    args: &str,
) -> PreparedSkillInvocation {
    let document = skill.document();
    PreparedSkillInvocation {
        name: name.to_owned(),
        attachment: skill.attachment.clone(),
        args: args.to_owned(),
        rendered: render_skill(document, args),
        source_path: document.path.to_string_lossy().into_owned(),
        base_dir: document.base_dir.to_string_lossy().into_owned(),
        supporting_files: document
            .supporting_files
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        grants: document
            .frontmatter
            .allowed_tools
            .iter()
            .map(|grant| PermissionRule {
                action: grant.action.clone(),
                runtime: None,
                resource: grant.pattern.clone(),
                effect: PermissionEffect::Allow,
            })
            .collect(),
        model: document.frontmatter.model.clone(),
        context: document.frontmatter.context,
    }
}

fn prepared_skill_from_payload(
    payload: &cookie_agent_protocol::StagedSkillPayload,
) -> PreparedSkillInvocation {
    PreparedSkillInvocation {
        name: payload.name.clone(),
        attachment: payload.attachment.clone(),
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
