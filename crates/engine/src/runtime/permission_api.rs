//! Session permissions as clients reach them: previews of a new session's
//! effective permissions, and reading and changing a live session's
//! overlay. The rules themselves are `cookie_agent_engine_permissions`.

use std::collections::HashSet;

use cookie_agent_protocol::*;

use super::{Engine, EngineError};
use crate::permissions::*;

impl Engine {
    /// The effective permissions a session created with `selection` and the
    /// overlay `rules` would have, computed exactly as for a live session.
    pub async fn preview_session_permissions(
        &self,
        selection: &RunSelection,
        rules: Vec<PermissionRule>,
    ) -> Result<Vec<EffectivePermissionAction>, EngineError> {
        check_rule_selectors(&rules).map_err(EngineError::Permission)?;
        let overlay = SessionPermissionOverlay { rules };
        overlay
            .validate()
            .map_err(|error| EngineError::Permission(error.to_string()))?;
        self.reconcile_provider_store_off_worker().await?;
        let (_, policy) = self.freeze_selection_policy(selection)?;
        Ok(effective_permission_view(
            &policy.agent,
            false,
            &overlay,
            &[],
        ))
    }

    pub fn get_session_permissions(
        &self,
        session_id: SessionId,
    ) -> Result<SessionPermissionGetResult, EngineError> {
        let session = self.inner.store.get(session_id)?;
        let policy = governing_agent(&session);
        let declared = self.declared_permission_actions(session_id)?;
        let delegated = matches!(session.meta.origin, SessionOrigin::Delegated { .. });
        Ok(SessionPermissionGetResult {
            permissions: effective_permission_view(
                &policy,
                delegated,
                &session.permission_overlay,
                &declared,
            ),
            current_mode: Some(self.permission_mode(session_id)),
        })
    }

    /// The actions the tools of every source of `session` are checked under,
    /// visible or not, in source order.
    fn declared_permission_actions(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PermissionAction>, EngineError> {
        let context = cookie_agent_runtime_api::SessionToolContext::new(session_id);
        let mut actions = Vec::new();
        for source in self.session_tool_sources(session_id)? {
            for tool in source
                .provider
                .tools_for_session(&context)
                .map_err(|error| EngineError::ToolFailed(error.to_string()))?
            {
                if let Some(Ok(action)) = tool
                    .permission_name
                    .as_deref()
                    .map(PermissionPipeline::action_for_permission_name)
                    && !actions.contains(&action)
                {
                    actions.push(action);
                }
            }
        }
        Ok(actions)
    }

    /// Sets the overlay rule for `action` (qualified by `runtime` when given)
    /// on `resource`. A change that makes the rule stricter for any runtime
    /// the session uses invalidates that action's tree grants there.
    pub async fn set_session_permission(
        &self,
        session_id: SessionId,
        action: &PermissionAction,
        runtime: Option<RuntimeId>,
        resource: WildcardPattern,
        effect: PermissionEffect,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<SessionPermissionMutationResult, EngineError> {
        check_rule_selectors(&[PermissionRule {
            action: action.clone(),
            runtime: runtime.clone(),
            resource: resource.clone(),
            effect,
        }])
        .map_err(EngineError::Permission)?;
        let _mutation = self
            .inner
            .approvals
            .permission_overlay_mutation
            .lock()
            .await;
        let session = self.inner.store.get(session_id)?;
        let policy = governing_agent(&session);
        let scopes = self.overlay_effect_scopes(&session, runtime.as_ref());
        let mut overlay = session.permission_overlay.clone();
        let old_effects = rule_effects(&policy, &overlay, action, resource.as_str(), &scopes);
        overlay.rules.retain(|rule| {
            rule.action != *action || rule.runtime != runtime || rule.resource != resource
        });
        overlay.rules.push(PermissionRule {
            action: action.clone(),
            runtime: runtime.clone(),
            resource: resource.clone(),
            effect,
        });
        overlay
            .validate()
            .map_err(|error| EngineError::Permission(error.to_string()))?;
        let new_effects = rule_effects(&policy, &overlay, action, resource.as_str(), &scopes);
        if stricter(&old_effects, &new_effects) {
            self.invalidate_action_grants(session_id, &session.meta.origin, action, runtime)?;
        }
        self.append(
            session_id,
            None,
            origin,
            EventPayload::SessionPermissionOverlaySet { overlay },
        )
        .await?;
        let result = self.get_session_permissions(session_id)?;
        Ok(SessionPermissionMutationResult {
            permissions: result.permissions,
        })
    }

    /// Removes the overlay rule for `action` (qualified by `runtime` when
    /// given) on `resource`.
    pub async fn clear_session_permission(
        &self,
        session_id: SessionId,
        action: &PermissionAction,
        runtime: Option<RuntimeId>,
        resource: &WildcardPattern,
        origin: cookie_agent_protocol::EventOrigin,
    ) -> Result<SessionPermissionMutationResult, EngineError> {
        let _mutation = self
            .inner
            .approvals
            .permission_overlay_mutation
            .lock()
            .await;
        let session = self.inner.store.get(session_id)?;
        let policy = governing_agent(&session);
        let scopes = self.overlay_effect_scopes(&session, runtime.as_ref());
        let mut overlay = session.permission_overlay.clone();
        let old_effects = rule_effects(&policy, &overlay, action, resource.as_str(), &scopes);
        let prior_len = overlay.rules.len();
        overlay.rules.retain(|rule| {
            rule.action != *action || rule.runtime != runtime || &rule.resource != resource
        });
        if overlay.rules.len() != prior_len {
            let new_effects = rule_effects(&policy, &overlay, action, resource.as_str(), &scopes);
            if stricter(&old_effects, &new_effects) {
                self.invalidate_action_grants(session_id, &session.meta.origin, action, runtime)?;
            }
            self.append(
                session_id,
                None,
                origin,
                EventPayload::SessionPermissionOverlaySet { overlay },
            )
            .await?;
        }
        let result = self.get_session_permissions(session_id)?;
        Ok(SessionPermissionMutationResult {
            permissions: result.permissions,
        })
    }

    /// The scopes an overlay rule can affect in `session`: for a rule
    /// qualified by `runtime`, each attachment of that runtime ID (or the
    /// runtime without a workspace when none is attached); for an
    /// unqualified rule, the base layer and every attachment.
    fn overlay_effect_scopes(
        &self,
        session: &cookie_agent_engine_store::session::SessionProjection,
        runtime: Option<&RuntimeId>,
    ) -> Vec<OwnedScope> {
        let attachments = self.attachments_of(session);
        let mut scopes = attachments
            .iter()
            .filter(|attachment| runtime.is_none_or(|runtime| &attachment.runtime_id == runtime))
            .map(|attachment| OwnedScope {
                runtime: Some(attachment.runtime_id.as_str().to_owned()),
                workspace: attachment
                    .workspace
                    .as_deref()
                    .map(std::path::PathBuf::from),
                home: self.attachment_home(attachment),
                platform: self.attachment_platform(attachment),
            })
            .collect::<Vec<_>>();
        match runtime {
            None => scopes.push(OwnedScope::default()),
            Some(runtime) if scopes.is_empty() => {
                scopes.push(OwnedScope {
                    runtime: Some(runtime.as_str().to_owned()),
                    workspace: None,
                    home: self.runtime_home(runtime.as_str()),
                    platform: self.runtime_platform(runtime.as_str()),
                });
            }
            Some(_) => {}
        }
        scopes
    }

    /// The home directory the runtime of `attachment` reported: its running
    /// instance's, else the one its runtime last reported.
    fn attachment_home(
        &self,
        attachment: &cookie_agent_protocol::RuntimeAttachment,
    ) -> Option<String> {
        self.inner
            .runtimes
            .instance(attachment)
            .and_then(|instance| instance.info().home().map(str::to_owned))
            .or_else(|| self.runtime_home(attachment.runtime_id.as_str()))
    }

    /// The platform the runtime of `attachment` reported: its running
    /// instance's workspace's, else the one its runtime last reported.
    fn attachment_platform(
        &self,
        attachment: &cookie_agent_protocol::RuntimeAttachment,
    ) -> Option<String> {
        self.inner
            .runtimes
            .instance(attachment)
            .and_then(|instance| {
                instance
                    .info()
                    .workspace()
                    .and_then(|workspace| workspace.platform().map(str::to_owned))
            })
            .or_else(|| self.runtime_platform(attachment.runtime_id.as_str()))
    }

    /// The platform the runtime with ID `runtime_id` last reported, whose
    /// paths its permission resources are.
    pub fn runtime_platform(&self, runtime_id: &str) -> Option<String> {
        self.inner
            .runtimes
            .templates()
            .into_iter()
            .find(|template| template.runtime_id == runtime_id)
            .and_then(|template| template.listing.platform)
    }

    /// The home directory the runtime with ID `runtime_id` last reported.
    pub fn runtime_home(&self, runtime_id: &str) -> Option<String> {
        self.inner
            .runtimes
            .templates()
            .into_iter()
            .find(|template| template.runtime_id == runtime_id)
            .and_then(|template| template.listing.home)
    }

    fn invalidate_action_grants(
        &self,
        session_id: SessionId,
        origin: &SessionOrigin,
        action: &PermissionAction,
        runtime: Option<RuntimeId>,
    ) -> Result<(), EngineError> {
        let root = match origin {
            SessionOrigin::Root => session_id,
            SessionOrigin::Delegated {
                root_session_id, ..
            } => *root_session_id,
        };
        let grants = self
            .inner
            .approvals
            .store
            .for_root(root)
            .into_iter()
            .filter(|grant| {
                runtime
                    .as_ref()
                    .is_none_or(|runtime| grant.runtime_id.as_ref() == Some(runtime))
                    && grant
                        .resources
                        .iter()
                        .any(|resource| resource.capability == *action)
            })
            .collect::<Vec<_>>();
        let ids = grants
            .iter()
            .map(|grant| grant.grant_id)
            .collect::<HashSet<_>>();
        if ids.is_empty() {
            return Ok(());
        }
        let digests = grants
            .iter()
            .flat_map(|grant| grant.resources.iter())
            .filter(|resource| resource.capability == *action)
            .map(|resource| resource.binding_digest.digest().clone())
            .collect();
        self.inner.grant_journals.for_root(root)?.invalidate(
            root,
            ids.iter().copied().collect(),
            digests,
        )?;
        self.inner.approvals.store.invalidate_grants(&ids);
        Ok(())
    }
}
