use cookie_agent_protocol::{
    ApprovalListResult, ApprovalRespondParams, ApprovalRespondResult, ApprovalStatus, EventOrigin,
    SessionId,
};

use super::{
    Engine, EngineError, PreparedApprovalInvalidation, SessionCommand,
    approval_projection::{approval_records, permission_overlay_epoch},
    helpers::root_id,
};
use crate::tool_api::ToolError;

impl Engine {
    pub async fn approval_respond(
        &self,
        params: ApprovalRespondParams,
        origin: EventOrigin,
    ) -> Result<ApprovalRespondResult, EngineError> {
        let _permission_guard = self.inner.permission_overlay_mutation.lock().await;
        let pending = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(params.session_id, params.approval_id))
            .map(|pending| (pending.executor.clone(), pending.permission_overlay_epoch));
        let Some((executor, pending_epoch)) = pending else {
            return self
                .request(params.session_id, |reply| SessionCommand::ApprovalRespond {
                    params,
                    origin,
                    reply,
                })
                .await;
        };

        let guard = executor.lock_owned().await;
        let mut invalidation = match guard.as_ref() {
            Some(executor) => executor.revalidate().await.err().map(|error| match error {
                ToolError::OperationChanged(_) => PreparedApprovalInvalidation::OperationChanged,
                _ => PreparedApprovalInvalidation::PreparedCapabilityLost,
            }),
            None => Some(PreparedApprovalInvalidation::PreparedCapabilityLost),
        };
        if invalidation.is_none()
            && pending_epoch
                != permission_overlay_epoch(&self.inner.store.get(params.session_id)?.log.events())
        {
            invalidation = Some(PreparedApprovalInvalidation::OperationChanged);
        }
        if let Some(invalidation) = invalidation {
            return self
                .request(params.session_id, |reply| {
                    SessionCommand::ApprovalCapabilityInvalid {
                        params,
                        invalidation,
                        reply,
                    }
                })
                .await;
        }
        self.request(params.session_id, |reply| SessionCommand::ApprovalRespond {
            params,
            origin,
            reply,
        })
        .await
    }

    #[must_use]
    pub fn list_approvals(
        &self,
        root_session_id: SessionId,
        status: Option<ApprovalStatus>,
    ) -> ApprovalListResult {
        let approvals = self
            .inner
            .store
            .all()
            .into_iter()
            .filter(|session| {
                root_id(&session.meta.origin, session.meta.session_id) == root_session_id
            })
            .flat_map(|session| {
                approval_records(session.meta.session_id, &session.log.events()).into_values()
            })
            .filter(|record| status.is_none_or(|status| record.status == status))
            .collect();
        ApprovalListResult {
            approvals,
            tree_grants: self.inner.approvals.for_root(root_session_id),
        }
    }
}
