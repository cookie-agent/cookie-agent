//! `/permissions`: the viewed session's effective rules, edited through
//! session overrides (`session.permission.set/clear`), and, while a new
//! session is composed, its draft rules previewed against its agent
//! (`session.permission.preview`) until `session.create` commits them.

use cookie_agent_client_state::permissions::{PermissionRow, remove_draft_rule, set_draft_rule};
use cookie_agent_protocol::{
    EffectivePermissionAction, PermissionAction, PermissionEffect, PermissionRule, RuntimeId,
    SessionPermissionClearParams, SessionPermissionGetParams, SessionPermissionPreviewParams,
    SessionPermissionSetParams, WildcardPattern,
};
use leptos::{prelude::*, task::spawn_local};

use super::{Panel, Workspace};

impl Workspace {
    /// Opens `/permissions` and reads its rules.
    pub(super) fn open_permissions(self) {
        if !self.new_session.get_untracked() && self.selected.get_untracked().is_none() {
            self.status
                .set("select a session before editing permissions".into());
            return;
        }
        self.permissions.set(None);
        self.panel.set(Some(Panel::Permissions));
        self.load_permissions();
    }

    /// Whether the table edits a new session's draft rather than a session.
    pub fn permissions_draft(self) -> bool {
        self.new_session.get()
    }

    pub(super) fn load_permissions(self) {
        let Some(client) = self.client() else {
            return;
        };
        let request = self.next_permission_request();
        if self.new_session.get_untracked() {
            let Some(selection) = self.draft.get_untracked() else {
                self.permissions
                    .set(Some(Err("pick an agent and a model first".into())));
                return;
            };
            let rules = self.draft_rules.get_untracked();
            spawn_local(async move {
                let result = client
                    .preview_session_permissions(SessionPermissionPreviewParams {
                        selection,
                        rules,
                    })
                    .await
                    .map(|result| result.permissions)
                    .map_err(|error| crate::errors::describe_full(&error));
                self.install_permissions(request, result);
            });
            return;
        }
        let Some(session_id) = self.selected.get_untracked() else {
            return;
        };
        spawn_local(async move {
            let result = client
                .get_session_permissions(SessionPermissionGetParams { session_id })
                .await
                .map(|result| result.permissions)
                .map_err(|error| crate::errors::describe_full(&error));
            self.install_permissions(request, result);
        });
    }

    fn next_permission_request(self) -> u64 {
        self.permission_request
            .update_value(|request| *request += 1);
        self.permission_request.get_value()
    }

    /// Installs an answer unless a newer request was made since.
    fn install_permissions(
        self,
        request: u64,
        result: Result<Vec<EffectivePermissionAction>, String>,
    ) {
        if self.permission_request.get_value() == request {
            self.permissions.set(Some(result));
        }
    }

    /// Sets `effect` for `action` (on `runtime` alone when given) on
    /// `resource`: a session override, or a draft rule. Editing a rule
    /// that changed its key or pattern, `replacing` names the old one,
    /// which the new rule replaces.
    pub fn set_permission(
        self,
        action: PermissionAction,
        runtime: Option<RuntimeId>,
        resource: &str,
        effect: PermissionEffect,
        replacing: Option<RuleRef>,
    ) {
        let Ok(resource) = WildcardPattern::new(resource.trim()) else {
            self.status
                .set("a pattern must be a non-empty wildcard pattern".into());
            return;
        };
        let replacing = replacing.filter(|old| {
            old.action != action || old.runtime != runtime || old.resource != resource
        });
        if self.new_session.get_untracked() {
            self.draft_rules.update(|rules| {
                if let Some(old) = &replacing {
                    remove_draft_rule(rules, &old.action, old.runtime.as_ref(), &old.resource);
                }
                set_draft_rule(
                    rules,
                    PermissionRule {
                        runtime,
                        action,
                        resource,
                        effect,
                    },
                );
            });
            self.load_permissions();
            return;
        }
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        if !self.writable() {
            self.status
                .set("cannot change permissions of a read-only session".into());
            return;
        }
        let request = self.next_permission_request();
        spawn_local(async move {
            let mut result = client
                .set_session_permission(SessionPermissionSetParams {
                    session_id,
                    action,
                    runtime,
                    resource,
                    effect,
                })
                .await;
            if let (Ok(_), Some(old)) = (&result, replacing) {
                result = client
                    .clear_session_permission(SessionPermissionClearParams {
                        session_id,
                        action: old.action,
                        runtime: old.runtime,
                        resource: old.resource,
                    })
                    .await;
            }
            let result = result
                .map(|result| result.permissions)
                .map_err(|error| crate::errors::describe_full(&error));
            self.install_permissions(request, result);
        });
    }

    /// Removes the session override (or draft rule) `rule`.
    pub fn clear_permission(self, rule: RuleRef) {
        if self.new_session.get_untracked() {
            self.draft_rules.update(|rules| {
                remove_draft_rule(rules, &rule.action, rule.runtime.as_ref(), &rule.resource);
            });
            self.load_permissions();
            return;
        }
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        let request = self.next_permission_request();
        spawn_local(async move {
            let result = client
                .clear_session_permission(SessionPermissionClearParams {
                    session_id,
                    action: rule.action,
                    runtime: rule.runtime,
                    resource: rule.resource,
                })
                .await
                .map(|result| result.permissions)
                .map_err(|error| crate::errors::describe_full(&error));
            self.install_permissions(request, result);
        });
    }
}

/// One rule of the table, by key and pattern: `bash@labpc` on `git *`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuleRef {
    pub action: PermissionAction,
    pub runtime: Option<RuntimeId>,
    pub resource: WildcardPattern,
}

impl RuleRef {
    /// The rule a table row shows; none for a pattern that is not one.
    pub fn of(row: &PermissionRow) -> Option<Self> {
        Some(Self {
            action: row.action.clone(),
            runtime: row.runtime.clone(),
            resource: WildcardPattern::new(row.resource.as_str()).ok()?,
        })
    }
}
